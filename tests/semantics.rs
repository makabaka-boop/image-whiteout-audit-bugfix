//! Targeted semantic tests with golden expectations, focused on:
//!   * whole-directory obscuring (opaque markers),
//!   * delete-then-recreate under the same name (source layer must move on),
//!   * parent/child type conflicts (files can never be directories' parents).

use layer_merge::*;

fn ok(text: &str) -> Output {
    run(text).unwrap_or_else(|e| panic!("expected success, got {e}\ninput: {text}"))
}

fn err(text: &str) -> MergeError {
    run(text).expect_err(&format!("expected rejection\ninput: {text}"))
}

fn file_src(out: &Output, path: &str) -> Option<usize> {
    out.files.iter().find(|f| f.path == path).map(|f| f.source_layer)
}

fn dir_src(out: &Output, path: &str) -> Option<usize> {
    fn find<'a>(node: &'a TreeNode, path: &str) -> Option<&'a TreeNode> {
        for c in &node.children {
            if c.path == path {
                return Some(c);
            }
            if let Some(hit) = find(c, path) {
                return Some(hit);
            }
        }
        None
    }
    find(&out.tree, path).and_then(|n| n.source_layer)
}

fn obscured<'a>(out: &'a Output, path: &str) -> Vec<&'a Obscured> {
    out.obscured.iter().filter(|o| o.path == path).collect()
}

fn one_obscured<'a>(out: &'a Output, path: &str) -> &'a Obscured {
    let hits = obscured(out, path);
    assert_eq!(hits.len(), 1, "expected exactly one obscured entry for '{path}'");
    hits[0]
}

// ---------------------------------------------------------------------------
// Whole-directory obscuring
// ---------------------------------------------------------------------------

#[test]
fn opaque_hides_all_lower_children_but_keeps_dir() {
    let out = ok(r#"{"layers":[
        {"mkdirs":["d"],"writes":["d/a","d/b","d/sub/x","keep"]},
        {"opaques":["d"]},
        {"writes":["d/new"]}
    ]}"#);
    // The directory itself survives with its original source; every lower
    // child is gone; only the post-opaque write is visible.
    assert_eq!(dir_src(&out, "d"), Some(0));
    assert_eq!(file_src(&out, "keep"), Some(0));
    assert_eq!(file_src(&out, "d/new"), Some(2));
    assert_eq!(out.files.len(), 2);
    for (p, src) in [("d/a", 0), ("d/b", 0), ("d/sub", 0), ("d/sub/x", 0)] {
        let o = one_obscured(&out, p);
        assert_eq!((o.source_layer, o.reason, o.by_layer, o.via.as_deref()),
                   (src, Reason::OpaqueAncestor, 1, Some("d")));
    }
    assert_eq!(out.obscured.len(), 4);
}

#[test]
fn opaque_on_dir_created_in_same_layer_is_valid_noop() {
    let out = ok(r#"{"layers":[
        {"writes":["elsewhere"]},
        {"mkdirs":["fresh"],"opaques":["fresh"],"writes":["fresh/inner"]}
    ]}"#);
    assert_eq!(dir_src(&out, "fresh"), Some(1));
    assert_eq!(file_src(&out, "fresh/inner"), Some(1));
    assert!(out.obscured.is_empty());
}

#[test]
fn opaque_on_implied_parent_of_same_layer_is_valid() {
    // "a/b" only exists as the implicit parent of this layer's write, which
    // is enough for the opaque marker to be legal. The marker on "a" is
    // processed first and hides the whole lower subtree.
    let out = ok(r#"{"layers":[
        {"writes":["a/b/c"]},
        {"opaques":["a","a/b"],"writes":["a/b/d"]}
    ]}"#);
    assert_eq!(file_src(&out, "a/b/d"), Some(1));
    let o = one_obscured(&out, "a/b/c");
    assert_eq!((o.reason, o.by_layer, o.via.as_deref()), (Reason::OpaqueAncestor, 1, Some("a")));
    let o = one_obscured(&out, "a/b");
    assert_eq!((o.reason, o.by_layer, o.via.as_deref()), (Reason::OpaqueAncestor, 1, Some("a")));
}

