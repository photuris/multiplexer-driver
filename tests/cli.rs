//! CLI tests: exit codes, error records, output shape, help snapshots.
//! They run the built binary and never reach a real harness.

use std::process::{Command, Output};

use rstest::rstest;

/// Runs the binary with `args`, without harness or session variables.
fn md(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_multiplexer-driver"))
        .args(args)
        .env_remove("MULTIPLEXER_DRIVER_HARNESS")
        .env_remove("MULTIPLEXER_DRIVER_SESSION")
        .env_remove("RUST_LOG")
        .output()
        .unwrap_or_else(|e| panic!("running the binary: {e}"))
}

/// The `error.type` of the last stderr line.
fn error_type(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().last().unwrap_or_default();
    let value: serde_json::Value = serde_json::from_str(last)
        .unwrap_or_else(|e| panic!("last stderr line {last:?}: {e}"));

    value["error"]["type"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn should_exit_2_with_usage_record_when_harness_missing() {
    let output = md(&["pane", "read", "%1"]);

    assert_eq!(
        (output.status.code(), error_type(&output).as_str()),
        (Some(2), "usage")
    );
}

#[test]
fn should_exit_2_when_flag_unknown() {
    let output = md(&["pane", "read", "--bogus"]);

    assert_eq!(
        (output.status.code(), error_type(&output).as_str()),
        (Some(2), "usage")
    );
}

#[test]
fn should_print_argv_when_resume_kind_known() {
    let output = md(&[
        "agent",
        "resume-args",
        "--kind",
        "claude",
        "--session-ref",
        "x",
    ]);

    assert_eq!(
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            output.stderr.is_empty(),
            output.status.code()
        ),
        (
            "{\"argv\":[\"claude\",\"--resume\",\"x\"]}\n".to_string(),
            true,
            Some(0)
        )
    );
}

#[test]
fn should_exit_5_when_resume_kind_unknown() {
    let output = md(&[
        "agent",
        "resume-args",
        "--kind",
        "gemini",
        "--session-ref",
        "x",
    ]);

    assert_eq!(output.status.code(), Some(5));
}

#[test]
fn should_exit_2_when_wait_timeout_missing() {
    let output = md(&["--harness", "tmux", "pane", "wait", "%1"]);

    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn should_emit_no_bom_or_cr_when_printing() {
    let output = md(&[
        "agent",
        "resume-args",
        "--kind",
        "claude",
        "--session-ref",
        "x",
    ]);

    assert!(
        !output.stdout.contains(&b'\r')
            && !output.stdout.starts_with(&[0xEF, 0xBB, 0xBF]),
        "stdout was {:?}",
        output.stdout
    );
}

#[test]
fn should_exit_2_when_sound_invalid() {
    let output = md(&["notify", "--message", "m", "--sound", "loud"]);

    assert_eq!(
        (output.status.code(), error_type(&output).as_str()),
        (Some(2), "usage")
    );
}

#[test]
fn should_ignore_patterns_file_when_harness_is_herdr() {
    let output = md(&[
        "--harness",
        "herdr",
        "pane",
        "status",
        "w1:p1",
        "--patterns",
        "/nonexistent",
    ]);

    assert_eq!(output.status.code(), Some(5));
}

#[test]
fn should_exit_5_when_tmux_stub_reads() {
    let output = md(&["--harness", "tmux", "pane", "read", "%1"]);

    assert_eq!(
        (output.status.code(), error_type(&output).as_str()),
        (Some(5), "unsupported")
    );
}

#[test]
fn should_mention_reset_when_pane_read_help_shown() {
    let output = md(&["pane", "read", "--help"]);

    assert!(String::from_utf8_lossy(&output.stdout).contains("reset"));
}

/// Snapshots the help of every noun and every verb.
#[rstest]
#[case::top("top", &["--help"])]
#[case::workspace("workspace", &["workspace", "--help"])]
#[case::pane("pane", &["pane", "--help"])]
#[case::agent("agent", &["agent", "--help"])]
#[case::workspace_list("workspace_list", &["workspace", "list", "--help"])]
#[case::workspace_create("workspace_create", &["workspace", "create", "--help"])]
#[case::workspace_close("workspace_close", &["workspace", "close", "--help"])]
#[case::workspace_tag("workspace_tag", &["workspace", "tag", "--help"])]
#[case::pane_list("pane_list", &["pane", "list", "--help"])]
#[case::pane_spawn("pane_spawn", &["pane", "spawn", "--help"])]
#[case::pane_split("pane_split", &["pane", "split", "--help"])]
#[case::pane_read("pane_read", &["pane", "read", "--help"])]
#[case::pane_prompt("pane_prompt", &["pane", "prompt", "--help"])]
#[case::pane_rename("pane_rename", &["pane", "rename", "--help"])]
#[case::pane_interrupt("pane_interrupt", &["pane", "interrupt", "--help"])]
#[case::pane_kill("pane_kill", &["pane", "kill", "--help"])]
#[case::pane_status("pane_status", &["pane", "status", "--help"])]
#[case::pane_wait("pane_wait", &["pane", "wait", "--help"])]
#[case::agent_resume_args("agent_resume_args", &["agent", "resume-args", "--help"])]
#[case::notify("notify", &["notify", "--help"])]
#[case::raw("raw", &["raw", "--help"])]
fn should_match_snapshot_when_help_shown(
    #[case] name: &str,
    #[case] args: &[&str],
) {
    let output = md(args);

    assert_eq!(output.status.code(), Some(0));

    insta::assert_snapshot!(
        format!("help_{name}"),
        String::from_utf8_lossy(&output.stdout)
    );
}

/// With a fake `notify-send` as the only program on PATH, hyphen-led
/// title and message must arrive as literal operands after `--`.
#[cfg(target_os = "linux")]
#[test]
fn should_pass_literal_operands_when_notify_send_text_starts_with_hyphen() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let dir = std::env::temp_dir()
        .join(format!("md-fake-notify-{}", std::process::id()));
    let log = dir.join("args.log");
    let script = dir.join("notify-send");

    fs::create_dir_all(&dir).unwrap();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a; do printf '%s\\n' \"$a\"; done > '{}'\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_multiplexer-driver"))
        .args(["notify", "--title=--help", "--message=-x"])
        .env("PATH", &dir)
        .env_remove("HERDR_ENV")
        .output()
        .unwrap_or_else(|e| panic!("running the binary: {e}"));
    let logged = fs::read_to_string(&log).unwrap_or_default();

    let _ = fs::remove_dir_all(&dir); // best effort cleanup

    assert_eq!(
        (output.status.code(), logged.as_str()),
        (Some(0), "--\n--help\n-x\n"),
        "stderr was {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}
