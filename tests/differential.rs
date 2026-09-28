//! Differential tests (对拍): the flat-map merge engine in `src/lib.rs` is
//! pitted against an independent, deliberately naive oracle that applies one
//! layer at a time to an explicit nested directory tree. Both must agree on
//! acceptance/rejection (and the error code) or on the exact final tree, the
//! per-file source layers and the obscured-path log.
//!
//! Two generators are used:
//!   * `gen_chaos`  — random junk, mostly invalid; checks error-code agreement,
//!   * `gen_guided` — builds layers against the current merged state, so most
//!     scenarios are valid and rich in overwrites, whiteouts, opaque markers
//!     and file/dir type replacements.

use layer_merge::*;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Naive explicit tree model (the oracle)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Node {
    File { src: usize },
    Dir { src: Option<usize>, children: BTreeMap<String, Node> },
}

impl Node {
    fn src(&self) -> Option<usize> {
        match self {
            Node::File { src } => Some(*src),
            Node::Dir { src, .. } => *src,
        }
    }
}

#[derive(Default)]
struct Model {
    root: BTreeMap<String, Node>,
    events: Vec<Obscured>,
}

fn ancestors_of(p: &str) -> Vec<&str> {
    p.match_indices('/').map(|(i, _)| &p[..i]).collect()
}

fn sorted(list: &[String]) -> Vec<&str> {
    let mut v: Vec<&str> = list.iter().map(String::as_str).collect();
    v.sort_unstable();
    v
}

fn split_parent(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('/') {
        Some((p, n)) => (Some(p), n),
        None => (None, path),
    }
}

fn whiteout_removes(whiteouts: &[String], path: &str) -> bool {
    whiteouts.iter().any(|w| w == path || path.starts_with(&format!("{w}/")))
}

/// All descendants of a directory node, pre-order (== lexicographic here).
fn collect_subtree(node: &Node, base: &str, out: &mut Vec<(String, usize)>) {
    if let Node::Dir { children, .. } = node {
        for (name, child) in children {
            let p = format!("{base}/{name}");
            out.push((p.clone(), child.src().expect("non-root node has a source")));
            collect_subtree(child, &p, out);
        }
    }
}

impl Model {
    fn get(&self, path: &str) -> Option<&Node> {
        let mut children = &self.root;
        let segs: Vec<&str> = path.split('/').collect();
        for (i, seg) in segs.iter().enumerate() {
            let node = children.get(*seg)?;
            if i == segs.len() - 1 {
                return Some(node);
            }
            match node {
                Node::Dir { children: c, .. } => children = c,
                Node::File { .. } => return None,
            }
        }
        None
    }

    fn get_mut(&mut self, path: &str) -> Option<&mut Node> {
        let mut children = &mut self.root;
        let segs: Vec<&str> = path.split('/').collect();
        for (i, seg) in segs.iter().enumerate() {
            let node = children.get_mut(*seg)?;
            if i == segs.len() - 1 {
                return Some(node);
            }
            match node {
                Node::Dir { children: c, .. } => children = c,
                Node::File { .. } => return None,
            }
        }
        None
    }

