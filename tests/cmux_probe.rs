//! Black-box probe of the cmux driver. It runs the built binary against a
//! fake `cmux` on PATH (`tests/fixtures/cmux/fake-cmux.sh`) that records
//! every call and answers from the fixture files, so the test pins the
//! exact argv the driver issues and the JSON it prints. It never reaches a
//! real cmux and runs on every unix CI runner.
//!
//! Written by the overseer as the behavioral contract of task 001; the
//! implementer does not edit it.

#![cfg(unix)]
#![expect(clippy::expect_used, reason = "test helpers panic on failure")]

use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

use rstest::rstest;
use serde_json::Value;

/// The Claude surface in `tree_all.json`, `tree_workspace.json`, and
/// `sessions.json`.
const S4: &str = "9368A7E3-DCC1-4510-8DD6-DB8A819770F8";
/// The workspace that holds [`S4`].
const W4: &str = "3543ECC9-B2A1-4751-9961-1695A3EC1222";
/// The pane that holds [`S4`].
const P4: &str = "C821ED94-AD86-4FBF-9788-EFF3D8ECB8DB";
/// The `caller.surface_id` of `identify.json`.
const SPLIT_SURFACE: &str = "2013CF4A-A408-4556-94D9-47C90D2B5509";
/// The `caller.workspace_id` of `identify.json`.
const SPLIT_WORKSPACE: &str = "71B1C2DE-315C-4A89-8121-4D6175B6905F";
/// A shell surface with no agent session, in workspace `group-1`.
const SHELL_SURFACE: &str = "4198F9DC-3B4A-444D-A428-47C2A9E1A58B";
/// The workspace of [`SHELL_SURFACE`].
const SHELL_WORKSPACE: &str = "5E20F894-5AEE-4DAB-9E37-A38278D1751D";
/// The pane of [`SHELL_SURFACE`].
const SHELL_PANE: &str = "91E3548A-3F3E-4CC9-951F-026C7FCF64FB";
/// The Claude session id in `sessions.json`.
const SESSION_ID: &str = "27a40df8-8472-4d5e-9e09-4859e42285ff";
/// A socket path for the --session tests.
const SOCKET: &str = "/tmp/md-probe-cmux.sock";

/// Counter that makes every fake directory unique within one process.
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// One recorded invocation of the fake.
#[derive(Debug)]
struct Call {
    /// `CMUX_*` variables the fake saw (`unset` when absent).
    env: BTreeMap<String, String>,
    /// The argv, without the program name.
    argv: Vec<String>,
}

/// A fake `cmux` installed in its own directory, plus the environment the
/// binary runs with.
struct Fake {
    /// Holds `cmux` (the script) and `calls.log`.
    dir: PathBuf,
    /// Extra environment for the binary.
    extra: Vec<(String, String)>,
}

impl Fake {
    /// Installs the fake script as `cmux` in a fresh directory.
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir()
            .join(format!("md-cmux-probe-{}-{n}", std::process::id()));

        fs::create_dir_all(&dir).expect("create fake dir");

        let script = fixtures().join("fake-cmux.sh");
        let target = dir.join("cmux");

        fs::copy(&script, &target).expect("copy fake cmux");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .expect("chmod fake cmux");

