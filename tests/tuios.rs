//! tuios integration test. One `#[test]` creates a private directory
//! `/tmp/tp.XXXXXX`, points `XDG_RUNTIME_DIR` and `XDG_STATE_HOME` at it
//! for this process and every child, starts a headless tuios daemon there
//! with two sessions (the test's own and `foreign`), runs every scenario
//! in order, then kills the daemon and deletes the directory. The user's
//! own daemon is never reached. Only shells run in panes, never a paid
//! agent.

#![cfg(unix)]
// Helpers outside `#[test]` fns are not covered by clippy.toml's
// allow-expect-in-tests; a failed setup step must panic with a message.
#![expect(clippy::expect_used, reason = "test helpers panic on failure")]

use std::{
    cell::RefCell,
    env, fs, io,
    os::unix::fs::PermissionsExt,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use multiplexer_driver::{
    driver::{Driver, SpawnRequest, SplitRequest, wait_for},
    model::{Confidence, Direction, Handle, Status},
    tuios::Tuios,
};
use serde_json::Value;

/// The name of the second session the guard keeps watching.
const FOREIGN: &str = "foreign";

/// A window that exists in no session.
const MISSING_WINDOW: &str = "00000000-0000-0000-0000-000000000000";

/// The private daemon of this test binary. Dropping it kills the daemon
/// and the attached client, then deletes the directory.
#[derive(Debug)]
struct Server {
    /// `/tmp/tp.XXXXXX`, the runtime and state directory of the daemon.
    dir: PathBuf,
    /// The test's own session, `md-test-<pid>`.
    session: String,
    /// The window the session starts with.
    root: Handle,
    /// Window ids of the `foreign` session, taken at startup.
    foreign_windows: Vec<String>,
    /// The attached client, once one is started.
    client: RefCell<Option<Child>>,
}

/// A `program` command that runs under the private directory `dir`, with
/// every `TUIOS_*` variable removed.
fn private_command(program: impl AsRef<Path>, dir: &Path) -> Command {
    let mut command = Command::new(program.as_ref());

    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("TUIOS_") {
            command.env_remove(key);
        }
    }

    command
        .env("XDG_RUNTIME_DIR", dir)
        .env("XDG_STATE_HOME", dir)
        .stdin(Stdio::null());

    command
}

/// Creates a fresh `/tmp/tp.XXXXXX` directory, mode 0700. The path stays
/// short because unix socket paths are limited in length.
fn make_private_dir() -> PathBuf {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

    let mut seed = u64::from(std::process::id())
        ^ u64::from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .subsec_nanos(),
        ) << 16;

    loop {
        let suffix: String = (0..6)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);

                char::from(ALPHABET[(seed >> 33) as usize % ALPHABET.len()])
            })
            .collect();
        let dir = PathBuf::from(format!("/tmp/tp.{suffix}"));

        match fs::create_dir(&dir) {
            Ok(()) => {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                    .expect("chmod the private directory");

                return dir;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => panic!("creating {}: {e}", dir.display()),
        }
    }
}

impl Server {
    /// Creates the private directory and starts the daemon with both
    /// sessions. `None` when `tuios` is not installed; any other failure
    /// panics. The directory is announced before anything else happens,
    /// and the cleanup guard exists before any tuios process does.
    fn start() -> Option<Self> {
        let dir = make_private_dir();

        println!("tuios-test-dir: {}", dir.display());

        let mut server = Self {
            dir,
            session: format!("md-test-{}", std::process::id()),
            root: Handle(String::new()),
            foreign_windows: Vec::new(),
            client: RefCell::new(None),
        };

        // SAFETY: nothing else in this process reads or writes the
        // environment concurrently: the test harness runs this one
        // `#[test]`, and no scenario thread exists yet.
        unsafe {
            for (key, _) in env::vars_os() {
                if key.to_string_lossy().starts_with("TUIOS_") {
                    env::remove_var(key);
                }
            }

            env::set_var("XDG_RUNTIME_DIR", &server.dir);
            env::set_var("XDG_STATE_HOME", &server.dir);
        }

        match private_command("tuios", &server.dir)
            .arg("--version")
            .output()
        {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => panic!("running tuios --version: {e}"),
        }

        server.tuios(&["new", "--detach", "--cwd", "/tmp", &server.session]);
        server.tuios(&["new", "--detach", "--cwd", "/tmp", FOREIGN]);
        server.root = Handle(server.window_ids(&server.session).remove(0));
        server.foreign_windows = server.window_ids(FOREIGN);

        Some(server)
    }