    fn apply_layer(&mut self, idx: usize, layer: &LayerInput) -> Result<(), MergeError> {
        // -- structural conflicts, re-derived naively from the spec --
        for list in [&layer.writes, &layer.mkdirs, &layer.whiteouts, &layer.opaques] {
            let mut seen = BTreeSet::new();
            for p in list {
                if !seen.insert(p) {
                    return Err(MergeError::new(ErrorCode::LayerConflict, "duplicate path"));
                }
            }
        }
        let files: BTreeSet<&str> = layer.writes.iter().map(String::as_str).collect();
        let mut dirs: BTreeSet<&str> = layer.mkdirs.iter().map(String::as_str).collect();
        for p in layer.writes.iter().chain(&layer.mkdirs) {
            dirs.extend(ancestors_of(p));
        }
        if files.intersection(&dirs).next().is_some() {
            return Err(MergeError::new(ErrorCode::LayerConflict, "file/dir clash"));
        }
        let xs: BTreeSet<&str> = layer.whiteouts.iter().map(String::as_str).collect();
        if layer.opaques.iter().any(|o| xs.contains(o.as_str())) {
            return Err(MergeError::new(ErrorCode::LayerConflict, "whiteout/opaque clash"));
        }

        // -- opaque markers must name an existing directory --
        for o in sorted(&layer.opaques) {
            let ok = match self.get(o) {
                Some(Node::Dir { .. }) => true,
                Some(Node::File { .. }) => false,
                None => dirs.contains(o),
            };
            if !ok {
                return Err(MergeError::new(ErrorCode::InvalidOpaque, "not a directory"));
            }
        }

        // -- files cannot serve as parent directories --
        let mkdir_set: BTreeSet<&str> = layer.mkdirs.iter().map(String::as_str).collect();
        let mut targets: Vec<&str> =
            layer.writes.iter().chain(&layer.mkdirs).map(String::as_str).collect();
        targets.sort_unstable();
        targets.dedup();
        for p in targets {
            for anc in ancestors_of(p) {
                if matches!(self.get(anc), Some(Node::File { .. }))
                    && !mkdir_set.contains(anc)
                    && !whiteout_removes(&layer.whiteouts, anc)
                {
                    return Err(MergeError::new(ErrorCode::FileAsParent, "file as parent"));
                }
            }
        }

        // -- apply: whiteouts & opaques on the lower snapshot, then writes --
        for w in sorted(&layer.whiteouts) {
            self.apply_whiteout(idx, w);
        }
        for o in sorted(&layer.opaques) {
            self.apply_opaque(idx, o);
        }
        for m in sorted(&layer.mkdirs) {
            self.mkdir(idx, m)?;
        }
        for w in sorted(&layer.writes) {
            self.write(idx, w)?;
        }
        Ok(())
    }

    fn apply_whiteout(&mut self, idx: usize, path: &str) {
        let Some(node) = self.get(path) else { return }; // dangling whiteout: no-op
        let src = node.src().expect("non-root node has a source");
        let mut subs = Vec::new();
        collect_subtree(node, path, &mut subs);
        self.events.push(Obscured {
            path: path.to_string(),
            source_layer: src,
            reason: Reason::Whiteout,
            by_layer: idx,
            via: None,
        });
        for (p, s) in subs {
            self.events.push(Obscured {
                path: p,
                source_layer: s,
                reason: Reason::WhiteoutAncestor,
                by_layer: idx,
                via: Some(path.to_string()),
            });
        }
        let (parent, name) = split_parent(path);
        let children = match parent {
            None => &mut self.root,
            Some(pp) => match self.get_mut(pp) {
                Some(Node::Dir { children, .. }) => children,
                _ => unreachable!("parent of an existing node is a directory"),
            },
        };
        children.remove(name);
    }

    fn apply_opaque(&mut self, idx: usize, path: &str) {
        let Some(node) = self.get(path) else { return };
        if !matches!(node, Node::Dir { .. }) {
            return;
        }
        let mut subs = Vec::new();
        collect_subtree(node, path, &mut subs);
        for (p, s) in subs {
            self.events.push(Obscured {
                path: p,
                source_layer: s,
                reason: Reason::OpaqueAncestor,
                by_layer: idx,
                via: Some(path.to_string()),
            });
        }
        if let Some(Node::Dir { children, .. }) = self.get_mut(path) {
            children.clear();
        }
    }

