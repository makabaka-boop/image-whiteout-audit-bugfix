//! layer-merge: merge OCI-style image layers into a single file-system view.
//!
//! Each layer contributes file writes, directory creations, whiteout paths
//! and opaque directories. Layers are applied bottom-up; within one layer the
//! order is fixed and order-independent in effect:
//!
//! 1. whiteouts and opaque markers are resolved against the *lower* snapshot
//!    (whiteouts delete paths, opaque markers hide every child of the marked
//!    directory),
//! 2. then this layer's own writes/mkdirs are applied (parent directories are
//!    created implicitly, so the order of individual writes does not matter).
//!
//! A path deleted and later recreated under the same name is a brand new
//! inode: it reports the recreating layer as its source.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const MAX_LAYERS: usize = 30;
pub const MAX_RECORDS: usize = 3000;
pub const MAX_PATH_LEN: usize = 512;

pub const USAGE: &str = "\
layer-merge — merge OCI-style image layers (whiteouts & opaque directories)

USAGE:
    layer-merge [--pretty] < input.json
    layer-merge --help

Reads a JSON document from stdin describing 1..=30 layers (at most 3000
path records in total) and writes the merged result as JSON to stdout.

INPUT:
    {\"layers\": [{\"writes\": [...], \"mkdirs\": [...],
                  \"whiteouts\": [...], \"opaques\": [...]}, ...]}

EXIT CODES:
    0  merge succeeded
    1  the layers are semantically invalid (conflicts, bad paths, ...)
    2  the input is not well-formed JSON / violates the schema or limits
";

// ---------------------------------------------------------------------------
// Input model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerInput {
    /// Files written by this layer.
    #[serde(default)]
    pub writes: Vec<String>,
    /// Directories explicitly created by this layer.
    #[serde(default)]
    pub mkdirs: Vec<String>,
    /// Whiteout paths: files/dirs of lower layers deleted by this layer.
    #[serde(default)]
    pub whiteouts: Vec<String>,
    /// Opaque directories: every lower-layer child of these dirs is hidden.
    #[serde(default)]
    pub opaques: Vec<String>,
}

impl LayerInput {
    pub fn records(&self) -> usize {
        self.writes.len() + self.mkdirs.len() + self.whiteouts.len() + self.opaques.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub layers: Vec<LayerInput>,
}

// ---------------------------------------------------------------------------
// Output model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeNode {
    pub kind: Kind,
    pub path: String,
    /// Layer that created this node; `null` for the synthetic root.
    pub source_layer: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileSource {
    pub path: String,
    pub source_layer: usize,
}

/// Why a path that once existed no longer shows up (with its old source).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// A file was rewritten by a later layer at the same path.
    Overwritten,
    /// The path itself was named by a whiteout.
    Whiteout,
    /// An ancestor directory was named by a whiteout (`via` = whiteout path).
    WhiteoutAncestor,
    /// An ancestor directory was marked opaque (`via` = opaque directory).
    OpaqueAncestor,
    /// A directory's path was taken over by a file write.
    ReplacedByFile,
    /// A file's path was taken over by a directory creation.
    ReplacedByDir,
    /// An ancestor directory was replaced by a file (`via` = replaced path).
    ReplacedAncestor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Obscured {
    pub path: String,
    /// Layer that created the now-hidden version of `path`.
    pub source_layer: usize,
    pub reason: Reason,
    /// Layer whose action hid the path.
    pub by_layer: usize,
    /// The whiteout path / opaque dir / replaced dir responsible, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub layers: usize,
    pub files: usize,
    pub dirs: usize,
    pub obscured: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Output {
    pub tree: TreeNode,
    pub files: Vec<FileSource>,
    pub obscured: Vec<Obscured>,
    pub stats: Stats,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidJson,
    InvalidInput,
    InvalidPath,
    LayerConflict,
    InvalidOpaque,
    FileAsParent,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeError {
    pub code: ErrorCode,
    pub message: String,
    pub layer: Option<usize>,
    pub path: Option<String>,
}

impl MergeError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        MergeError { code, message: message.into(), layer: None, path: None }
    }

    pub fn at(mut self, layer: usize, path: impl Into<String>) -> Self {
        self.layer = Some(layer);
        self.path = Some(path.into());
        self
    }

    /// Process exit code: 2 for malformed input, 1 for semantic rejection.
    pub fn exit_code(&self) -> u8 {
        match self.code {
            ErrorCode::InvalidJson | ErrorCode::InvalidInput => 2,
            _ => 1,
        }
    }
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.layer, &self.path) {
            (Some(l), Some(p)) => write!(f, "layer {l}, path '{p}': {}", self.message),
            (Some(l), None) => write!(f, "layer {l}: {}", self.message),
            _ => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for MergeError {}

// ---------------------------------------------------------------------------
// JSON envelopes
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OkEnvelope<'a> {
    ok: bool,
    #[serde(flatten)]
    output: &'a Output,
}

#[derive(Serialize)]
struct ErrEnvelope<'a> {
    ok: bool,
    error: ErrBody<'a>,
}