        Self {
            dir,
            extra: Vec::new(),
        }
    }

    /// Adds one environment variable for the binary (and the fake).
    fn env(mut self, key: &str, value: &str) -> Self {
        self.extra.push((key.to_string(), value.to_string()));

        self
    }

    /// Path of the call log.
    fn log(&self) -> PathBuf {
        self.dir.join("calls.log")
    }

    /// Runs `multiplexer-driver --harness cmux args…` with only the fake
    /// (plus the system shell utilities) on PATH.
    fn run(&self, args: &[&str]) -> Output {
        let mut command =
            Command::new(env!("CARGO_BIN_EXE_multiplexer-driver"));

        command
            .args(["--harness", "cmux"])
            .args(args)
            .env("PATH", format!("{}:/usr/bin:/bin", self.dir.display()))
            .env("FAKE_CMUX_LOG", self.log())
            .env("FAKE_CMUX_FIXTURES", fixtures())
            .env_remove("MULTIPLEXER_DRIVER_HARNESS")
            .env_remove("MULTIPLEXER_DRIVER_SESSION")
            .env_remove("RUST_LOG")
            .env_remove("CMUX_SOCKET_PATH")
            .env_remove("CMUX_SOCKET")
            .env_remove("CMUX_QUIET");

        for (key, value) in &self.extra {
            command.env(key, value);
        }

        command.output().expect("running multiplexer-driver")
    }

    /// Every call the fake recorded, in order.
    fn calls(&self) -> Vec<Call> {
        let text = fs::read_to_string(self.log()).unwrap_or_default();
        let mut calls = Vec::new();

        for line in text.lines() {
            if line == "#call" {
                calls.push(Call {
                    env: BTreeMap::new(),
                    argv: Vec::new(),
                });
            } else if let Some(pair) = line.strip_prefix("#env ") {
                let (key, value) = pair.split_once('=').expect("env pair");

                calls
                    .last_mut()
                    .expect("env before #call")
                    .env
                    .insert(key.to_string(), value.to_string());
            } else if let Some(arg) = line.strip_prefix('\t') {
                calls
                    .last_mut()
                    .expect("arg before #call")
                    .argv
                    .push(arg.to_string());
            } else {
                panic!("unexpected log line {line:?}");
            }
        }

        calls
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir); // best effort
    }
}

/// The fixture directory.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cmux")
}

/// Owned strings from literals.
fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// stdout as one JSON document.
fn json(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);

    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("stdout {text:?} is not JSON: {e}"))
}

/// stdout as JSON Lines.
fn json_lines(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line {line:?} is not JSON: {e}"))
        })
        .collect()
}