    fn mkdir(&mut self, idx: usize, path: &str) -> Result<(), MergeError> {
        let segs: Vec<&str> = path.split('/').collect();
        let mut children = &mut self.root;
        let mut prefix = String::new();
        for (i, seg) in segs.iter().enumerate() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            let is_target = i == segs.len() - 1;
            let node = children
                .entry(seg.to_string())
                .or_insert_with(|| Node::Dir { src: Some(idx), children: BTreeMap::new() });
            if let Node::File { src } = node {
                if is_target {
                    let old = *src;
                    self.events.push(Obscured {
                        path: prefix.clone(),
                        source_layer: old,
                        reason: Reason::ReplacedByDir,
                        by_layer: idx,
                        via: None,
                    });
                    *node = Node::Dir { src: Some(idx), children: BTreeMap::new() };
                } else {
                    return Err(MergeError::new(
                        ErrorCode::Internal,
                        "file ancestor survived pre-checks",
                    ));
                }
            }
            match node {
                Node::Dir { children: c, .. } => children = c,
                Node::File { .. } => unreachable!("file was replaced above"),
            }
        }
        Ok(())
    }

    fn write(&mut self, idx: usize, path: &str) -> Result<(), MergeError> {
        let segs: Vec<&str> = path.split('/').collect();
        let mut children = &mut self.root;
        for seg in &segs[..segs.len() - 1] {
            let node = children
                .entry(seg.to_string())
                .or_insert_with(|| Node::Dir { src: Some(idx), children: BTreeMap::new() });
            match node {
                Node::Dir { children: c, .. } => children = c,
                Node::File { .. } => {
                    return Err(MergeError::new(
                        ErrorCode::Internal,
                        "file ancestor survived pre-checks",
                    ));
                }
            }
        }
        let name = segs.last().expect("non-empty path").to_string();
        enum Act {
            Create,
            Overwrite(usize),
            ReplaceDir(usize),
        }
        let act = match children.get(&name) {
            None => Act::Create,
            Some(Node::File { src }) => Act::Overwrite(*src),
            Some(Node::Dir { src, .. }) => Act::ReplaceDir(src.expect("non-root dir has a source")),
        };
        match act {
            Act::Create => {
                children.insert(name, Node::File { src: idx });
            }
            Act::Overwrite(old) => {
                self.events.push(Obscured {
                    path: path.to_string(),
                    source_layer: old,
                    reason: Reason::Overwritten,
                    by_layer: idx,
                    via: None,
                });
                children.insert(name, Node::File { src: idx });
            }
            Act::ReplaceDir(old) => {
                self.events.push(Obscured {
                    path: path.to_string(),
                    source_layer: old,
                    reason: Reason::ReplacedByFile,
                    by_layer: idx,
                    via: None,
                });
                let mut subs = Vec::new();
                collect_subtree(children.get(&name).expect("dir exists"), path, &mut subs);
                for (p, s) in subs {
                    self.events.push(Obscured {
                        path: p,
                        source_layer: s,
                        reason: Reason::ReplacedAncestor,
                        by_layer: idx,
                        via: Some(path.to_string()),
                    });
                }
                children.insert(name, Node::File { src: idx });
            }
        }
        Ok(())
    }

    fn finish(mut self) -> (BTreeMap<String, (Kind, usize)>, Vec<Obscured>) {
        self.events
            .sort_by(|a, b| (a.by_layer, a.path.as_str()).cmp(&(b.by_layer, b.path.as_str())));
        let mut flat = BTreeMap::new();
        fn walk(
            children: &BTreeMap<String, Node>,
            prefix: &str,
            out: &mut BTreeMap<String, (Kind, usize)>,
        ) {
            for (name, node) in children {
                let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                let kind = match node {
                    Node::File { .. } => Kind::File,
                    Node::Dir { .. } => Kind::Dir,
                };
                out.insert(path.clone(), (kind, node.src().expect("non-root node has a source")));
                if let Node::Dir { children, .. } = node {
                    walk(children, &path, out);
                }
            }
        }
        walk(&self.root, "", &mut flat);
        (flat, self.events)
    }
}

// ---------------------------------------------------------------------------
// Engine-side helpers
// ---------------------------------------------------------------------------

fn flatten_engine(tree: &TreeNode) -> BTreeMap<String, (Kind, usize)> {
    let mut out = BTreeMap::new();
    fn rec(node: &TreeNode, out: &mut BTreeMap<String, (Kind, usize)>) {
        for c in &node.children {
            out.insert(c.path.clone(), (c.kind, c.source_layer.expect("non-root node has a source")));
            rec(c, out);
        }
    }
    rec(tree, &mut out);
    out
}

/// Flattened image view: path -> (kind, source layer).
type FlatTree = BTreeMap<String, (Kind, usize)>;

fn run_model(input: &Input) -> Result<(FlatTree, Vec<Obscured>), MergeError> {
    let mut model = Model::default();
    for (idx, layer) in input.layers.iter().enumerate() {
        model.apply_layer(idx, layer)?;
    }
    Ok(model.finish())
}

