//! Herdr integration test. One `#[test]` starts its own headless Herdr
//! server in a throwaway session (`md-test-<pid>`), with every HERDR_*
//! variable removed, runs every scenario in order, then stops the server
//! and deletes the session. No other session is touched. Only shells run
//! in panes, never a paid agent.

// Helpers outside `#[test]` fns are not covered by clippy.toml's
// allow-expect-in-tests; a failed setup step must panic with a message.
#![expect(clippy::expect_used, reason = "test helpers panic on failure")]

use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use multiplexer_driver::{
    driver::{Driver, SpawnRequest, SplitRequest},
    herdr::Herdr,
    model::{Confidence, Direction, Handle, Status},
    text::shell_join,
};
use serde_json::Value;

/// A headless Herdr server for this test binary.
#[derive(Debug)]
struct Server {
    /// The `herdr` program to run.
    program: String,
    /// Session name, always `md-test-…`.
    name: String,
    /// Socket file of the session.
    socket: PathBuf,
    /// Server process.
    child: Child,
    /// Whether this test created the session. Only an owned session is
    /// ever stopped or deleted.
    owned: bool,
    /// Workspace created for the scenarios.
    workspace: String,
    /// The workspace's root pane.
    root: Handle,
}

/// Why [`Server::start_with`] gave up.
#[derive(Debug)]
enum StartError {
    /// The `herdr` program does not exist.
    NotInstalled,
    /// The session is taken or the server did not come up.
    Failed(String),
}

/// Directory that holds Herdr's `sessions/` tree.
fn config_dir() -> PathBuf {
    match env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env::var_os("HOME").expect("HOME is set"))
            .join(".config"),
    }
}

/// A `program` command with every `HERDR_*` variable removed.
fn clean_herdr(program: &str) -> Command {
    let mut command = Command::new(program);

    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("HERDR_") {
            command.env_remove(key);
        }
    }

    command
}

impl Server {
    /// Starts the server and creates the test workspace. `None` when
    /// `herdr` is not installed; any other failure panics.
    fn start() -> Option<Self> {
        let name = format!("md-test-{}", std::process::id());

        match Self::start_with("herdr", &config_dir(), &name) {
            Ok(server) => Some(server),
            Err(StartError::NotInstalled) => None,
            Err(StartError::Failed(why)) => panic!("{why}"),
        }
    }

    /// Starts `program --session name server` under the config
    /// directory `config`. Refuses a session that already exists
    /// before spawning anything, and gives up ownership when the
    /// child exits early, so a session this test did not create is
    /// never used, stopped, or deleted.
    fn start_with(
        program: &str,
        config: &Path,
        name: &str,
    ) -> Result<Self, StartError> {
        assert!(name.starts_with("md-test-"));

        let session_dir = config.join("herdr/sessions").join(name);

        if session_dir.exists() {
            return Err(StartError::Failed(format!(
                "session {name} already exists at {}; not touching it",
                session_dir.display()
            )));
        }

        let child = match clean_herdr(program)
            .args(["--session", name, "server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StartError::NotInstalled);
            }
            Err(e) => {
                return Err(StartError::Failed(format!(
                    "starting herdr server: {e}"
                )));
            }
        };
        let mut server = Self {
            program: program.to_string(),
            name: name.to_string(),
            socket: session_dir.join("herdr.sock"),
            child,
            owned: true,
            workspace: String::new(),
            root: Handle(String::new()),
        };

        server.wait_for_socket()?;
        server.create_workspace();

        Ok(server)
    }

    /// Waits up to 5 s for the socket, failing at once if the child
    /// has exited (and then disowning the session).
    fn wait_for_socket(&mut self) -> Result<(), StartError> {
        let deadline = Instant::now() + Duration::from_secs(5);

        while !self.socket.exists() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.owned = false;

                return Err(StartError::Failed(format!(
                    "herdr server exited during startup: {status}"
                )));
            }