/// The `error.type` on the last stderr line.
fn error_type(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().last().unwrap_or_default();
    let value: Value = serde_json::from_str(last)
        .unwrap_or_else(|e| panic!("last stderr line {last:?}: {e}"));

    value["error"]["type"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The process exit code.
fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

/// Asserts success and returns stdout as JSON.
fn ok_json(output: &Output) -> Value {
    assert_eq!(
        code(output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    json(output)
}

#[test]
fn should_list_panes_joined_with_sessions() {
    let fake = Fake::new();

    let output = fake.run(&["pane", "list"]);

    assert_eq!(
        code(&output),
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let records = json_lines(&output);

    assert_eq!(records.len(), 5, "{records:#?}");

    let claude = records
        .iter()
        .find(|r| r["handle"] == S4)
        .expect("claude surface listed");

    assert_eq!(claude["workspace_id"], W4);
    assert_eq!(claude["workspace_label"], "md-test-probe-renamed");
    assert_eq!(claude["tab_id"], P4);
    assert_eq!(claude["tab_label"], Value::Null);
    assert_eq!(claude["label"], "✳ Pong reply test");
    assert_eq!(claude["cwd"], "/private/tmp");
    assert_eq!(claude["agent"], "claude");
    assert_eq!(claude["agent_session"]["kind"], "id");
    assert_eq!(claude["agent_session"]["value"], SESSION_ID);
    assert_eq!(claude["session_confidence"], "native");
    assert_eq!(claude["status"], "idle");
    assert_eq!(claude["status_confidence"], "native");

    let shell = records
        .iter()
        .find(|r| r["handle"] == SHELL_SURFACE)
        .expect("shell surface listed");

    assert_eq!(shell["workspace_id"], SHELL_WORKSPACE);
    assert_eq!(shell["workspace_label"], "group-1");
    assert_eq!(shell["tab_id"], SHELL_PANE);
    assert_eq!(shell["label"], "/home/u");
    assert_eq!(shell["cwd"], Value::Null);
    assert_eq!(shell["agent"], Value::Null);
    assert_eq!(shell["agent_session"], Value::Null);
    assert_eq!(shell["session_confidence"], "none");
    assert_eq!(shell["status"], "unknown");
    assert_eq!(shell["status_confidence"], "none");

    let calls = fake.calls();
    let argvs: Vec<&[String]> =
        calls.iter().map(|c| c.argv.as_slice()).collect();

    assert_eq!(calls.len(), 2, "{argvs:?}");
    assert!(
        argvs.contains(
            &argv(&["tree", "--all", "--json", "--id-format", "uuids"])
                .as_slice()
        ) && argvs
            .contains(&argv(&["sessions", "--json", "--all"]).as_slice()),
        "{argvs:?}"
    );

    for call in &calls {
        assert_eq!(call.env.get("CMUX_QUIET").map(String::as_str), Some("1"));
    }
}

#[test]
fn should_filter_pane_list_by_workspace() {
    let fake = Fake::new();

    let output = fake.run(&["pane", "list", "--workspace", W4]);
    let records = json_lines(&output);

    assert_eq!(code(&output), 0);
    assert_eq!(records.len(), 2, "{records:#?}");
    assert!(records.iter().all(|r| r["workspace_id"] == W4));
}

#[rstest]
#[case::idle("sessions.json", "idle")]
#[case::running("sessions_running.json", "working")]
#[case::needs_input("sessions_needs_input.json", "blocked")]
fn should_report_native_status_when_session_active(
    #[case] sessions: &str,
    #[case] expected: &str,
) {
    let fake = Fake::new().env("FAKE_CMUX_SESSIONS", sessions);

    let value = ok_json(&fake.run(&["pane", "status", S4]));

    assert_eq!(value["handle"], S4);
    assert_eq!(value["status"], expected);
    assert_eq!(value["confidence"], "native");
    assert_eq!(value["tail"], Value::Null);

    let calls = fake.calls();

    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        calls[0].argv,
        argv(&["sessions", "--json", "--surface", S4])
    );
}

#[test]
fn should_report_unknown_and_skip_read_when_no_session_and_no_patterns() {
    let fake = Fake::new().env("FAKE_CMUX_SESSIONS", "sessions_empty.json");

    let value = ok_json(&fake.run(&["pane", "status", S4]));

    assert_eq!(value["status"], "unknown");
    assert_eq!(value["confidence"], "none");
    assert_eq!(value["tail"], Value::Null);

    let calls = fake.calls();

    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].argv[0], "sessions");
}

#[test]
fn should_classify_tail_with_patterns_when_no_session() {
    let fake = Fake::new().env("FAKE_CMUX_SESSIONS", "sessions_empty.json");
    let patterns = fake.dir.join("patterns.json");

    fs::write(&patterns, r#"{"idle":["❯ $"]}"#).expect("write patterns");

    let value = ok_json(&fake.run(&[
        "pane",
        "status",
        S4,
        "--patterns",
        patterns.to_str().expect("utf-8 path"),
    ]));

    assert_eq!(value["status"], "idle");
    assert_eq!(value["confidence"], "heuristic");
    assert!(
        value["tail"]
            .as_str()
            .is_some_and(|t| t.contains("hello-42")),
        "{value}"
    );

    let calls = fake.calls();

    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1].argv,
        argv(&["read-screen", "--surface", S4, "--lines", "15", "--json"])
    );
}

#[test]
fn should_wait_until_idle_natively() {
    let fake = Fake::new();

    let value = ok_json(&fake.run(&[
        "pane",
        "wait",
        S4,
        "--timeout",
        "5s",
        "--until",
        "idle",
    ]));

    assert_eq!(value["status"], "idle");
    assert_eq!(value["confidence"], "native");
}

#[test]
fn should_read_trimmed_text_when_pane_read() {
    let fake = Fake::new();

    let value = ok_json(&fake.run(&["pane", "read", S4, "--lines", "5"]));

    assert_eq!(value["handle"], S4);
    assert_eq!(
        value["output"],
        "\n/tmp ✗ echo hello-$((40+2))\nhello-42\n\n/tmp ❯ "
    );

    let calls = fake.calls();

    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].argv,
        argv(&["read-screen", "--surface", S4, "--lines", "5", "--json"])
    );
}

