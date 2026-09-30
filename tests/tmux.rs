//! tmux integration tests. Each test owns a private tmux server
//! (`-L md-test-<pid>-<n>`) and kills it on drop, so the user's own
//! server is never touched.

use std::{
    fs, io,
    path::Path,
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
    thread,
    time::Duration,
};

use rstest::rstest;

use multiplexer_driver::{
    driver::{Driver, SpawnRequest, SplitRequest},
    model::{Direction, Handle, Status},
    patterns::Patterns,
    tmux::Tmux,
};

/// Counter that makes each test's socket name unique.
static NEXT: AtomicU32 = AtomicU32::new(0);

/// A private tmux server with one session, `ws`, killed on drop.
struct Server {
    /// Socket name.
    name: String,
    /// Socket file, which tmux leaves behind after `kill-server`.
    socket: String,
}

impl Server {
    /// Starts a server, or returns `None` when tmux is not installed.
    /// Any other startup failure panics with tmux's stderr.
    fn start() -> Option<Self> {
        let name = format!(
            "md-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let output = match Command::new("tmux")
            .args(["-L", &name, "new-session", "-d", "-s", "ws", "-c", "/tmp"])
            .args(["-P", "-F", "#{socket_path}"])
            .output()
        {
            Ok(output) => output,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => panic!("starting tmux: {e}"),
        };

        assert!(
            output.status.success(),
            "tmux failed to start: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let socket =
            String::from_utf8_lossy(&output.stdout).trim().to_string();

        Some(Self { name, socket })
    }

    /// A driver bound to this server.
    fn driver(&self) -> Tmux {
        Tmux::new(Some(self.name.clone()), None)
    }

    /// Runs tmux against this server and returns stdout.
    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-L", &self.name])
            .args(args)
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default(); // a failed call reads as empty output

        String::from_utf8_lossy(&out).trim().to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux") // best effort cleanup
            .args(["-L", &self.name, "kill-server"])
            .status();
        let _ = fs::remove_file(&self.socket); // tmux leaves it behind
    }
}

/// Skips the test when tmux is missing.
macro_rules! server {
    () => {
        match Server::start() {
            Some(s) => s,
            None => {
                eprintln!("tmux not available; skipping");
                return;
            }
        }
    };
}

/// Owned argv for a command.
fn cmd(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_string()).collect()
}

/// Polls `check` every 100 ms for up to 5 s; true once it holds. The
/// pane's shell can take a while to start, so fixed sleeps are flaky.
fn eventually(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..50 {
        if check() {
            return true;
        }

        thread::sleep(Duration::from_millis(100));
    }

    false
}

/// The ID of the first pane of session `ws`.
fn first_pane(server: &Server) -> Handle {
    Handle(server.tmux(&["display-message", "-p", "-t", "ws", "#{pane_id}"]))
}

#[test]
fn should_spawn_window_and_read_output_when_command_given() {
    let server = server!();
    let driver = server.driver();
    let ws =
        server.tmux(&["display-message", "-p", "-t", "ws", "#{session_id}"]);
    let command = cmd(&["bash", "-c", "echo hello-md; sleep 30"]);

    let handle = driver
        .pane_spawn(&SpawnRequest {
            name: "impl-1",
            workspace: Some(&ws),
            cwd: Path::new("/tmp"),
            command: &command,
        })
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    let output = driver.pane_read(&handle, 10, false).unwrap();

    assert!(output.contains("hello-md"), "output was {output:?}");
}

#[test]
fn should_store_label_option_when_spawned() {
    let server = server!();
    let driver = server.driver();
    let ws =
        server.tmux(&["display-message", "-p", "-t", "ws", "#{session_id}"]);
    let command = cmd(&["sleep", "30"]);

    let handle = driver
        .pane_spawn(&SpawnRequest {
            name: "impl-2",
            workspace: Some(&ws),
            cwd: Path::new("/tmp"),
            command: &command,
        })
        .unwrap();

    assert_eq!(
        server.tmux(&[
            "display-message",
            "-p",
            "-t",
            &handle.0,
            "#{@md-label}"
        ]),
        "impl-2"
    );
}

#[test]
fn should_keep_sibling_when_split_pane_killed() {
    let server = server!();
    let driver = server.driver();
    let first = first_pane(&server);
    let command = cmd(&["sleep", "30"]);

    let second = driver
        .pane_split(&SplitRequest {
            target: &first,
            direction: Direction::Down,
            name: "impl-3",
            cwd: Path::new("/tmp"),
            command: &command,
        })
        .unwrap();
    driver.pane_kill(&second).unwrap();

    assert_eq!(
        server.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]),
        first.0
    );
}

#[test]
fn should_type_text_and_submit_when_prompted() {
    let server = server!();
    let driver = server.driver();
    let pane = first_pane(&server);

    driver
        .pane_prompt(&pane, "echo prompted-$((40+2))")
        .unwrap();

    assert!(eventually(|| {
        driver
            .pane_read(&pane, 10, false)
            .is_ok_and(|out| out.contains("prompted-42"))
    }));
}