            if Instant::now() >= deadline {
                return Err(StartError::Failed("no socket after 5 s".into()));
            }

            thread::sleep(Duration::from_millis(50));
        }

        Ok(())
    }

    /// Creates the workspace the scenarios run in.
    fn create_workspace(&mut self) {
        let created: Value = serde_json::from_str(&self.herdr(&[
            "workspace",
            "create",
            "--label",
            "md-test",
            "--cwd",
            "/tmp",
            "--no-focus",
        ]))
        .expect("workspace create prints JSON");

        self.workspace = created["result"]["workspace"]["workspace_id"]
            .as_str()
            .expect("workspace_id")
            .to_string();
        self.root = Handle(
            created["result"]["root_pane"]["pane_id"]
                .as_str()
                .expect("root pane_id")
                .to_string(),
        );
    }

    /// A driver aimed at this server.
    fn driver(&self) -> Herdr {
        Herdr::new(Some(self.name.clone()))
    }

    /// Runs `herdr args…` against this server only and returns stdout.
    fn herdr(&self, args: &[&str]) -> String {
        let output = self.herdr_output(args);

        assert!(
            output.status.success(),
            "herdr {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Like [`Server::herdr`] but returns the raw output.
    fn herdr_output(&self, args: &[&str]) -> std::process::Output {
        assert!(self.name.starts_with("md-test-"));
        assert!(self.owned, "refusing to command a session we do not own");

        clean_herdr(&self.program)
            .env("HERDR_SOCKET_PATH", &self.socket)
            .args(args)
            .output()
            .expect("running herdr")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        assert!(self.name.starts_with("md-test-"));

        if !self.owned {
            return;
        }

        let _ = self.herdr_output(&["server", "stop"]); // best effort
        let deadline = Instant::now() + Duration::from_secs(5);

        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }

            thread::sleep(Duration::from_millis(50));
        }

        let _ = self.child.kill(); // no-op when already exited
        let _ = self.child.wait(); // reap
        let _ = clean_herdr(&self.program) // session may already be gone
            .args(["session", "delete", &self.name])
            .output();
    }
}

/// Startup must never command a session it did not create: a taken
/// name is refused before anything runs, and a server that exits at
/// startup is never stopped or deleted. A fake `herdr` logs every call.
fn startup_never_touches_foreign_session() {
    let name = format!("md-test-fake-{}", std::process::id());
    let dir = env::temp_dir().join(&name);
    let session = dir.join("herdr/sessions").join(&name);
    let log = dir.join("calls.log");
    let fake = dir.join("fake-herdr");

    fs::create_dir_all(&session).expect("create fake config");
    fs::write(session.join("herdr.sock"), "").expect("fake socket");
    // POSIX-quoted, so a TMPDIR with spaces or quotes stays one word.
    let quoted_log = shell_join(&[log.to_string_lossy().into_owned()]);

    fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" >> {quoted_log}\nexit 1\n"),
    )
    .expect("write fake herdr");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755))
        .expect("chmod fake herdr");

    let program = fake.to_string_lossy();
    let taken = Server::start_with(&program, &dir, &name);

    assert!(matches!(taken, Err(StartError::Failed(_))), "got {taken:?}");
    assert!(!log.exists(), "a command ran against a taken session");

    fs::remove_dir_all(&session).expect("free the session");

    let exited = Server::start_with(&program, &dir, &name);

    assert!(
        matches!(exited, Err(StartError::Failed(_))),
        "got {exited:?}"
    );

    let calls = fs::read_to_string(&log).expect("fake herdr was started");

    assert_eq!(calls.trim(), format!("--session {name} server"));

    fs::remove_dir_all(&dir).expect("remove fake config");
}

/// Announces a passed scenario.
fn ok(name: &str) {
    eprintln!("scenario ok: {name}");
}

/// Runs the built binary with the test-harness environment scrubbed.
fn cli(args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_multiplexer-driver"));

    command
        .args(args)
        .env_remove("MULTIPLEXER_DRIVER_HARNESS")
        .env_remove("MULTIPLEXER_DRIVER_SESSION");

    command.output().expect("running multiplexer-driver")
}