#[test]
fn should_exit_5_when_read_ansi() {
    let fake = Fake::new();

    let output = fake.run(&["pane", "read", S4, "--ansi"]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (5, "unsupported")
    );
    assert!(fake.calls().is_empty(), "cmux was called");
}

#[test]
fn should_exit_5_when_workspace_tag() {
    let fake = Fake::new();

    let output = fake.run(&["workspace", "tag", W4, "k=v"]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (5, "unsupported")
    );
}

#[test]
fn should_exit_2_when_spawn_names_workspace() {
    let fake = Fake::new();

    let output = fake.run(&[
        "pane",
        "spawn",
        "--name",
        "impl-1",
        "--workspace",
        W4,
        "--",
        "bash",
    ]);

    assert_eq!((code(&output), error_type(&output).as_str()), (2, "usage"));
    assert!(fake.calls().is_empty(), "cmux was called");
}

#[test]
fn should_exit_2_when_spawn_has_no_command() {
    let fake = Fake::new();

    let output =
        fake.run(&["pane", "spawn", "--name", "impl-1", "--cwd", "/tmp"]);

    assert_eq!((code(&output), error_type(&output).as_str()), (2, "usage"));
    assert!(fake.calls().is_empty(), "cmux was called");
}

#[test]
fn should_create_workspace_then_rename_when_spawned() {
    let fake = Fake::new();

    let value = ok_json(&fake.run(&[
        "pane", "spawn", "--name", "impl-1", "--cwd", "/tmp", "--", "claude",
        "--model", "sonnet",
    ]));

    assert_eq!(value, serde_json::json!({ "handle": S4, "started": true }));

    let calls = fake.calls();
    let argvs: Vec<Vec<String>> = calls.into_iter().map(|c| c.argv).collect();

    assert_eq!(
        argvs,
        vec![
            argv(&[
                "new-workspace",
                "--name",
                "impl-1",
                "--cwd",
                "/tmp",
                "--command",
                "claude --model sonnet",
                "--focus",
                "false",
            ]),
            argv(&[
                "tree",
                "--workspace",
                "workspace:4",
                "--json",
                "--id-format",
                "uuids"
            ]),
            argv(&[
                "rename-tab",
                "--workspace",
                W4,
                "--surface",
                S4,
                "--",
                "impl-1"
            ]),
        ]
    );
}

#[test]
fn should_split_paste_then_rename_when_split() {
    let fake = Fake::new();

    let value = ok_json(&fake.run(&[
        "pane",
        "split",
        S4,
        "--name",
        "impl-2",
        "--direction",
        "down",
        "--cwd",
        "/tmp",
        "--",
        "bash",
        "-c",
        "echo hi",
    ]));

    assert_eq!(
        value,
        serde_json::json!({ "handle": SPLIT_SURFACE, "started": true })
    );

    let calls = fake.calls();

    assert_eq!(calls.len(), 4, "{calls:?}");
    assert_eq!(
        calls[0].argv,
        argv(&["new-split", "down", "--surface", S4, "--focus", "false"])
    );
    assert_eq!(
        calls[1].argv,
        argv(&[
            "identify",
            "--workspace",
            "workspace:4",
            "--surface",
            "surface:8",
            "--json",
            "--id-format",
            "uuids",
        ])
    );
    assert_eq!(calls[2].argv[..2], argv(&["rpc", "terminal.paste"]));

    let params: Value = serde_json::from_str(&calls[2].argv[2])
        .expect("paste params are JSON");

    assert_eq!(
        params,
        serde_json::json!({
            "surface_id": SPLIT_SURFACE,
            "text": "cd /tmp && bash -c 'echo hi'",
            "submit_key": "return",
        })
    );
    assert_eq!(
        calls[3].argv,
        argv(&[
            "rename-tab",
            "--workspace",
            SPLIT_WORKSPACE,
            "--surface",
            SPLIT_SURFACE,
            "--",
            "impl-2",
        ])
    );
}