    /// Runs `tuios args…` under the private directory and returns the raw
    /// output.
    fn tuios_output(&self, args: &[&str]) -> Output {
        private_command("tuios", &self.dir)
            .args(args)
            .output()
            .expect("running tuios")
    }

    /// Like [`Server::tuios_output`] but asserts success and returns
    /// stdout.
    fn tuios(&self, args: &[&str]) -> String {
        let output = self.tuios_output(args);

        assert!(
            output.status.success(),
            "tuios {args:?} failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A driver aimed at the test session. It reaches the private daemon
    /// through the environment this process exported at startup.
    fn driver(&self) -> Tuios {
        Tuios::new(Some(self.session.clone()), None)
    }

    /// The window ids of `session`, in tuios order.
    fn window_ids(&self, session: &str) -> Vec<String> {
        let listed: Value = serde_json::from_str(&self.tuios(&[
            "list-windows",
            "-s",
            session,
            "--json",
        ]))
        .expect("list-windows prints JSON");

        listed["windows"]
            .as_array()
            .expect("windows array")
            .iter()
            .map(|w| w["window_id"].as_str().expect("window_id").to_string())
            .collect()
    }

    /// Every session name the daemon lists, sorted.
    fn session_names(&self) -> Vec<String> {
        let listed: Value =
            serde_json::from_str(&self.tuios(&["ls", "--json"]))
                .expect("ls prints JSON");
        let mut names: Vec<String> = listed
            .as_array()
            .expect("sessions array")
            .iter()
            .map(|s| s["name"].as_str().expect("name").to_string())
            .collect();

        names.sort();

        names
    }

    /// Whether a client is attached to the test session.
    fn is_attached(&self) -> bool {
        let listed: Value =
            serde_json::from_str(&self.tuios(&["ls", "--json"]))
                .expect("ls prints JSON");

        listed.as_array().expect("sessions array").iter().any(|s| {
            s["name"] == self.session.as_str() && s["attached"] == true
        })
    }

    /// Attaches a client to the test session through `script`, which
    /// gives `tuios attach` the terminal it needs, and waits until the
    /// daemon reports it. Does nothing when one is attached already.
    fn attach_client(&self) {
        if self.client.borrow().is_some() {
            return;
        }

        let child = private_command("script", &self.dir)
            .args([
                "-qec",
                &format!("tuios attach {}", self.session),
                "/dev/null",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting script for the attached client");

        *self.client.borrow_mut() = Some(child);

        let deadline = Instant::now() + Duration::from_secs(5);

        while !self.is_attached() {
            assert!(
                Instant::now() < deadline,
                "script gave no working client: the session never showed \
                 attached"
            );

            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        assert!(
            self.dir.to_string_lossy().starts_with("/tmp/tp."),
            "refusing to clean {}",
            self.dir.display()
        );

        if let Some(mut client) = self.client.borrow_mut().take() {
            let _ = client.kill(); // best effort; may have exited
            let _ = client.wait(); // reap
        }

        // Best effort: nothing runs when `tuios` is missing or the daemon
        // never started.
        let _ = private_command("tuios", &self.dir)
            .arg("kill-server")
            .output();

        // The daemon saves its state on the way out and may recreate files
        // after the first removal, so remove until the directory is gone.
        for _ in 0..30 {
            let _ = fs::remove_dir_all(&self.dir); // retried below

            if !self.dir.exists() {
                return;
            }

            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// A command line as a spawn request argument list.
fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// Polls `pane_read` until `check` accepts the output, for up to 5 s.
fn read_until(
    driver: &Tuios,
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

/// Whether `out` has a line that is exactly `line`.
fn has_line(out: &str, line: &str) -> bool {
    out.lines().any(|l| l == line)
}

/// Spawns a window named `name` in `/tmp` that runs `command`.
fn spawn(driver: &Tuios, name: &str, command: &[&str]) -> Handle {
    driver
        .pane_spawn(&SpawnRequest {
            name,
            workspace: None,
            cwd: Path::new("/tmp"),
            command: &argv(command),
        })
        .expect("pane spawn")
}

/// Spawns a plain shell window named `name`.
fn spawn_shell(driver: &Tuios, name: &str) -> Handle {
    spawn(driver, name, &[])
}

/// Spawns a window that prints `READY-42` and then runs `sleep 300`.
fn spawn_sleeper(driver: &Tuios, name: &str) -> Handle {
    spawn(
        driver,
        name,
        &["bash", "-c", "echo READY-$((40+2)); exec sleep 300"],
    )
}

/// Splits `target` to the right into a window named `name` that runs
/// `command`.
fn split(
    driver: &Tuios,
    target: &Handle,
    name: &str,
    command: &[&str],
) -> multiplexer_driver::driver::Result<Handle> {
    driver.pane_split(&SplitRequest {
        target,
        direction: Direction::Right,
        name,
        cwd: Path::new("/tmp"),
        command: &argv(command),
    })
}

/// Sets the agent state of window `pane` as a program in it would.
fn set_state(server: &Server, pane: &Handle, state: &str) {
    server.tuios(&[
        "set-agent-state",
        state,
        "-s",
        &server.session,
        "-w",
        &pane.0,
    ]);
}

/// Closes the window `pane`.
fn close(driver: &Tuios, pane: &Handle) {
    driver.pane_kill(pane).expect("pane kill");
}

/// Spawning a command runs it and its output can be read.
fn should_run_command_and_read_output_when_spawned(server: &Server) {
    let driver = server.driver();
    let pane = spawn(
        &driver,
        "spawn-1",
        &["bash", "-c", "echo hello-$((40+2)); exec bash"],
    );
    let out = read_until(&driver, &pane, |o| has_line(o, "hello-42"));

    assert!(has_line(&out, "hello-42"), "no hello-42 line in {out:?}");

    close(&driver, &pane);
}

/// Reading with `ansi` keeps the escape code the program printed.
fn should_keep_escape_code_when_read_with_ansi(server: &Server) {
    let driver = server.driver();
    let pane = spawn(
        &driver,
        "ansi-1",
        &[
            "bash",
            "-c",
            "printf '\\033[31mRED-%s\\033[0m\\n' MARK; exec bash",
        ],
    );

    read_until(&driver, &pane, |o| has_line(o, "RED-MARK"));

    let out = driver.pane_read(&pane, 20, true).expect("pane read");

    assert!(out.contains('\u{1b}'), "no escape code in {out:?}");

    close(&driver, &pane);
}

/// Reading without `ansi` leaves no escape code in the text.
fn should_strip_escape_code_when_read_without_ansi(server: &Server) {
    let driver = server.driver();
    let pane = spawn(
        &driver,
        "ansi-2",
        &[
            "bash",
            "-c",
            "printf '\\033[31mRED-%s\\033[0m\\n' MARK; exec bash",
        ],
    );
    let out = read_until(&driver, &pane, |o| has_line(o, "RED-MARK"));

    assert!(out.contains("RED-MARK"), "no marker in {out:?}");
    assert!(!out.contains('\u{1b}'), "escape code in {out:?}");

    close(&driver, &pane);
}

/// A prompt typed into a shell runs, and its result can be read.
fn should_run_line_when_shell_is_prompted(server: &Server) {
    let driver = server.driver();
    let pane = spawn_shell(&driver, "prompt-1");

    driver
        .pane_prompt(&pane, "echo PROMPT-$((40+2))")
        .expect("pane prompt");

    let out = read_until(&driver, &pane, |o| has_line(o, "PROMPT-42"));

    assert!(has_line(&out, "PROMPT-42"), "no PROMPT-42 line in {out:?}");

    close(&driver, &pane);
}

/// Renaming changes the label and not the handle.
fn should_keep_handle_when_renamed(server: &Server) {
    let driver = server.driver();
    let pane = spawn_shell(&driver, "rename-1");

    driver.pane_rename(&pane, "renamed").expect("pane rename");

    let panes = driver.pane_list(None).expect("pane list");
    let found = panes
        .iter()
        .find(|p| p.handle == pane)
        .expect("the handle is still listed");

    assert_eq!(found.label.as_deref(), Some("renamed"));

    close(&driver, &pane);
}

/// An interrupt stops a running `sleep`, so the shell takes the next
/// line at once.
fn should_stop_sleep_when_interrupted(server: &Server) {
    let driver = server.driver();
    let pane = spawn_sleeper(&driver, "interrupt-1");

    read_until(&driver, &pane, |o| has_line(o, "READY-42"));
    driver.pane_interrupt(&pane).expect("pane interrupt");
    driver
        .pane_prompt(&pane, "echo AFTER-$((40+2))")
        .expect("pane prompt");

    let out = read_until(&driver, &pane, |o| has_line(o, "AFTER-42"));

    assert!(has_line(&out, "AFTER-42"), "sleep still runs: {out:?}");

    close(&driver, &pane);
}

/// Killing a pane removes that pane and leaves its sibling.
fn should_remove_only_that_pane_when_killed(server: &Server) {
    let driver = server.driver();
    let doomed = spawn_shell(&driver, "kill-1");
    let sibling = spawn_shell(&driver, "kill-2");

    driver.pane_kill(&doomed).expect("pane kill");

    let handles: Vec<Handle> = driver
        .pane_list(None)
        .expect("pane list")
        .into_iter()
        .map(|p| p.handle)
        .collect();

    assert!(!handles.contains(&doomed), "still listed: {handles:?}");
    assert!(handles.contains(&sibling), "sibling lost: {handles:?}");

    close(&driver, &sibling);
}

/// The listing shows the label, the directory, and the workspace.
fn should_list_label_cwd_and_workspace_when_pane_exists(server: &Server) {
    let driver = server.driver();
    let pane = spawn_shell(&driver, "list-1");
    let panes = driver.pane_list(None).expect("pane list");
    let found = panes
        .iter()
        .find(|p| p.handle == pane)
        .expect("the pane is listed");

    assert_eq!(found.label.as_deref(), Some("list-1"));
    assert_eq!(found.cwd.as_deref(), Some("/tmp"));
    assert_eq!(found.workspace_id, "1");

    close(&driver, &pane);
}

/// A read of 60 lines after 65 numbered lines reaches back into the
/// scrollback: `LINE-030` is above the 24-row viewport.
fn should_return_scrollback_when_read_with_lines(server: &Server) {
    let driver = server.driver();
    let pane = spawn_shell(&driver, "scroll-1");

    driver
        .pane_prompt(
            &pane,
            "for i in $(seq 1 65); do printf 'LINE-%03d\\n' $i; done",
        )
        .expect("pane prompt");
    read_until(&driver, &pane, |o| has_line(o, "LINE-065"));

    let out = driver.pane_read(&pane, 60, false).expect("pane read");

    assert!(out.contains("LINE-065"), "no LINE-065 in {out:?}");
    assert!(out.contains("LINE-030"), "no LINE-030 in {out:?}");
    assert!(out.lines().count() <= 60, "{} lines", out.lines().count());

    close(&driver, &pane);
}

/// A shell reports no agent state: `unknown` with no confidence.
fn should_report_unknown_when_pane_is_a_shell(server: &Server) {
    let driver = server.driver();
    let pane = spawn_shell(&driver, "status-1");
    let result = driver.pane_status(&pane).expect("pane status");

    assert_eq!(result.status, Status::Unknown);
    assert_eq!(result.confidence, Confidence::None);

    close(&driver, &pane);
}

/// `needs_input` is `blocked`, natively.
fn should_report_blocked_when_agent_needs_input(server: &Server) {
    let driver = server.driver();
    let pane = spawn_shell(&driver, "status-2");

    set_state(server, &pane, "needs_input");

    let result = driver.pane_status(&pane).expect("pane status");

    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.confidence, Confidence::Native);

    close(&driver, &pane);
}

/// `working` is `working`, natively.
fn should_report_working_when_agent_works(server: &Server) {
    let driver = server.driver();
    let pane = spawn_shell(&driver, "status-3");

    set_state(server, &pane, "working");

    let result = driver.pane_status(&pane).expect("pane status");

    assert_eq!(result.status, Status::Working);
    assert_eq!(result.confidence, Confidence::Native);

    close(&driver, &pane);
}

/// `idle` is `idle`, natively.
fn should_report_idle_when_agent_is_idle(server: &Server) {
    let driver = server.driver();
    let pane = spawn_shell(&driver, "status-4");

    set_state(server, &pane, "idle");

    let result = driver.pane_status(&pane).expect("pane status");

    assert_eq!(result.status, Status::Idle);
    assert_eq!(result.confidence, Confidence::Native);

    close(&driver, &pane);
}

/// A wait returns when a helper thread changes the state to `idle`.
fn should_return_when_state_changes_during_wait(server: &Server) {
    let driver = server.driver();
    let pane = spawn_shell(&driver, "wait-1");

    set_state(server, &pane, "working");

    let helper = {
        let dir = server.dir.clone();
        let args = argv(&[
            "set-agent-state",
            "idle",
            "-s",
            &server.session,
            "-w",
            &pane.0,
        ]);

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(1000));

            private_command("tuios", &dir)
                .args(args)
                .output()
                .expect("running tuios in the helper")
        })
    };
    let started = Instant::now();
    let result = wait_for(
        &driver,
        &pane,
        &[Status::Idle],
        Duration::from_secs(10),
        Duration::from_millis(100),
    );

    assert!(helper.join().expect("helper thread").status.success());
    assert!(
        matches!(&result, Ok(r) if r.status == Status::Idle),
        "wait ended with {result:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(800));

    close(&driver, &pane);
}

/// Reading a window that does not exist is `not_found`.
fn should_report_not_found_when_pane_missing(server: &Server) {
    let err = server
        .driver()
        .pane_read(&Handle(MISSING_WINDOW.into()), 5, false)
        .expect_err("read must fail");

    assert_eq!(err.kind(), "not_found", "got {err:?}");
}

/// A session that does not exist is `harness_unavailable`.
fn should_report_unavailable_when_session_missing(server: &Server) {
    let err = Tuios::new(Some("md-test-absent".into()), None)
        .pane_read(&server.root, 5, false)
        .expect_err("read must fail");

    assert_eq!(err.kind(), "harness_unavailable", "got {err:?}");
}

/// The workspace list shows workspace 1, focused.
fn should_list_workspace_one_focused_when_session_is_new(server: &Server) {
    let listed = server.driver().workspace_list().expect("workspace list");
    let first = listed
        .iter()
        .find(|w| w.workspace_id == "1")
        .expect("workspace 1 is listed");

    assert!(first.focused, "got {first:?}");
}

/// Workspaces are fixed slots, so none can be created.
fn should_refuse_workspace_create_when_slots_are_fixed(server: &Server) {
    let err = server
        .driver()
        .workspace_create("x", Path::new("/tmp"))
        .expect_err("create must fail");

    assert_eq!(err.kind(), "unsupported", "got {err:?}");
}

/// Workspaces carry no tags.
fn should_refuse_workspace_tag_when_tuios_has_no_tags(server: &Server) {
    let err = server
        .driver()
        .workspace_tag("1", &[("k".into(), "v".into())], &[])
        .expect_err("tag must fail");

    assert_eq!(err.kind(), "unsupported", "got {err:?}");
}

/// A spawn that names a workspace is a usage error, and it opens
/// nothing.
fn should_refuse_spawn_when_workspace_given(server: &Server) {
    let before = server.window_ids(&server.session);
    let err = server
        .driver()
        .pane_spawn(&SpawnRequest {
            name: "ws-1",
            workspace: Some("2"),
            cwd: Path::new("/tmp"),
            command: &[],
        })
        .expect_err("spawn must fail");

    assert_eq!(err.kind(), "usage", "got {err:?}");
    assert_eq!(server.window_ids(&server.session), before);
}

/// Without an attached client the split is refused as `unsupported`,
/// carrying tuios's own text.
fn should_refuse_split_with_tuios_text_when_session_is_headless(
    server: &Server,
) {
    assert!(!server.is_attached(), "a client is attached already");

    let err = split(&server.driver(), &server.root, "split-0", &[])
        .expect_err("split must fail");

    assert_eq!(err.kind(), "unsupported", "got {err:?}");
    assert!(err.to_string().contains("attached client"), "got {err:?}");
}

/// A split with an attached client types the command: the new pane
/// shows `SPLIT-42`, which the typed line `echo SPLIT-$((40+2))` lacks.
fn should_type_command_when_split_has_attached_client(server: &Server) {
    server.attach_client();

    let driver = server.driver();
    let new = split(
        &driver,
        &server.root,
        "split-1",
        &["bash", "-c", "echo SPLIT-$((40+2)); exec bash"],
    )
    .expect("pane split");

    assert_ne!(new, server.root);

    let out = read_until(&driver, &new, |o| has_line(o, "SPLIT-42"));

    assert!(has_line(&out, "SPLIT-42"), "no SPLIT-42 line in {out:?}");

    close(&driver, &new);
}

/// A split with an empty command leaves a working shell.
fn should_give_shell_when_split_command_is_empty(server: &Server) {
    server.attach_client();

    let driver = server.driver();
    let new =
        split(&driver, &server.root, "split-2", &[]).expect("pane split");

    let sent = driver.pane_prompt(&new, "echo SHELL-$((40+2))");
    let out = read_until(&driver, &new, |o| has_line(o, "SHELL-42"));

    assert!(
        has_line(&out, "SHELL-42"),
        "no SHELL-42 line in {out:?} (prompt: {sent:?})"
    );

    close(&driver, &new);
}

/// A refusal because tiling is off is `unsupported` with tuios's text,
/// and the driver does not try to enable tiling. A fake `tuios` first on
/// `PATH` plays the daemon and logs every call.
fn should_carry_text_as_unsupported_when_tiling_is_off(server: &Server) {
    let bin = server.dir.join("fake-bin");
    let log = server.dir.join("fake-calls.log");
    let refusal = include_str!("fixtures/tuios/err_split_no_tiling.json");

    fs::create_dir_all(&bin).expect("create the fake bin directory");
    fs::write(bin.join("refusal.json"), refusal).expect("write the refusal");
    fs::write(
        bin.join("tuios"),
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\ncat '{}'\nexit 1\n",
            log.display(),
            bin.join("refusal.json").display()
        ),
    )
    .expect("write the fake tuios");
    fs::set_permissions(bin.join("tuios"), fs::Permissions::from_mode(0o755))
        .expect("chmod the fake tuios");

    let path = env::var_os("PATH").expect("PATH is set");
    let fake_path = env::join_paths(
        [bin.clone()].into_iter().chain(env::split_paths(&path)),
    )
    .expect("join PATH");

    // SAFETY: no other thread of this process exists at this point; the
    // wait scenario joined its helper.
    unsafe { env::set_var("PATH", &fake_path) };

    let result = split(&server.driver(), &server.root, "split-3", &[]);

    // SAFETY: as above.
    unsafe { env::set_var("PATH", &path) };

    let err = result.expect_err("split must fail");
    let calls = fs::read_to_string(&log).expect("the fake tuios was run");

    assert_eq!(err.kind(), "unsupported", "got {err:?}");
    assert!(
        err.to_string().contains(
            "tiling is not enabled; run EnableTiling before this command."
        ),
        "got {err:?}"
    );
    assert_eq!(calls.lines().count(), 1, "calls: {calls}");
    assert!(calls.contains("split-window"), "calls: {calls}");
}

/// `raw` hands its arguments to tuios unchanged, session included.
fn should_pass_args_through_when_raw(server: &Server) {
    let out = server
        .driver()
        .raw(&argv(&["list-windows", "-s", &server.session, "--json"]))
        .expect("raw");
    let listed: Value = serde_json::from_str(&out).expect("JSON");

    assert_eq!(listed["windows"][0]["window_id"], server.root.0.as_str());
}

/// A failing `raw` command is `unexpected`, never `not_found`.
fn should_fail_unexpected_when_raw_command_fails(server: &Server) {
    let err = server
        .driver()
        .raw(&argv(&[
            "capture-pane",
            "-s",
            &server.session,
            "-w",
            MISSING_WINDOW,
            "--json",
        ]))
        .expect_err("raw must fail");

    assert_eq!(err.kind(), "unexpected", "got {err:?}");
}

/// Guard: through the driver's own children, with the private
/// environment, the daemon lists exactly this test's two sessions. The
/// user's daemon would list others.
fn should_list_only_test_sessions_when_env_is_private(server: &Server) {
    let result = server.driver().raw(&argv(&["ls", "--json"]));

    assert!(
        result.is_ok(),
        "the driver cannot reach the daemon: {result:?}"
    );

    let out = result.unwrap_or_default();
    let listed: Value = serde_json::from_str(&out).expect("ls JSON");
    let mut names: Vec<&str> = listed
        .as_array()
        .expect("sessions array")
        .iter()
        .map(|s| s["name"].as_str().expect("name"))
        .collect();

    names.sort_unstable();

    let mut expected = vec![server.session.as_str(), FOREIGN];

    expected.sort_unstable();

    assert_eq!(names, expected);
    assert_eq!(server.session_names().len(), 2);
}

/// A scenario that fails the foreign-session check names it.
fn foreign_unchanged(server: &Server) -> Result<(), String> {
    let now = server.window_ids(FOREIGN);

    if now == server.foreign_windows {
        return Ok(());
    }

    Err(format!(
        "the foreign session changed: {:?} -> {now:?}",
        server.foreign_windows
    ))
}

/// Extracts the text of a caught panic.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_else(|| "panic without text".into())
}

/// Pairs a scenario function with its name.
macro_rules! scenarios {
    ($($name:ident),+ $(,)?) => {
        [$((stringify!($name), $name as fn(&Server))),+]
    };
}

/// Runs every scenario, in order, against one private daemon. A failed
/// scenario does not stop the others; all failures are reported at the
/// end. After every scenario the `foreign` session must be unchanged.
#[test]
fn tuios_driver_scenarios() {
    let Some(server) = Server::start() else {
        println!("tuios not installed; skipping");
        return;
    };
    let mut failures = Vec::new();

    for (name, scenario) in scenarios![
        should_list_only_test_sessions_when_env_is_private,
        should_run_command_and_read_output_when_spawned,
        should_keep_escape_code_when_read_with_ansi,
        should_strip_escape_code_when_read_without_ansi,
        should_run_line_when_shell_is_prompted,
        should_keep_handle_when_renamed,
        should_stop_sleep_when_interrupted,
        should_remove_only_that_pane_when_killed,
        should_list_label_cwd_and_workspace_when_pane_exists,
        should_return_scrollback_when_read_with_lines,
        should_report_unknown_when_pane_is_a_shell,
        should_report_blocked_when_agent_needs_input,
        should_report_working_when_agent_works,
        should_report_idle_when_agent_is_idle,
        should_return_when_state_changes_during_wait,
        should_report_not_found_when_pane_missing,
        should_report_unavailable_when_session_missing,
        should_list_workspace_one_focused_when_session_is_new,
        should_refuse_workspace_create_when_slots_are_fixed,
        should_refuse_workspace_tag_when_tuios_has_no_tags,
        should_refuse_spawn_when_workspace_given,
        should_refuse_split_with_tuios_text_when_session_is_headless,
        should_type_command_when_split_has_attached_client,
        should_give_shell_when_split_command_is_empty,
        should_carry_text_as_unsupported_when_tiling_is_off,
        should_pass_args_through_when_raw,
        should_fail_unexpected_when_raw_command_fails,
    ] {
        let caught =
            panic::catch_unwind(AssertUnwindSafe(|| scenario(&server)));
        let guard = foreign_unchanged(&server);

        match (caught, guard) {
            (Ok(()), Ok(())) => eprintln!("scenario ok: {name}"),
            (Err(payload), _) => {
                let text = panic_text(payload.as_ref());

                eprintln!("scenario FAILED: {name}: {text}");
                failures.push(format!("{name}: {text}"));
            }
            (Ok(()), Err(text)) => {
                eprintln!("scenario FAILED: {name}: {text}");
                failures.push(format!("{name}: {text}"));
            }
        }
    }

    assert!(failures.is_empty(), "failed scenarios: {failures:#?}");
}