/// The `error.type` on the last stderr line of `output`.
fn last_error_type(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr.lines().last().expect("stderr has a line");
    let record: Value = serde_json::from_str(line).expect("error record");

    record["error"]["type"]
        .as_str()
        .expect("error type")
        .to_string()
}

/// Polls `pane_read` until `check` accepts the output, for up to 5 s.
fn read_until(
    driver: &Herdr,
    pane: &Handle,
    check: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        let out = driver.pane_read(pane, 20, false).expect("pane read");

        if check(&out) || Instant::now() >= deadline {
            return out;
        }

        thread::sleep(Duration::from_millis(100));
    }
}

/// A pane name as a spawn request argument list.
fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// The missing pane `<ws>:p99`.
fn missing_pane(server: &Server) -> Handle {
    Handle(format!("{}:p99", server.workspace))
}

/// Spawning a raw command runs it and its output can be read.
fn should_spawn_raw_command_and_read_output(server: &Server) {
    let driver = server.driver();
    let command = argv(&["bash", "-c", "echo hello-$((40+2)); exec bash"]);
    let handle = driver
        .pane_spawn(&SpawnRequest {
            name: "raw-1",
            workspace: Some(&server.workspace),
            cwd: Path::new("/tmp"),
            command: &command,
        })
        .expect("pane spawn");
    let out =
        read_until(&driver, &handle, |o| o.lines().any(|l| l == "hello-42"));

    assert!(
        out.lines().any(|l| l == "hello-42"),
        "no hello-42 line in {out:?}"
    );
}

/// Killing a split pane leaves the pane it was split from.
fn should_split_and_kill_only_new_pane(server: &Server) {
    let driver = server.driver();
    let command = argv(&["bash"]);
    let new = driver
        .pane_split(&SplitRequest {
            target: &server.root,
            direction: Direction::Right,
            name: "split-1",
            cwd: Path::new("/tmp"),
            command: &command,
        })
        .expect("pane split");

    assert_ne!(new, server.root);

    driver.pane_kill(&new).expect("pane kill");
    server.herdr(&["pane", "get", &server.root.0]);
}

/// Renaming changes the label and not the handle.
fn should_keep_handle_when_renamed(server: &Server) {
    server
        .driver()
        .pane_rename(&server.root, "renamed")
        .expect("pane rename");

    let got: Value =
        serde_json::from_str(&server.herdr(&["pane", "get", &server.root.0]))
            .expect("pane get JSON");

    assert_eq!(got["result"]["pane"]["label"], "renamed");
}

/// A pane without an agent reports `unknown` with native confidence.
fn should_report_native_unknown_when_no_agent(server: &Server) {
    let result = server
        .driver()
        .pane_status(&server.root)
        .expect("pane status");

    assert_eq!(result.status, Status::Unknown);
    assert_eq!(result.confidence, Confidence::Native);
    assert_eq!(result.tail, None);
}

/// Prompting a pane with no agent is `not_found`.
fn should_report_not_found_when_prompting_non_agent(server: &Server) {
    let err = server
        .driver()
        .pane_prompt(&server.root, "hi")
        .expect_err("prompt must fail");

    assert_eq!(err.kind(), "not_found", "got {err:?}");
}

/// Reading a pane that does not exist is `not_found`.
fn should_report_not_found_when_pane_missing(server: &Server) {
    let err = server
        .driver()
        .pane_read(&missing_pane(server), 5, false)
        .expect_err("read must fail");

    assert_eq!(err.kind(), "not_found", "got {err:?}");
}

/// A session with no socket is `harness_unavailable`.
fn should_fail_unavailable_when_session_missing(server: &Server) {
    let err = Herdr::new(Some("md-test-absent".into()))
        .pane_read(&server.root, 5, false)
        .expect_err("read must fail");

    assert_eq!(err.kind(), "harness_unavailable", "got {err:?}");
}