#[test]
fn should_default_split_direction_to_right() {
    let fake = Fake::new();

    let output = fake.run(&[
        "pane", "split", S4, "--name", "impl-3", "--cwd", "/tmp", "--", "bash",
    ]);

    assert_eq!(code(&output), 0);
    assert_eq!(fake.calls()[0].argv[..2], argv(&["new-split", "right"]));
}

#[test]
fn should_print_started_false_when_rename_fails_after_spawn() {
    let fake = Fake::new().env("FAKE_CMUX_FAIL", "rename-tab");

    let output = fake.run(&[
        "pane", "spawn", "--name", "impl-1", "--cwd", "/tmp", "--", "bash",
    ]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (6, "start_failed")
    );
    assert_eq!(
        json(&output),
        serde_json::json!({ "handle": S4, "started": false })
    );
}

#[test]
fn should_print_started_false_when_paste_fails_after_split() {
    let fake = Fake::new().env("FAKE_CMUX_FAIL", "rpc");

    let output = fake.run(&[
        "pane", "split", S4, "--name", "impl-2", "--cwd", "/tmp", "--", "bash",
    ]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (6, "start_failed")
    );
    assert_eq!(
        json(&output),
        serde_json::json!({ "handle": SPLIT_SURFACE, "started": false })
    );
}

#[test]
fn should_paste_text_verbatim_when_prompted() {
    let fake = Fake::new();
    let text = r"say \n and \t literally; it's $HOME";

    let value = ok_json(&fake.run(&["pane", "prompt", S4, "--text", text]));

    assert_eq!(value, serde_json::json!({ "handle": S4, "sent": true }));

    let calls = fake.calls();

    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].argv[..2], argv(&["rpc", "terminal.paste"]));

    let params: Value = serde_json::from_str(&calls[0].argv[2])
        .expect("paste params are JSON");

    assert_eq!(
        params,
        serde_json::json!({
            "surface_id": S4,
            "text": text,
            "submit_key": "return",
        })
    );
}

#[test]
fn should_send_ctrl_c_when_interrupted() {
    let fake = Fake::new();

    let value = ok_json(&fake.run(&["pane", "interrupt", S4]));

    assert_eq!(
        value,
        serde_json::json!({ "handle": S4, "interrupted": true })
    );
    assert_eq!(
        fake.calls()[0].argv,
        argv(&["send-key", "--surface", S4, "ctrl+c"])
    );
}

#[test]
fn should_locate_workspace_then_rename_tab_when_renamed() {
    let fake = Fake::new();

    let value =
        ok_json(&fake.run(&["pane", "rename", S4, "--label", "impl-9"]));

    assert_eq!(
        value,
        serde_json::json!({ "handle": S4, "label": "impl-9" })
    );

    let argvs: Vec<Vec<String>> =
        fake.calls().into_iter().map(|c| c.argv).collect();

    assert_eq!(
        argvs,
        vec![
            argv(&["tree", "--all", "--json", "--id-format", "uuids"]),
            argv(&[
                "rename-tab",
                "--workspace",
                W4,
                "--surface",
                S4,
                "--",
                "impl-9"
            ]),
        ]
    );
}

#[test]
fn should_exit_4_when_renamed_surface_is_not_in_tree() {
    let fake = Fake::new();

    let output = fake.run(&[
        "pane",
        "rename",
        "00000000-0000-0000-0000-000000000000",
        "--label",
        "x",
    ]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (4, "not_found")
    );
    assert_eq!(fake.calls().len(), 1, "only the tree lookup runs");
}