#[derive(Serialize)]
struct ErrBody<'a> {
    code: ErrorCode,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    layer: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
}

pub fn ok_json(output: &Output, pretty: bool) -> String {
    let env = OkEnvelope { ok: true, output };
    if pretty {
        serde_json::to_string_pretty(&env).expect("serialization cannot fail")
    } else {
        serde_json::to_string(&env).expect("serialization cannot fail")
    }
}

pub fn err_json(err: &MergeError, pretty: bool) -> String {
    let env = ErrEnvelope {
        ok: false,
        error: ErrBody {
            code: err.code,
            message: &err.message,
            layer: err.layer,
            path: err.path.as_deref(),
        },
    };
    if pretty {
        serde_json::to_string_pretty(&env).expect("serialization cannot fail")
    } else {
        serde_json::to_string(&env).expect("serialization cannot fail")
    }
}

// ---------------------------------------------------------------------------
// Parsing & validation
// ---------------------------------------------------------------------------

/// Parse and statically validate the input document.
pub fn parse_input(text: &str) -> Result<Input, MergeError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| MergeError::new(ErrorCode::InvalidJson, format!("invalid JSON: {e}")))?;
    let input: Input = serde_json::from_value(value)
        .map_err(|e| MergeError::new(ErrorCode::InvalidInput, format!("invalid input: {e}")))?;

    if input.layers.is_empty() || input.layers.len() > MAX_LAYERS {
        return Err(MergeError::new(
            ErrorCode::InvalidInput,
            format!("expected 1..={MAX_LAYERS} layers, got {}", input.layers.len()),
        ));
    }
    let total: usize = input.layers.iter().map(LayerInput::records).sum();
    if total > MAX_RECORDS {
        return Err(MergeError::new(
            ErrorCode::InvalidInput,
            format!("expected at most {MAX_RECORDS} path records in total, got {total}"),
        ));
    }
    for (idx, layer) in input.layers.iter().enumerate() {
        for path in layer
            .writes
            .iter()
            .chain(&layer.mkdirs)
            .chain(&layer.whiteouts)
            .chain(&layer.opaques)
        {
            validate_path(path).map_err(|e| e.at(idx, path.clone()))?;
        }
    }
    Ok(input)
}

/// A canonical relative ASCII path: non-empty printable-ASCII segments joined
/// by single `/`, no `.`/`..` segments, no leading/trailing/double slashes.
fn validate_path(path: &str) -> Result<(), MergeError> {
    let bad = |msg: &str| MergeError::new(ErrorCode::InvalidPath, msg.to_string());
    if path.is_empty() {
        return Err(bad("path is empty"));
    }
    if path.len() > MAX_PATH_LEN {
        return Err(bad(&format!("path exceeds {MAX_PATH_LEN} bytes")));
    }
    if !path.is_ascii() {
        return Err(bad("path must be ASCII"));
    }
    if path.starts_with('/') {
        return Err(bad("path must be relative, not absolute"));
    }
    for seg in path.split('/') {
        if seg.is_empty() {
            return Err(bad("path contains an empty segment (leading/trailing/double slash)"));
        }
        if seg == "." || seg == ".." {
            return Err(bad("path contains a '.' or '..' segment"));
        }
        if !seg.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
            return Err(bad("path contains non-printable characters"));
        }
    }
    Ok(())
}