/// Compare engine and model on one input; `context` identifies the case.
fn assert_agree(input: &Input, context: &str) {
    let json = serde_json::to_string(input).expect("input serializes");
    // Round-trip through the real JSON parsing path.
    let parsed = parse_input(&json)
        .unwrap_or_else(|e| panic!("{context}: generated input failed to parse: {e}\n{json}"));
    match (merge(&parsed), run_model(&parsed)) {
        (Ok(out), Ok((flat, events))) => {
            let eng_flat = flatten_engine(&out.tree);
            assert_eq!(eng_flat, flat, "{context}: tree mismatch\n{json}");
            assert_eq!(out.obscured, events, "{context}: obscured log mismatch\n{json}");
            // `files` must be exactly the file entries of the tree, sorted.
            let files_from_tree: Vec<FileSource> = flat
                .iter()
                .filter(|(_, (k, _))| *k == Kind::File)
                .map(|(p, (_, s))| FileSource { path: p.clone(), source_layer: *s })
                .collect();
            assert_eq!(out.files, files_from_tree, "{context}: files list mismatch\n{json}");
            assert_eq!(out.stats.files, out.files.len());
            assert_eq!(out.stats.dirs, flat.len() - out.files.len());
            assert_eq!(out.stats.obscured, out.obscured.len());
            assert_eq!(out.stats.layers, parsed.layers.len());
        }
        (Err(e1), Err(e2)) => {
            assert_eq!(e1.code, e2.code, "{context}: error code mismatch\n{json}");
        }
        (eng, mdl) => panic!("{context}: disagreement\nengine: {eng:?}\nmodel: {mdl:?}\n{json}"),
    }
}

// ---------------------------------------------------------------------------
// Deterministic PRNG + scenario generators
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn gen_path(rng: &mut Rng) -> String {
    const SEGS: [&str; 6] = ["a", "b", "c", "d", "e", "f"];
    let depth = 1 + rng.below(4);
    (0..depth).map(|_| SEGS[rng.below(SEGS.len())]).collect::<Vec<_>>().join("/")
}

/// Random junk: frequently invalid inputs, to check error-code agreement.
fn gen_chaos(rng: &mut Rng) -> Input {
    let layers = (0..1 + rng.below(8))
        .map(|_| {
            // Half of the layers get no opaques, so deeper checks (like
            // file-as-parent) are reached often enough.
            let allow_opaque = rng.below(2) == 0;
            LayerInput {
                writes: (0..rng.below(5)).map(|_| gen_path(rng)).collect(),
                mkdirs: (0..rng.below(4)).map(|_| gen_path(rng)).collect(),
                whiteouts: (0..rng.below(4)).map(|_| gen_path(rng)).collect(),
                opaques: if allow_opaque {
                    (0..rng.below(3)).map(|_| gen_path(rng)).collect()
                } else {
                    Vec::new()
                },
            }
        })
        .collect();
    Input { layers }
}

/// Directories and files of the merged state after `input`'s layers.
fn state_paths(input: &Input) -> (Vec<String>, Vec<String>) {
    if input.layers.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let out = merge(input).expect("guided prefix is valid by construction");
    let mut dirs = Vec::new();
    fn walk(node: &TreeNode, dirs: &mut Vec<String>) {
        for c in &node.children {
            if c.kind == Kind::Dir {
                dirs.push(c.path.clone());
                walk(c, dirs);
            }
        }
    }
    walk(&out.tree, &mut dirs);
    (dirs, out.files.iter().map(|f| f.path.clone()).collect())
}

fn gen_path_under(rng: &mut Rng, dirs: &[String]) -> String {
    if !dirs.is_empty() && rng.below(2) == 0 {
        format!("{}/{}", dirs[rng.below(dirs.len())], gen_path(rng))
    } else {
        gen_path(rng)
    }
}

/// One layer that is very likely to be valid on top of the current state and
/// to trigger interesting merges (overwrites, rebuilds, type replacements).
fn gen_guided_layer(rng: &mut Rng, dirs: &[String], files: &[String]) -> LayerInput {
    let mut layer = LayerInput::default();
    for _ in 0..rng.below(5) {
        match rng.below(10) {
            0..=3 if !files.is_empty() => {
                layer.writes.push(files[rng.below(files.len())].clone()) // overwrite
            }
            4 if !dirs.is_empty() => {
                layer.writes.push(dirs[rng.below(dirs.len())].clone()) // dir -> file
            }
            _ => layer.writes.push(gen_path_under(rng, dirs)),
        }
    }
    for _ in 0..rng.below(3) {
        match rng.below(10) {
            0..=1 if !files.is_empty() => {
                layer.mkdirs.push(files[rng.below(files.len())].clone()) // file -> dir
            }
            2..=3 if !dirs.is_empty() => {
                layer.mkdirs.push(dirs[rng.below(dirs.len())].clone()) // merge existing
            }
            _ => layer.mkdirs.push(gen_path_under(rng, dirs)),
        }
    }
    for _ in 0..rng.below(3) {
        let all: Vec<&String> = files.iter().chain(dirs).collect();
        if !all.is_empty() && rng.below(4) > 0 {
            layer.whiteouts.push(all[rng.below(all.len())].clone());
        } else {
            layer.whiteouts.push(gen_path(rng)); // possibly dangling
        }
    }
    for _ in 0..rng.below(2) {
        if !dirs.is_empty() && rng.below(5) > 0 {
            layer.opaques.push(dirs[rng.below(dirs.len())].clone());
        } else {
            layer.opaques.push(gen_path(rng));
        }
    }
    // Sometimes rebuild a just-whiteout'ed path in the same layer.
    if !layer.whiteouts.is_empty() && rng.below(3) == 0 {
        let w = layer.whiteouts[rng.below(layer.whiteouts.len())].clone();
        if rng.below(2) == 0 {
            layer.writes.push(w);
        } else {
            layer.mkdirs.push(w);
        }
    }
    layer
}