#[test]
fn should_close_surface_when_killed() {
    let fake = Fake::new();

    let value = ok_json(&fake.run(&["pane", "kill", S4]));

    assert_eq!(value, serde_json::json!({ "handle": S4, "killed": true }));

    let argvs: Vec<Vec<String>> =
        fake.calls().into_iter().map(|c| c.argv).collect();

    assert_eq!(
        argvs,
        vec![
            argv(&["tree", "--all", "--json", "--id-format", "uuids"]),
            argv(&["close-surface", "--workspace", W4, "--surface", S4]),
        ]
    );
}

#[test]
fn should_close_workspace_when_killed_surface_is_last() {
    let fake = Fake::new()
        .env("FAKE_CMUX_FAIL", "close-surface")
        .env("FAKE_CMUX_FAIL_WITH", "err_last_surface.txt");

    let value = ok_json(&fake.run(&["pane", "kill", S4]));

    assert_eq!(value, serde_json::json!({ "handle": S4, "killed": true }));

    let argvs: Vec<Vec<String>> =
        fake.calls().into_iter().map(|c| c.argv).collect();

    assert_eq!(argvs.len(), 3, "{argvs:?}");
    assert_eq!(argvs[2], argv(&["close-workspace", "--workspace", W4]));
}

#[test]
fn should_exit_4_when_cmux_reports_not_found() {
    let fake = Fake::new().env("FAKE_CMUX_FAIL", "read-screen");

    let output = fake.run(&["pane", "read", S4]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (4, "not_found")
    );
}