/// Proper ancestor prefixes of a path, shortest first: "a/b/c" -> ["a", "a/b"].
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(i, _)| &path[..i])
}

// ---------------------------------------------------------------------------
// Merge engine (flat path -> entry map)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Entry {
    kind: Kind,
    source: usize,
}

/// Parse, validate and merge; the whole CLI pipeline in one call.
pub fn run(text: &str) -> Result<Output, MergeError> {
    let input = parse_input(text)?;
    merge(&input)
}

/// Merge an already validated input into the final image view.
pub fn merge(input: &Input) -> Result<Output, MergeError> {
    let mut engine = Engine::default();
    for (idx, layer) in input.layers.iter().enumerate() {
        engine.apply_layer(idx, layer)?;
    }
    Ok(engine.finish(input.layers.len()))
}

#[derive(Default)]
struct Engine {
    /// Every live path except the implicit root. Invariant: the parent of
    /// every entry exists and is a directory.
    map: BTreeMap<String, Entry>,
    events: Vec<Obscured>,
}

impl Engine {
    fn apply_layer(&mut self, idx: usize, layer: &LayerInput) -> Result<(), MergeError> {
        check_layer_conflicts(idx, layer)?;
        self.check_opaques(idx, layer)?;
        self.check_parents(idx, layer)?;

        // 1. Whiteouts and opaque markers act on the lower snapshot.
        for w in sorted(&layer.whiteouts) {
            self.apply_whiteout(idx, w);
        }
        for o in sorted(&layer.opaques) {
            self.apply_opaque(idx, o);
        }
        // 2. Then this layer's own content is written.
        for m in sorted(&layer.mkdirs) {
            self.ensure_dir(idx, m)?;
        }
        for w in sorted(&layer.writes) {
            self.write_file(idx, w)?;
        }
        Ok(())
    }

    /// Opaque markers may only name a directory that exists in the lower
    /// snapshot or is created (explicitly or implicitly) by this layer.
    fn check_opaques(&self, idx: usize, layer: &LayerInput) -> Result<(), MergeError> {
        let layer_dirs = layer_dir_set(layer);
        for o in sorted(&layer.opaques) {
            let ok = match self.map.get(o) {
                Some(e) => e.kind == Kind::Dir,
                None => layer_dirs.contains(o),
            };
            if !ok {
                let msg = match self.map.get(o) {
                    Some(_) => format!("opaque path '{o}' is a file, not a directory"),
                    None => format!(
                        "opaque directory '{o}' exists neither in lower layers nor in this layer"
                    ),
                };
                return Err(MergeError::new(ErrorCode::InvalidOpaque, msg).at(idx, o));
            }
        }
        Ok(())
    }

    /// No write/mkdir may pass through a file: every strict ancestor that is
    /// a file in the lower snapshot must be deleted by this layer's whiteouts
    /// or replaced by an explicit mkdir of this layer.
    fn check_parents(&self, idx: usize, layer: &LayerInput) -> Result<(), MergeError> {
        let mkdir_set: BTreeSet<&str> = layer.mkdirs.iter().map(String::as_str).collect();
        let targets: BTreeSet<&str> = layer
            .writes
            .iter()
            .chain(&layer.mkdirs)
            .map(String::as_str)
            .collect();
        for p in targets {
            for anc in ancestors(p) {
                let is_file = matches!(self.map.get(anc), Some(e) if e.kind == Kind::File);
                if is_file && !mkdir_set.contains(anc) && !removed_by_whiteouts(anc, &layer.whiteouts)
                {
                    return Err(MergeError::new(
                        ErrorCode::FileAsParent,
                        format!("file '{anc}' cannot serve as a parent directory of '{p}'"),
                    )
                    .at(idx, p));
                }
            }
        }
        Ok(())
    }

    fn apply_whiteout(&mut self, by: usize, path: &str) {
        let Some(entry) = self.map.get(path).copied() else {
            return; // dangling whiteout: nothing in the lower snapshot, no-op
        };
        self.events.push(Obscured {
            path: path.to_string(),
            source_layer: entry.source,
            reason: Reason::Whiteout,
            by_layer: by,
            via: None,
        });
        if entry.kind == Kind::Dir {
            for (d, e) in descendants_of(&self.map, path) {
                self.events.push(Obscured {
                    path: d,
                    source_layer: e.source,
                    reason: Reason::WhiteoutAncestor,
                    by_layer: by,
                    via: Some(path.to_string()),
                });
            }
        }
        remove_descendants(&mut self.map, path);
        self.map.remove(path);
    }