/// Mostly-valid scenarios with rich obscuring activity.
fn gen_guided(rng: &mut Rng) -> Input {
    let mut input = Input { layers: Vec::new() };
    for _ in 0..1 + rng.below(10) {
        let (dirs, files) = state_paths(&input);
        let mut trial = input.clone();
        trial.layers.push(gen_guided_layer(rng, &dirs, &files));
        if merge(&trial).is_ok() {
            input = trial;
        } else {
            // Fall back to a benign layer of fresh paths (always valid).
            input.layers.push(LayerInput {
                writes: (0..1 + rng.below(3))
                    .map(|_| format!("g{}/f{}", rng.below(1_000_000), rng.below(1_000_000)))
                    .collect(),
                ..Default::default()
            });
        }
    }
    input
}

// ---------------------------------------------------------------------------
// The differential tests
// ---------------------------------------------------------------------------

#[test]
fn engine_matches_model_on_chaos_inputs() {
    let mut err_counts: BTreeMap<ErrorCode, usize> = BTreeMap::new();
    let mut accepted = 0usize;
    for seed in 1..=600u64 {
        let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let input = gen_chaos(&mut rng);
        if merge(&parse_input(&serde_json::to_string(&input).unwrap()).unwrap()).is_ok() {
            accepted += 1;
        } else if let Err(e) = merge(&parse_input(&serde_json::to_string(&input).unwrap()).unwrap())
        {
            *err_counts.entry(e.code).or_default() += 1;
        }
        assert_agree(&input, &format!("chaos seed {seed}"));
    }
    // The generator must keep every rejection class well covered.
    assert!(accepted > 20, "chaos: too few accepted scenarios ({accepted})");
    for code in [ErrorCode::LayerConflict, ErrorCode::InvalidOpaque, ErrorCode::FileAsParent] {
        assert!(
            err_counts.get(&code).copied().unwrap_or(0) > 10,
            "chaos: {code:?} under-covered: {err_counts:?}"
        );
    }
}

#[test]
fn engine_matches_model_on_guided_inputs() {
    let mut reason_counts: BTreeMap<Reason, usize> = BTreeMap::new();
    for seed in 1..=600u64 {
        let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17));
        let input = gen_guided(&mut rng);
        // Tally from the engine output (model agreement is checked next).
        let parsed = parse_input(&serde_json::to_string(&input).unwrap()).unwrap();
        let out = merge(&parsed).expect("guided inputs are valid by construction");
        for o in &out.obscured {
            *reason_counts.entry(o.reason).or_default() += 1;
        }
        assert_agree(&input, &format!("guided seed {seed}"));
    }
    // Every obscuring mechanism must be exercised many times.
    for reason in [
        Reason::Overwritten,
        Reason::Whiteout,
        Reason::WhiteoutAncestor,
        Reason::OpaqueAncestor,
        Reason::ReplacedByFile,
        Reason::ReplacedByDir,
        Reason::ReplacedAncestor,
    ] {
        assert!(
            reason_counts.get(&reason).copied().unwrap_or(0) > 20,
            "guided: {reason:?} under-covered: {reason_counts:?}"
        );
    }
}

/// Write order inside a layer must not change the outcome.
#[test]
fn write_order_within_layer_is_irrelevant() {
    let a = parse_input(
        r#"{"layers":[{"writes":["x","a/b","a/c/deep"],"mkdirs":["m/n"],"whiteouts":["x"]}]}"#,
    )
    .unwrap();
    let b = parse_input(
        r#"{"layers":[{"writes":["a/c/deep","a/b","x"],"mkdirs":["m/n"],"whiteouts":["x"]}]}"#,
    )
    .unwrap();
    assert_eq!(merge(&a), merge(&b));
}