/// `raw` hands its arguments to herdr unchanged.
fn should_pass_args_through_when_raw(server: &Server) {
    let out = server
        .driver()
        .raw(&argv(&["tab", "list", "--workspace", &server.workspace]))
        .expect("raw");

    assert!(out.contains("\"tab_list\""), "got {out:?}");
}

/// The driver talks to the test session, not the caller's own: the
/// workspaces it lists are exactly the ones this run created, all
/// labelled `md-test…`, which no ambient session would hold.
fn should_route_only_to_test_session(server: &Server) {
    let out = server
        .driver()
        .raw(&argv(&["workspace", "list"]))
        .expect("raw workspace list");
    let listed: Value = serde_json::from_str(&out).expect("workspace list");
    let labels: Vec<&str> = listed["result"]["workspaces"]
        .as_array()
        .expect("workspaces array")
        .iter()
        .map(|w| w["label"].as_str().expect("label"))
        .collect();

    assert!(labels.contains(&"md-test"), "labels {labels:?}");
    assert!(
        labels.iter().all(|l| l.starts_with("md-test")),
        "foreign workspace in {labels:?}"
    );
}

/// A failing raw command is `unexpected`, never `not_found`.
fn should_fail_unexpected_when_raw_command_fails(server: &Server) {
    let missing = missing_pane(server).0;
    let err = server
        .driver()
        .raw(&argv(&["pane", "get", &missing]))
        .expect_err("raw must fail");

    assert_eq!(err.kind(), "unexpected", "got {err:?}");
}

/// The binary exits 3 for a session with no socket.
fn should_exit_3_when_cli_session_missing() {
    let session = format!("md-test-absent-{}", std::process::id());
    let output = cli(&[
        "--harness",
        "herdr",
        "--session",
        &session,
        "pane",
        "read",
        "w1:p1",
    ]);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(last_error_type(&output), "harness_unavailable");
}

/// The binary exits 4 when reading a missing pane.
fn should_exit_4_when_cli_reads_missing_pane(server: &Server) {
    let missing = missing_pane(server).0;
    let output = cli(&[
        "--harness",
        "herdr",
        "--session",
        &server.name,
        "pane",
        "read",
        &missing,
    ]);

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(last_error_type(&output), "not_found");
}

/// Runs every scenario, in order, against one throwaway server.
#[test]
fn herdr_driver_scenarios() {
    startup_never_touches_foreign_session();
    eprintln!("precheck ok: startup_never_touches_foreign_session");

    let Some(server) = Server::start() else {
        eprintln!("herdr not installed; skipping");
        return;
    };

    should_spawn_raw_command_and_read_output(&server);
    ok("should_spawn_raw_command_and_read_output");
    should_split_and_kill_only_new_pane(&server);
    ok("should_split_and_kill_only_new_pane");
    should_keep_handle_when_renamed(&server);
    ok("should_keep_handle_when_renamed");
    should_report_native_unknown_when_no_agent(&server);
    ok("should_report_native_unknown_when_no_agent");
    should_report_not_found_when_prompting_non_agent(&server);
    ok("should_report_not_found_when_prompting_non_agent");
    should_report_not_found_when_pane_missing(&server);
    ok("should_report_not_found_when_pane_missing");
    should_fail_unavailable_when_session_missing(&server);
    ok("should_fail_unavailable_when_session_missing");
    should_pass_args_through_when_raw(&server);
    ok("should_pass_args_through_when_raw");
    should_route_only_to_test_session(&server);
    ok("should_route_only_to_test_session");
    should_fail_unexpected_when_raw_command_fails(&server);
    ok("should_fail_unexpected_when_raw_command_fails");
    should_exit_3_when_cli_session_missing();
    ok("should_exit_3_when_cli_session_missing");
    should_exit_4_when_cli_reads_missing_pane(&server);
    ok("should_exit_4_when_cli_reads_missing_pane");
}
