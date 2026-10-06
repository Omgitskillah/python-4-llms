//! Exercise the Python adapter directly without Docker or an MCP fixture.
use serde_json::json;
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run(source: &str) -> Output {
    let mut child = Command::new("python3")
        .args(["-I", "-B", "-u"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/sandbox/runner.py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 adapter");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(json!({"code": source, "stdin": ""}).to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn runtime_error_shows_source_and_preserves_partial_stdout() {
    let out = run("print('partial')\ndef fit():\n    return [1, 2] - 1.0\nfit()\n");
    assert!(!out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "partial\n");
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("line 3, in fit"), "{error}");
    assert!(error.contains("return [1, 2] - 1.0"), "{error}");
    assert!(error.contains("TypeError:"), "{error}");
    assert!(!error.contains("runner.py"), "{error}");
}

#[test]
fn syntax_error_keeps_source_and_caret_without_adapter_frames() {
    let out = run("if True\n    print('bad')\n");
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("script.py\", line 1"), "{error}");
    assert!(error.contains("if True") && error.contains('^'), "{error}");
    assert!(error.contains("SyntaxError:"), "{error}");
    assert!(!error.contains("runner.py"), "{error}");
}

#[test]
fn chained_exceptions_show_both_sources_without_adapter_frames() {
    let out = run(
        "try:\n    1 / 0\nexcept ZeroDivisionError as cause:\n    raise ValueError('fit failed') from cause\n",
    );
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(error.contains("1 / 0"), "{error}");
    assert!(
        error.contains("raise ValueError('fit failed') from cause"),
        "{error}"
    );
    assert!(
        error.contains("ZeroDivisionError:") && error.contains("ValueError: fit failed"),
        "{error}"
    );
    assert!(!error.contains("runner.py"), "{error}");
}

#[test]
fn successful_script_and_system_exit_keep_their_exit_behavior() {
    let out = run("import sys\nprint(sys.argv[0], __file__)\n");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "script.py script.py\n"
    );
    assert!(out.stderr.is_empty());
    assert_eq!(run("raise SystemExit(7)").status.code(), Some(7));
}

#[test]
fn exception_groups_preserve_nested_sources_without_adapter_frames() {
    let out = run(
        "import sys\nif sys.version_info < (3, 11):\n    print('exception groups unavailable')\n    raise SystemExit(0)\ntry:\n    [1, 2] - 1.0\nexcept TypeError as cause:\n    raise ExceptionGroup('fit errors', [cause]) from None\n",
    );
    if out.stdout == b"exception groups unavailable\n" {
        assert!(out.status.success() && out.stderr.is_empty());
        return; // The pinned Docker Python test below covers groups on Python 3.14.
    }
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(error.contains("ExceptionGroup: fit errors"), "{error}");
    assert!(error.contains("[1, 2] - 1.0"), "{error}");
    assert!(error.contains("TypeError:"), "{error}");
    assert!(!error.contains("runner.py"), "{error}");
}