#[test]
fn should_keep_handle_and_set_label_when_renamed() {
    let server = server!();
    let driver = server.driver();
    let pane = first_pane(&server);

    driver.pane_rename(&pane, "renamed").unwrap();

    assert_eq!(
        server.tmux(&["display-message", "-p", "-t", &pane.0, "#{@md-label}"]),
        "renamed"
    );
}

#[test]
fn should_report_not_found_when_pane_missing() {
    let server = server!();

    let err = server
        .driver()
        .pane_read(&Handle("%999".into()), 5, false)
        .unwrap_err();

    assert_eq!(err.kind(), "not_found");
}

#[test]
fn should_report_none_confidence_when_no_patterns() {
    let server = server!();
    let pane = first_pane(&server);

    let result = server.driver().pane_status(&pane).unwrap();

    assert_eq!(serde_json::to_value(&result).unwrap()["confidence"], "none");
}

#[test]
fn should_classify_idle_when_pattern_matches() {
    let server = server!();
    let pane = first_pane(&server);
    let patterns = Patterns::from_json(r#"{"idle":["md-2-ready"]}"#).unwrap();
    let driver = Tmux::new(Some(server.name.clone()), Some(patterns));

    // The pattern matches only the output, not the typed command line.
    driver.pane_prompt(&pane, "echo md-$((1+1))-ready").unwrap();

    assert!(eventually(|| {
        driver
            .pane_status(&pane)
            .is_ok_and(|r| r.status == Status::Idle)
    }));
}

#[test]
fn should_pass_args_through_when_raw() {
    let server = server!();

    let out = server
        .driver()
        .raw(&cmd(&["list-sessions", "-F", "#{session_name}"]))
        .unwrap();

    assert_eq!(out.trim(), "ws");
}

#[test]
fn should_fail_unexpected_when_raw_command_fails() {
    let server = server!();

    let err = server
        .driver()
        .raw(&cmd(&["kill-pane", "-t", "%999"]))
        .unwrap_err();

    assert_eq!(err.kind(), "unexpected");
}

#[test]
fn should_return_only_requested_lines_when_read() {
    let server = server!();
    let driver = server.driver();
    let ws =
        server.tmux(&["display-message", "-p", "-t", "ws", "#{session_id}"]);
    // No shell prompt: the last output line is the last number.
    let command = cmd(&[
        "bash",
        "-c",
        "for i in $(seq 1 20); do echo num-$i; done; sleep 30",
    ]);
    let pane = driver
        .pane_spawn(&SpawnRequest {
            name: "numbers",
            workspace: Some(&ws),
            cwd: Path::new("/tmp"),
            command: &command,
        })
        .unwrap();

    assert!(eventually(|| {
        driver
            .pane_read(&pane, 3, false)
            .is_ok_and(|out| out.ends_with("num-20"))
    }));

    assert_eq!(
        driver.pane_read(&pane, 3, false).unwrap(),
        "num-18\nnum-19\nnum-20"
    );
}

#[rstest]
#[case::lone(";")]
#[case::word("echo literal;")]
#[case::backslash(r"echo a\;")]
fn should_deliver_text_literally_when_prompt_ends_with_semicolon(
    #[case] text: &str,
) {
    let server = server!();
    let driver = server.driver();
    let ws =
        server.tmux(&["display-message", "-p", "-t", "ws", "#{session_id}"]);
    let command = cmd(&["cat"]);
    let pane = driver
        .pane_spawn(&SpawnRequest {
            name: "cat",
            workspace: Some(&ws),
            cwd: Path::new("/tmp"),
            command: &command,
        })
        .unwrap();

    driver.pane_prompt(&pane, text).unwrap();

    assert!(
        eventually(|| {
            driver
                .pane_read(&pane, 10, false)
                .is_ok_and(|out| out.lines().any(|l| l == text))
        }),
        "no line equal to {text:?}"
    );
}

#[rstest]
#[case::lone(";")]
#[case::word("semi;")]
#[case::backslash(r"a\;")]
fn should_store_label_literally_when_renamed_with_semicolon(
    #[case] label: &str,
) {
    let server = server!();
    let driver = server.driver();
    let pane = first_pane(&server);

    driver.pane_rename(&pane, label).unwrap();

    assert_eq!(
        server.tmux(&["display-message", "-p", "-t", &pane.0, "#{@md-label}"]),
        label
    );
}

#[test]
fn should_fail_harness_unavailable_when_raw_server_gone() {
    let server = server!();

    server.tmux(&["kill-server"]);
    let err = server.driver().raw(&cmd(&["list-sessions"])).unwrap_err();

    assert_eq!(err.kind(), "harness_unavailable");
}

#[test]
fn should_exit_3_when_cli_raw_server_gone() {
    let server = server!();

    server.tmux(&["kill-server"]);
    let output = Command::new(env!("CARGO_BIN_EXE_multiplexer-driver"))
        .args(["--harness", "tmux", "--session", &server.name])
        .args(["raw", "--", "list-sessions"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().last().unwrap_or_default();
    let record: serde_json::Value = serde_json::from_str(last).unwrap();

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(record["error"]["type"], "harness_unavailable");
}