    fn apply_opaque(&mut self, by: usize, path: &str) {
        if !matches!(self.map.get(path), Some(e) if e.kind == Kind::Dir) {
            return; // dir not present in the lower snapshot: nothing to hide
        }
        for (d, e) in descendants_of(&self.map, path) {
            self.events.push(Obscured {
                path: d,
                source_layer: e.source,
                reason: Reason::OpaqueAncestor,
                by_layer: by,
                via: Some(path.to_string()),
            });
        }
        remove_descendants(&mut self.map, path);
    }

    /// Create `path` (and any missing ancestors) as directories. An explicit
    /// mkdir may replace a file at the exact same path.
    fn ensure_dir(&mut self, idx: usize, path: &str) -> Result<(), MergeError> {
        for anc in ancestors(path) {
            self.ensure_dir_one(idx, anc, false)?;
        }
        self.ensure_dir_one(idx, path, true)
    }

    fn ensure_dir_one(&mut self, idx: usize, path: &str, is_target: bool) -> Result<(), MergeError> {
        match self.map.get(path) {
            None => {
                self.map.insert(path.to_string(), Entry { kind: Kind::Dir, source: idx });
            }
            Some(e) if e.kind == Kind::Dir => {} // merged with the lower dir, source kept
            Some(e) if is_target => {
                self.events.push(Obscured {
                    path: path.to_string(),
                    source_layer: e.source,
                    reason: Reason::ReplacedByDir,
                    by_layer: idx,
                    via: None,
                });
                self.map.insert(path.to_string(), Entry { kind: Kind::Dir, source: idx });
            }
            Some(_) => {
                return Err(MergeError::new(
                    ErrorCode::Internal,
                    format!("file '{path}' survived as an ancestor directory"),
                ));
            }
        }
        Ok(())
    }

    fn write_file(&mut self, idx: usize, path: &str) -> Result<(), MergeError> {
        for anc in ancestors(path) {
            self.ensure_dir_one(idx, anc, false)?;
        }
        match self.map.get(path).copied() {
            None => {
                self.map.insert(path.to_string(), Entry { kind: Kind::File, source: idx });
            }
            Some(e) if e.kind == Kind::File => {
                self.events.push(Obscured {
                    path: path.to_string(),
                    source_layer: e.source,
                    reason: Reason::Overwritten,
                    by_layer: idx,
                    via: None,
                });
                self.map.insert(path.to_string(), Entry { kind: Kind::File, source: idx });
            }
            Some(e) => {
                self.events.push(Obscured {
                    path: path.to_string(),
                    source_layer: e.source,
                    reason: Reason::ReplacedByFile,
                    by_layer: idx,
                    via: None,
                });
                for (d, de) in descendants_of(&self.map, path) {
                    self.events.push(Obscured {
                        path: d,
                        source_layer: de.source,
                        reason: Reason::ReplacedAncestor,
                        by_layer: idx,
                        via: Some(path.to_string()),
                    });
                }
                remove_descendants(&mut self.map, path);
                self.map.insert(path.to_string(), Entry { kind: Kind::File, source: idx });
            }
        }
        Ok(())
    }

    fn finish(mut self, layers: usize) -> Output {
        self.events
            .sort_by(|a, b| (a.by_layer, a.path.as_str()).cmp(&(b.by_layer, b.path.as_str())));
        let files: Vec<FileSource> = self
            .map
            .iter()
            .filter(|(_, e)| e.kind == Kind::File)
            .map(|(p, e)| FileSource { path: p.clone(), source_layer: e.source })
            .collect();
        let dirs = self.map.values().filter(|e| e.kind == Kind::Dir).count();
        let obscured = self.events.len();
        Output {
            tree: build_tree(&self.map),
            obscured: self.events,
            stats: Stats { layers, files: files.len(), dirs, obscured },
            files,
        }
    }
}