#[rstest]
#[case::no_socket("err_no_socket.txt")]
#[case::access_denied("err_access_denied.txt")]
#[case::bad_password("err_bad_password.txt")]
fn should_exit_3_when_cmux_is_unreachable(#[case] stderr: &str) {
    let fake = Fake::new()
        .env("FAKE_CMUX_FAIL", "tree")
        .env("FAKE_CMUX_FAIL_WITH", stderr);

    let output = fake.run(&["workspace", "list"]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (3, "harness_unavailable"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn should_exit_3_when_cmux_is_not_installed() {
    let fake = Fake::new();

    fs::remove_file(fake.dir.join("cmux")).expect("remove fake");

    let output = fake.run(&["workspace", "list"]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (3, "harness_unavailable")
    );
}

#[test]
fn should_exit_1_when_error_code_unknown() {
    let fake = Fake::new()
        .env("FAKE_CMUX_FAIL", "send-key")
        .env("FAKE_CMUX_FAIL_WITH", "err_invalid_params.txt");

    let output = fake.run(&["pane", "interrupt", S4]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (1, "unexpected")
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Unknown key"),
        "cmux's message is kept"
    );
}

#[test]
fn should_prefix_socket_and_scrub_env_when_session_given() {
    let fake = Fake::new()
        .env("CMUX_SOCKET_PATH", "/from/env.sock")
        .env("CMUX_SOCKET", "/from/env.sock");

    let output =
        fake.run(&["--session", SOCKET, "pane", "read", S4, "--lines", "3"]);

    assert_eq!(code(&output), 0);

    let calls = fake.calls();

    assert_eq!(
        calls[0].argv,
        argv(&[
            "--socket",
            SOCKET,
            "read-screen",
            "--surface",
            S4,
            "--lines",
            "3",
            "--json"
        ])
    );
    assert_eq!(
        (
            calls[0].env.get("CMUX_SOCKET_PATH").map(String::as_str),
            calls[0].env.get("CMUX_SOCKET").map(String::as_str),
        ),
        (Some("unset"), Some("unset")),
        "the ambient socket must not leak into the child"
    );
}

#[test]
fn should_keep_ambient_socket_when_no_session_given() {
    let fake = Fake::new().env("CMUX_SOCKET_PATH", "/from/env.sock");

    let output = fake.run(&["pane", "read", S4, "--lines", "3"]);

    assert_eq!(code(&output), 0);

    let calls = fake.calls();

    assert_eq!(calls[0].argv[0], "read-screen");
    assert_eq!(
        calls[0].env.get("CMUX_SOCKET_PATH").map(String::as_str),
        Some("/from/env.sock")
    );
}

#[test]
fn should_list_workspaces_from_tree() {
    let fake = Fake::new();

    let output = fake.run(&["workspace", "list"]);
    let records = json_lines(&output);

    assert_eq!(code(&output), 0);
    assert_eq!(records.len(), 4, "{records:#?}");
    assert_eq!(
        records[1],
        serde_json::json!({
            "workspace_id": "8E9D74DC-7771-4FD6-B127-95015AC1FB75",
            "label": "~/proj/a",
            "focused": true,
            "tags": {},
        })
    );
    assert_eq!(records[3]["workspace_id"], W4);
    assert_eq!(records[3]["focused"], false);
    assert_eq!(
        fake.calls()[0].argv,
        argv(&["tree", "--all", "--json", "--id-format", "uuids"])
    );
}

#[test]
fn should_create_workspace() {
    let fake = Fake::new();

    let value = ok_json(&fake.run(&[
        "workspace",
        "create",
        "--label",
        "ws-1",
        "--cwd",
        "/tmp",
    ]));

    assert_eq!(
        value,
        serde_json::json!({
            "workspace_id": W4,
            "label": "md-test-probe",
            "focused": false,
            "tags": {},
        })
    );

    let argvs: Vec<Vec<String>> =
        fake.calls().into_iter().map(|c| c.argv).collect();

    assert_eq!(
        argvs,
        vec![
            argv(&[
                "new-workspace",
                "--name",
                "ws-1",
                "--cwd",
                "/tmp",
                "--focus",
                "false"
            ]),
            argv(&[
                "tree",
                "--workspace",
                "workspace:4",
                "--json",
                "--id-format",
                "uuids"
            ]),
        ]
    );
}

#[test]
fn should_close_workspace() {
    let fake = Fake::new();

    let value = ok_json(&fake.run(&["workspace", "close", W4]));

    assert_eq!(
        value,
        serde_json::json!({ "workspace_id": W4, "closed": true })
    );
    assert_eq!(
        fake.calls()[0].argv,
        argv(&["close-workspace", "--workspace", W4])
    );
}

#[test]
fn should_ignore_session_that_is_not_active_for_surface() {
    let fake = Fake::new().env("FAKE_CMUX_SESSIONS", "sessions_inactive.json");

    let value = ok_json(&fake.run(&["pane", "status", S4]));

    assert_eq!(value["status"], "unknown");
    assert_eq!(value["confidence"], "none");
}

#[test]
fn should_exit_1_when_paste_is_not_submitted() {
    let fake =
        Fake::new().env("FAKE_CMUX_PASTE", "terminal_paste_unsubmitted.json");

    let output = fake.run(&["pane", "prompt", S4, "--text", "hi"]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (1, "unexpected")
    );
}

#[test]
fn should_exit_6_when_split_paste_is_not_submitted() {
    let fake =
        Fake::new().env("FAKE_CMUX_PASTE", "terminal_paste_unsubmitted.json");

    let output = fake.run(&[
        "pane", "split", S4, "--name", "impl-2", "--cwd", "/tmp", "--", "bash",
    ]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (6, "start_failed")
    );
    assert_eq!(
        json(&output),
        serde_json::json!({ "handle": SPLIT_SURFACE, "started": false })
    );
}

#[test]
fn should_pass_raw_args_through() {
    let fake = Fake::new();

    let output = fake.run(&["raw", "--", "ping"]);

    assert_eq!(code(&output), 0);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "PONG\n");
    assert_eq!(fake.calls()[0].argv, argv(&["ping"]));
}

#[test]
fn should_exit_1_when_raw_command_fails() {
    let fake = Fake::new();

    let output = fake.run(&["raw", "--", "no-such-command"]);

    assert_eq!(
        (code(&output), error_type(&output).as_str()),
        (1, "unexpected")
    );
}