#[test]
fn opaque_rejects_missing_file_or_non_dir_targets() {
    // Does not exist anywhere.
    let e = err(r#"{"layers":[{"writes":["x"]},{"opaques":["ghost"]}]}"#);
    assert_eq!(e.code, ErrorCode::InvalidOpaque);
    assert_eq!(e.layer, Some(1));
    // Exists, but as a file.
    let e = err(r#"{"layers":[{"writes":["f"]},{"opaques":["f"]}]}"#);
    assert_eq!(e.code, ErrorCode::InvalidOpaque);
    // Parent is a file, so the dir cannot exist.
    let e = err(r#"{"layers":[{"writes":["f"]},{"opaques":["f/g"]}]}"#);
    assert_eq!(e.code, ErrorCode::InvalidOpaque);
}

#[test]
fn whiteout_removes_whole_subtree() {
    let out = ok(r#"{"layers":[
        {"mkdirs":["d"],"writes":["d/a","d/b/c","top"]},
        {"whiteouts":["d"]}
    ]}"#);
    assert_eq!(out.files.len(), 1);
    assert_eq!(file_src(&out, "top"), Some(0));
    let o = one_obscured(&out, "d");
    assert_eq!((o.reason, o.via.as_deref()), (Reason::Whiteout, None));
    for p in ["d/a", "d/b", "d/b/c"] {
        let o = one_obscured(&out, p);
        assert_eq!((o.reason, o.via.as_deref()), (Reason::WhiteoutAncestor, Some("d")));
    }
}

#[test]
fn dangling_whiteout_is_a_noop() {
    let out = ok(r#"{"layers":[{"writes":["x"]},{"whiteouts":["ghost","x/child"]}]}"#);
    assert_eq!(file_src(&out, "x"), Some(0));
    assert!(out.obscured.is_empty());
}

// ---------------------------------------------------------------------------
// Delete-then-recreate under the same name
// ---------------------------------------------------------------------------

#[test]
fn recreated_file_gets_new_source_layer() {
    let out = ok(r#"{"layers":[
        {"writes":["f"]},
        {"whiteouts":["f"],"writes":["f"]},
        {"whiteouts":["f"]},
        {"writes":["f"]}
    ]}"#);
    assert_eq!(file_src(&out, "f"), Some(3));
    let log = obscured(&out, "f");
    assert_eq!(log.len(), 2);
    assert_eq!((log[0].source_layer, log[0].reason, log[0].by_layer), (0, Reason::Whiteout, 1));
    assert_eq!((log[1].source_layer, log[1].reason, log[1].by_layer), (1, Reason::Whiteout, 2));
}

#[test]
fn recreated_dir_gets_new_source_and_fresh_contents() {
    let out = ok(r#"{"layers":[
        {"mkdirs":["a"],"writes":["a/old"]},
        {"whiteouts":["a"]},
        {"mkdirs":["a"],"writes":["a/new"]}
    ]}"#);
    assert_eq!(dir_src(&out, "a"), Some(2));
    assert_eq!(file_src(&out, "a/new"), Some(2));
    assert_eq!(file_src(&out, "a/old"), None);
    let o = one_obscured(&out, "a/old");
    assert_eq!((o.reason, o.via.as_deref()), (Reason::WhiteoutAncestor, Some("a")));
}

#[test]
fn overwritten_file_reports_latest_layer_as_source() {
    let out = ok(r#"{"layers":[{"writes":["f"]},{"writes":["f"]},{"writes":["f"]}]}"#);
    assert_eq!(file_src(&out, "f"), Some(2));
    let log = obscured(&out, "f");
    assert_eq!(log.len(), 2);
    assert_eq!((log[0].source_layer, log[0].reason, log[0].by_layer), (0, Reason::Overwritten, 1));
    assert_eq!((log[1].source_layer, log[1].reason, log[1].by_layer), (1, Reason::Overwritten, 2));
}

#[test]
fn mkdir_over_existing_dir_keeps_original_source() {
    let out = ok(r#"{"layers":[{"mkdirs":["d"]},{"mkdirs":["d"],"writes":["d/f"]}]}"#);
    assert_eq!(dir_src(&out, "d"), Some(0));
    assert_eq!(file_src(&out, "d/f"), Some(1));
    assert!(out.obscured.is_empty());
}

// ---------------------------------------------------------------------------
// Parent/child type conflicts
// ---------------------------------------------------------------------------

#[test]
fn same_layer_file_dir_conflicts_reject_everything() {
    // File that must also be a parent directory (implicitly).
    let e = err(r#"{"layers":[{"writes":["a","a/b"]}]}"#);
    assert_eq!(e.code, ErrorCode::LayerConflict);
    // Same path as file and as explicit directory.
    let e = err(r#"{"layers":[{"writes":["a"],"mkdirs":["a"]}]}"#);
    assert_eq!(e.code, ErrorCode::LayerConflict);
    // File above an explicit mkdir.
    let e = err(r#"{"layers":[{"writes":["a"],"mkdirs":["a/b"]}]}"#);
    assert_eq!(e.code, ErrorCode::LayerConflict);
    // Duplicates within one list.
    let e = err(r#"{"layers":[{"writes":["a","a"]}]}"#);
    assert_eq!(e.code, ErrorCode::LayerConflict);
    let e = err(r#"{"layers":[{"whiteouts":["a","a"]}]}"#);
    assert_eq!(e.code, ErrorCode::LayerConflict);
    // Same path as whiteout and opaque.
    let e = err(r#"{"layers":[{"mkdirs":["a"],"whiteouts":["a"],"opaques":["a"]}]}"#);
    assert_eq!(e.code, ErrorCode::LayerConflict);
}

#[test]
fn cross_layer_file_as_parent_is_rejected() {
    let e = err(r#"{"layers":[{"writes":["a"]},{"writes":["a/b"]}]}"#);
    assert_eq!(e.code, ErrorCode::FileAsParent);
    assert_eq!((e.layer, e.path.as_deref()), (Some(1), Some("a/b")));
    let e = err(r#"{"layers":[{"writes":["a"]},{"mkdirs":["a/b"]}]}"#);
    assert_eq!(e.code, ErrorCode::FileAsParent);
    // Deep chain through a file.
    let e = err(r#"{"layers":[{"writes":["a/b"]},{"writes":["a/b/c/d"]}]}"#);
    assert_eq!(e.code, ErrorCode::FileAsParent);
}

#[test]
fn whiteout_clears_the_way_for_rebuilding_under_a_file() {
    // The whiteout deletes the file first, so children may be created below.
    let out = ok(r#"{"layers":[
        {"writes":["a"]},
        {"whiteouts":["a"],"writes":["a/b/c"]}
    ]}"#);
    assert_eq!(dir_src(&out, "a"), Some(1));
    assert_eq!(file_src(&out, "a/b/c"), Some(1));
    let o = one_obscured(&out, "a");
    assert_eq!((o.source_layer, o.reason, o.by_layer), (0, Reason::Whiteout, 1));
}

#[test]
fn explicit_mkdir_replaces_file_and_allows_children() {
    let out = ok(r#"{"layers":[
        {"writes":["a"]},
        {"mkdirs":["a"],"writes":["a/b"]}
    ]}"#);
    assert_eq!(dir_src(&out, "a"), Some(1));
    assert_eq!(file_src(&out, "a/b"), Some(1));
    let o = one_obscured(&out, "a");
    assert_eq!((o.source_layer, o.reason, o.by_layer), (0, Reason::ReplacedByDir, 1));
}

#[test]
fn file_write_replaces_dir_and_hides_its_subtree() {
    let out = ok(r#"{"layers":[
        {"mkdirs":["d"],"writes":["d/x","d/y/z"]},
        {"writes":["d"]}
    ]}"#);
    assert_eq!(file_src(&out, "d"), Some(1));
    let o = one_obscured(&out, "d");
    assert_eq!((o.source_layer, o.reason, o.by_layer), (0, Reason::ReplacedByFile, 1));
    for p in ["d/x", "d/y", "d/y/z"] {
        let o = one_obscured(&out, p);
        assert_eq!((o.reason, o.by_layer, o.via.as_deref()), (Reason::ReplacedAncestor, 1, Some("d")));
    }
}

#[test]
fn type_can_flip_back_and_forth_across_layers() {
    let out = ok(r#"{"layers":[
        {"writes":["t"]},
        {"mkdirs":["t"],"writes":["t/inner"]},
        {"whiteouts":["t/inner"],"writes":["t"]},
        {"whiteouts":["t"],"mkdirs":["t"]}
    ]}"#);
    assert_eq!(dir_src(&out, "t"), Some(3));
    let log = obscured(&out, "t");
    let got: Vec<(usize, Reason, usize)> =
        log.iter().map(|o| (o.source_layer, o.reason, o.by_layer)).collect();
    assert_eq!(
        got,
        vec![
            (0, Reason::ReplacedByDir, 1),
            (1, Reason::ReplacedByFile, 2),
            (2, Reason::Whiteout, 3),
        ]
    );
}

// ---------------------------------------------------------------------------
// Path & input validation
// ---------------------------------------------------------------------------

#[test]
fn rejects_non_canonical_paths() {
    for bad in [
        "", "/abs", "a/", "a//b", "a/./b", "a/../b", ".", "..", "./a",
        "non-ascii-中文", r"ctrl\tx", r"nul\u0000x",
    ] {
        let input = format!(r#"{{"layers":[{{"writes":["{bad}"]}}]}}"#);
        let e = err(&input);
        assert_eq!(e.code, ErrorCode::InvalidPath, "path {bad:?}");
        assert_eq!(e.exit_code(), 1);
    }
    // A single dotfile segment that is not exactly "." is fine, spaces too.
    let out = ok(r#"{"layers":[{"writes":[".hidden","a b/c.txt","under_score/dash-e"]}]}"#);
    assert_eq!(out.files.len(), 3);
}

#[test]
fn enforces_layer_and_record_limits() {
    // 0 layers.
    let e = err(r#"{"layers":[]}"#);
    assert_eq!(e.code, ErrorCode::InvalidInput);
    assert_eq!(e.exit_code(), 2);
    // 31 layers.
    let layers: Vec<serde_json::Value> =
        (0..31).map(|_| serde_json::json!({"writes": []})).collect();
    let e = err(&serde_json::json!({"layers": layers}).to_string());
    assert_eq!(e.code, ErrorCode::InvalidInput);
    // 3001 records in one layer.
    let writes: Vec<String> = (0..3001).map(|i| format!("f{i}")).collect();
    let e = err(&serde_json::json!({"layers": [{"writes": writes}]}).to_string());
    assert_eq!(e.code, ErrorCode::InvalidInput);
}

#[test]
fn accepts_the_maximum_size() {
    // 30 layers x 100 records = exactly 3000 records.
    let layers: Vec<serde_json::Value> = (0..30)
        .map(|l| {
            serde_json::json!({
                "writes": (0..50).map(|i| format!("dir{l}/w{i}")).collect::<Vec<_>>(),
                "mkdirs": (0..50).map(|i| format!("dir{l}/m{i}")).collect::<Vec<_>>(),
            })
        })
        .collect();
    let out = ok(&serde_json::json!({"layers": layers}).to_string());
    assert_eq!(out.stats.layers, 30);
    assert_eq!(out.stats.files, 30 * 50);
    assert_eq!(out.stats.dirs, 30 * (1 + 50));
}

#[test]
fn rejects_schema_violations() {
    for bad in [
        r#"{"layers":[{"writes":["a"],"bogus":1}]}"#, // unknown field
        r#"{"layers":"nope"}"#,                       // wrong type
        r#"{"layerz":[]}"#,                           // missing `layers`
        r#"{"layers":[{"writes":"a"}]}"#,             // writes must be a list
    ] {
        let e = err(bad);
        assert_eq!(e.code, ErrorCode::InvalidInput, "input: {bad}");
        assert_eq!(e.exit_code(), 2);
    }
    let e = err("this is not json");
    assert_eq!(e.code, ErrorCode::InvalidJson);
    assert_eq!(e.exit_code(), 2);
}

#[test]
fn empty_layer_is_a_noop() {
    let out = ok(r#"{"layers":[{"writes":["a"]},{},{"mkdirs":[],"writes":[]}]}"#);
    assert_eq!(file_src(&out, "a"), Some(0));
    assert_eq!(out.stats.layers, 3);
}

// ---------------------------------------------------------------------------
// The documented end-to-end example
// ---------------------------------------------------------------------------

#[test]
fn readme_example() {
    let out = ok(include_str!("../examples/sample.json"));
    assert_eq!(file_src(&out, "app/main.py"), Some(1)); // overwritten
    assert_eq!(file_src(&out, "app/util/helpers.py"), Some(0));
    assert_eq!(file_src(&out, "app/util/cache.py"), Some(2));
    assert_eq!(file_src(&out, "etc/config.yml"), Some(2)); // rebuilt after whiteout
    assert_eq!(file_src(&out, "data/fresh.csv"), Some(1)); // written after opaque
    assert_eq!(out.files.len(), 5);

    let expect = [
        ("app/main.py", 0, Reason::Overwritten, 1, None),
        ("data/old_a.csv", 0, Reason::OpaqueAncestor, 1, Some("data")),
        ("data/old_b.csv", 0, Reason::OpaqueAncestor, 1, Some("data")),
        ("etc/config.yml", 0, Reason::Whiteout, 1, None),
    ];
    assert_eq!(out.obscured.len(), expect.len());
    for (path, src, reason, by, via) in expect {
        let o = one_obscured(&out, path);
        assert_eq!(
            (o.source_layer, o.reason, o.by_layer, o.via.as_deref()),
            (src, reason, by, via),
            "obscured entry for {path}"
        );
    }
}