/// Same-layer structural conflicts; the whole input is rejected on any hit.
fn check_layer_conflicts(idx: usize, layer: &LayerInput) -> Result<(), MergeError> {
    for (name, list) in [
        ("writes", &layer.writes),
        ("mkdirs", &layer.mkdirs),
        ("whiteouts", &layer.whiteouts),
        ("opaques", &layer.opaques),
    ] {
        let mut seen = BTreeSet::new();
        for p in list {
            if !seen.insert(p) {
                return Err(MergeError::new(
                    ErrorCode::LayerConflict,
                    format!("duplicate path '{p}' in {name}"),
                )
                .at(idx, p));
            }
        }
    }

    // A path cannot be both a file and a directory in one layer. The
    // directory set includes implicit parents, so a file that would have to
    // act as another entry's parent is caught here as well.
    let files: BTreeSet<&str> = layer.writes.iter().map(String::as_str).collect();
    let dirs = layer_dir_set(layer);
    if let Some(p) = files.intersection(&dirs).next() {
        return Err(MergeError::new(
            ErrorCode::LayerConflict,
            format!("path '{p}' is both a file (writes) and a directory"),
        )
        .at(idx, *p));
    }

    let whiteouts: BTreeSet<&str> = layer.whiteouts.iter().map(String::as_str).collect();
    if let Some(p) = layer.opaques.iter().find(|p| whiteouts.contains(p.as_str())) {
        return Err(MergeError::new(
            ErrorCode::LayerConflict,
            format!("path '{p}' is both a whiteout and an opaque directory"),
        )
        .at(idx, p));
    }
    Ok(())
}

/// Directories this layer creates: explicit mkdirs plus implicit parents of
/// every write/mkdir.
fn layer_dir_set(layer: &LayerInput) -> BTreeSet<&str> {
    let mut dirs: BTreeSet<&str> = layer.mkdirs.iter().map(String::as_str).collect();
    for p in layer.writes.iter().chain(&layer.mkdirs) {
        dirs.extend(ancestors(p));
    }
    dirs
}

/// Is `path` itself deleted by one of these whiteouts (directly or because an
/// ancestor directory is whiteout'ed)?
fn removed_by_whiteouts(path: &str, whiteouts: &[String]) -> bool {
    whiteouts
        .iter()
        .any(|w| w == path || path.strip_prefix(w.as_str()).is_some_and(|rest| rest.starts_with('/')))
}

fn sorted(list: &[String]) -> Vec<&str> {
    let mut v: Vec<&str> = list.iter().map(String::as_str).collect();
    v.sort_unstable();
    v
}

fn descendants_of(map: &BTreeMap<String, Entry>, path: &str) -> Vec<(String, Entry)> {
    let prefix = format!("{path}/");
    map.range(prefix.clone()..)
        .take_while(|(k, _)| k.starts_with(&prefix))
        .map(|(k, v)| (k.clone(), *v))
        .collect()
}

fn remove_descendants(map: &mut BTreeMap<String, Entry>, path: &str) {
    let keys: Vec<String> = descendants_of(map, path).into_iter().map(|(k, _)| k).collect();
    for k in keys {
        map.remove(&k);
    }
}

fn build_tree(map: &BTreeMap<String, Entry>) -> TreeNode {
    let mut root = TreeNode {
        kind: Kind::Dir,
        path: String::new(),
        source_layer: None,
        children: Vec::new(),
    };
    // `map` is ordered, so parents are always visited before their children
    // and children of one directory arrive contiguously, already sorted.
    for (path, entry) in map {
        let mut node = &mut root;
        let mut rest = path.as_str();
        while let Some((head, tail)) = rest.split_once('/') {
            let prefix_len = path.len() - rest.len() + head.len();
            let pos = node
                .children
                .iter()
                .position(|c| c.kind == Kind::Dir && c.path.len() == prefix_len && c.path[..] == path[..prefix_len])
                .expect("parent directory is inserted before its children");
            node = &mut node.children[pos];
            rest = tail;
        }
        node.children.push(TreeNode {
            kind: entry.kind,
            path: path.clone(),
            source_layer: Some(entry.source),
            children: Vec::new(),
        });
    }
    root
}
