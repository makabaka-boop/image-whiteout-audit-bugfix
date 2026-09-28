//! End-to-end tests of the compiled binary: JSON in via stdin, JSON out via
//! stdout, exit codes per the contract.

use std::io::Write;
use std::process::{Command, Stdio};

fn run_cli(stdin: &str, args: &[&str]) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_layer-merge"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("binary spawns");
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8(out.stdout).unwrap())
}

#[test]
fn merges_stdin_to_stdout() {
    let (code, stdout) = run_cli(include_str!("../examples/sample.json"), &[]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["stats"]["layers"], 3);
    let files = v["files"].as_array().unwrap();
    let main = files.iter().find(|f| f["path"] == "app/main.py").unwrap();
    assert_eq!(main["source_layer"], 1);
}

#[test]
fn semantic_errors_exit_1_with_json_error() {
    let (code, stdout) =
        run_cli(r#"{"layers":[{"writes":["a"]},{"writes":["a/b"]}]}"#, &[]);
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "file_as_parent");
    assert_eq!(v["error"]["layer"], 1);
    assert_eq!(v["error"]["path"], "a/b");
}

#[test]
fn malformed_input_exits_2() {
    let (code, stdout) = run_cli("not json at all", &[]);
    assert_eq!(code, 2);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "invalid_json");

    let (code, stdout) = run_cli(r#"{"layers":[]}"#, &[]);
    assert_eq!(code, 2);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["error"]["code"], "invalid_input");
}

#[test]
fn pretty_flag_only_changes_formatting() {
    let input = include_str!("../examples/sample.json");
    let (code_c, compact) = run_cli(input, &[]);
    let (code_p, pretty) = run_cli(input, &["--pretty"]);
    assert_eq!((code_c, code_p), (0, 0));
    assert!(pretty.contains('\n'));
    assert!(!compact.trim().contains('\n'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&compact).unwrap(),
        serde_json::from_str::<serde_json::Value>(&pretty).unwrap()
    );
}

#[test]
fn help_exits_0() {
    let (code, _) = run_cli("", &["--help"]);
    assert_eq!(code, 0);
}
