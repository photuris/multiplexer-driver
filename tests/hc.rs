//! hc integration test. One `#[test]` creates a private directory
//! `/tmp/hc.XXXXXX`, points `XDG_RUNTIME_DIR`, `XDG_STATE_HOME`, and
//! `XDG_CONFIG_HOME` at it for this process and every child, removes every
//! `HC_*` variable, starts two private herdr cores there (the one under
//! test and a sentinel), runs every scenario in order, then stops both
//! cores and deletes the directory. The user's own core (session
//! `default`) is never reached. Only `bash`, `sh`, `sleep`, `printf`, and
//! `seq` run in terminals, never a paid agent.

#![cfg(unix)]
// Helpers outside `#[test]` fns are not covered by clippy.toml's
// allow-expect-in-tests; a failed setup step must panic with a message.
#![expect(clippy::expect_used, reason = "test helpers panic on failure")]

use std::{
    env,
    ffi::OsString,
    fs, io,
    os::unix::fs::PermissionsExt,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use multiplexer_driver::{
    driver::{Driver, Error, Result, SpawnRequest, SplitRequest, wait_for},
    hc::Hc,
    model::{Confidence, Direction, Handle, Status, StatusResult},
    patterns::Patterns,
};
use serde_json::Value;

/// An address that exists in no core.
const MISSING: &str = "t999999";

/// The two private cores of this test binary. Dropping it stops both
/// cores and deletes the directory.
#[derive(Debug)]
struct Cores {
    /// `/tmp/hc.XXXXXX`, the runtime, state, and config directory.
    dir: PathBuf,
    /// The core under test, `md-test-<pid>`.
    a: String,
    /// The sentinel core, `md-test-<pid>-b`.
    b: String,
    /// Address in `b` of the sentinel terminal.
    s_b: String,
    /// Sessions this test started; only these are ever stopped.
    started: Vec<String>,
}

/// An `hc` command for `session`: the inherited environment (already
/// private) without any `HC_*` variable, plus `HC_SESSION`.
fn hc_cmd(session: &str) -> Command {
    let mut command = Command::new("hc");

    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("HC_") {
            command.env_remove(key);
        }
    }

    command.env("HC_SESSION", session).stdin(Stdio::null());

    command
}

/// Runs `hc args…` in `session` and returns the raw output.
fn hc_output(session: &str, args: &[&str]) -> Output {
    hc_cmd(session).args(args).output().expect("running hc")
}

/// Like [`hc_output`] but asserts success and returns stdout.
fn hc(session: &str, args: &[&str]) -> String {
    let output = hc_output(session, args);

    assert!(
        output.status.success(),
        "hc {args:?} in {session} failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Runs `hc args…` in `session` and parses its stdout as JSON.
fn hc_json(session: &str, args: &[&str]) -> Value {
    serde_json::from_str(&hc(session, args)).expect("hc prints JSON")
}

/// The terminals `hc ls --json` lists in `session`; with `all`, finished
/// ones too.
fn terminals(session: &str, all: bool) -> Vec<Value> {
    let args: &[&str] = if all {
        &["ls", "--all", "--json"]
    } else {
        &["ls", "--json"]
    };

    hc_json(session, args)["terminals"]
        .as_array()
        .expect("terminals array")
        .clone()
}

/// The addresses `hc ls` lists in `session`, in `hc` order.
fn addresses(session: &str, all: bool) -> Vec<String> {
    terminals(session, all)
        .iter()
        .map(|t| t["address"].as_str().expect("address").to_string())
        .collect()
}

/// `hc get --json` for the terminal `address` in `session`.
fn get(session: &str, address: &str) -> Value {
    hc_json(session, &["get", "--json", address])
}

/// Creates a fresh `/tmp/hc.XXXXXX` directory, mode 0700. The path stays
/// short because unix socket paths are limited in length.
fn make_private_dir() -> PathBuf {
    /// The characters a directory suffix draws from.
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
        let dir = PathBuf::from(format!("/tmp/hc.{suffix}"));

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

impl Cores {
    /// Creates the private directory and starts both cores. `None` when
    /// `hc` is not installed; any other failure panics. The directory is
    /// announced before anything else happens, and the cleanup guard
    /// exists before any core does.
    fn start() -> Option<Self> {
        let dir = make_private_dir();

        println!("hc-test-dir: {}", dir.display());

        let pid = std::process::id();
        let mut cores = Self {
            dir,
            a: format!("md-test-{pid}"),
            b: format!("md-test-{pid}-b"),
            s_b: String::new(),
            started: Vec::new(),
        };

        // SAFETY: nothing else in this process reads or writes the
        // environment concurrently: the test harness runs this one
        // `#[test]`, and no scenario thread exists yet.
        unsafe {
            for (key, _) in env::vars_os() {
                if key.to_string_lossy().starts_with("HC_") {
                    env::remove_var(key);
                }
            }

            env::set_var("XDG_RUNTIME_DIR", &cores.dir);
            env::set_var("XDG_STATE_HOME", cores.dir.join("state"));
            env::set_var("XDG_CONFIG_HOME", cores.dir.join("config"));
        }

        match Command::new("hc").arg("--version").output() {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => panic!("running hc --version: {e}"),
        }

        for session in [cores.a.clone(), cores.b.clone()] {
            cores.started.push(session.clone());
            hc(&session, &["server", "start"]);

            let deadline = Instant::now() + Duration::from_secs(10);

            while !hc_output(&session, &["server", "status"]).status.success()
            {
                assert!(
                    Instant::now() < deadline,
                    "core {session} never came up"
                );

                thread::sleep(Duration::from_millis(100));
            }
        }

        let sentinel = hc_json(
            &cores.b,
            &[
                "run", "--json", "--bg", "--name", "sentinel", "--", "sleep",
                "300",
            ],
        );

        cores.s_b = sentinel["address"]
            .as_str()
            .expect("sentinel address")
            .to_string();

        Some(cores)
    }

    /// A driver aimed at the core under test.
    fn driver(&self) -> Hc {
        Hc::new(Some(self.a.clone()), None)
    }

    /// A driver aimed at the core under test, with fallback patterns.
    fn patterned(&self) -> Hc {
        Hc::new(Some(self.a.clone()), Some(patterns()))
    }

    /// `hc get --json` in the core under test.
    fn get(&self, pane: &Handle) -> Value {
        get(&self.a, &pane.0)
    }

    /// How many live terminals the core under test has.
    fn live(&self) -> usize {
        terminals(&self.a, false).len()
    }
}

impl Drop for Cores {
    fn drop(&mut self) {
        assert!(
            self.dir.to_string_lossy().starts_with("/tmp/hc."),
            "refusing to clean {}",
            self.dir.display()
        );

        // Best effort: nothing runs when `hc` is missing or a core never
        // started. Only sessions this test started are stopped.
        for session in self.started.iter().rev() {
            let _ = hc_cmd(session).args(["server", "stop"]).output();
        }

        // A core saves its state on the way out and may recreate files
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

/// The patterns of the fallback scenarios.
fn patterns() -> Patterns {
    // The idle pattern allows no trailing space: `hc read` strips it from
    // the prompt line `$ `.
    Patterns::from_json(r#"{"idle":["\\$ ?$"],"blocked":["\\(y/n\\)"]}"#)
        .expect("patterns compile")
}

/// A command line as a spawn request argument list.
fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// Polls `probe` until it returns `Some`, for up to 5 s.
fn until<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        if let Some(found) = probe() {
            return Some(found);
        }

        if Instant::now() >= deadline {
            return None;
        }

        thread::sleep(Duration::from_millis(100));
    }
}

/// Polls `pane_read` of `lines` lines until `check` accepts the output,
/// for up to 5 s, and returns the last output.
fn read_until(
    driver: &Hc,
    pane: &Handle,
    lines: u32,
    check: impl Fn(&str) -> bool,
) -> String {
    let mut last = String::new();

    until(|| {
        last = driver.pane_read(pane, lines, false).expect("pane read");

        check(&last).then_some(())
    });

    last
}

/// Whether `out` has a line that is exactly `line`.
fn has_line(out: &str, line: &str) -> bool {
    out.lines().any(|l| l == line)
}

/// Polls `pane_status` until it shows `status` with `confidence`, for up
/// to 5 s.
fn await_status(
    driver: &Hc,
    pane: &Handle,
    status: Status,
    confidence: Confidence,
) -> StatusResult {
    let mut last = None;
    let found = until(|| {
        let result = driver.pane_status(pane).expect("pane status");
        let hit = result.status == status && result.confidence == confidence;

        last = Some(result.clone());

        hit.then_some(result)
    });

    found.unwrap_or_else(|| {
        panic!("never {status:?} with {confidence:?}; last {last:?}")
    })
}

/// Tries to spawn a terminal named `name` in `/tmp` that runs `command`.
fn try_spawn(
    driver: &Hc,
    name: &str,
    workspace: Option<&str>,
    command: &[&str],
) -> Result<Handle> {
    driver.pane_spawn(&SpawnRequest {
        name,
        workspace,
        cwd: Path::new("/tmp"),
        command: &argv(command),
    })
}

/// Spawns a terminal named `name` that runs `command`.
fn spawn(driver: &Hc, name: &str, command: &[&str]) -> Handle {
    try_spawn(driver, name, None, command).expect("pane spawn")
}

/// Spawns a plain `bash` named `name`.
fn spawn_shell(driver: &Hc, name: &str) -> Handle {
    spawn(driver, name, &["bash", "--norc", "--noprofile"])
}

/// Spawns a `bash` with the prompt `$ ` named `name`.
fn spawn_prompted_shell(driver: &Hc, name: &str) -> Handle {
    spawn(
        driver,
        name,
        &["env", "PS1=$ ", "bash", "--norc", "--noprofile"],
    )
}

/// Types `text` into `pane` and presses Enter.
fn prompt(driver: &Hc, pane: &Handle, text: &str) {
    driver.pane_prompt(pane, text).expect("pane prompt");
}

/// The shell line that makes the program report `state` over OSC 7501.
fn report(state: &str) -> String {
    format!(r"printf '\033]7501;state={state}\007'")
}

/// Removes `pane` with `pane_kill`.
fn close(driver: &Hc, pane: &Handle) {
    driver.pane_kill(pane).expect("pane kill");
}

/// Runs `run` with the process variables `vars` set, then restores them,
/// also when `run` panics. No other thread may exist at this point.
fn with_env<T>(vars: &[(&str, &OsString)], run: impl FnOnce() -> T) -> T {
    let saved: Vec<(String, Option<OsString>)> = vars
        .iter()
        .map(|(key, _)| ((*key).to_string(), env::var_os(key)))
        .collect();

    // SAFETY: no other thread of this process exists at this point; the
    // wait scenario joined its helper, and the harness thread only waits.
    unsafe {
        for (key, value) in vars {
            env::set_var(key, value);
        }
    }

    let caught = panic::catch_unwind(AssertUnwindSafe(run));

    // SAFETY: as above.
    unsafe {
        for (key, old) in &saved {
            match old {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
        }
    }

    match caught {
        Ok(value) => value,
        Err(payload) => panic::resume_unwind(payload),
    }
}

/// Guard: the socket directory holds the two private sockets and no
/// `default.sock`, so the user's own core cannot be reached.
fn should_hold_private_sockets_when_cores_started(cores: &Cores) {
    let names: Vec<String> = fs::read_dir(cores.dir.join("herdr-core"))
        .expect("the socket directory exists")
        .map(|e| {
            e.expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    for session in [&cores.a, &cores.b] {
        assert!(
            names.contains(&format!("{session}.sock")),
            "no socket for {session} in {names:?}"
        );
    }

    assert!(
        !names.iter().any(|n| n == "default.sock"),
        "default.sock exists: {names:?}"
    );
}

/// A spawned command runs, and its output can be read.
fn should_run_command_and_read_output_when_spawned(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn(
        &driver,
        "spawn-1",
        &["bash", "-c", r#"echo hc-spawn-mar""ker; exec sleep 60"#],
    );
    let out =
        read_until(&driver, &pane, 20, |o| has_line(o, "hc-spawn-marker"));

    assert!(has_line(&out, "hc-spawn-marker"), "no marker in {out:?}");

    close(&driver, &pane);
}

/// hc keeps no escape codes, so an ANSI read is unsupported.
fn should_fail_unsupported_when_read_with_ansi(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn_shell(&driver, "ansi-1");
    let err = driver
        .pane_read(&pane, 5, true)
        .expect_err("read must fail");

    assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");

    close(&driver, &pane);
}

/// A prompt typed into a shell runs, and its result can be read.
fn should_run_line_when_shell_is_prompted(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn_shell(&driver, "prompt-1");

    prompt(&driver, &pane, "echo hc-prompt-mar''ker");

    let out =
        read_until(&driver, &pane, 20, |o| has_line(o, "hc-prompt-marker"));

    assert!(has_line(&out, "hc-prompt-marker"), "no marker in {out:?}");

    close(&driver, &pane);
}

/// The driver, not hc, bounds the output to the lines asked for.
fn should_bound_output_when_read_with_lines(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn_prompted_shell(&driver, "lines-1");

    prompt(&driver, &pane, "seq 1 200");

    let out = read_until(&driver, &pane, 5, |o| {
        has_line(o, "200") && o.ends_with('$')
    });

    assert!(has_line(&out, "200"), "seq never finished: {out:?}");

    // hc strips the trailing space of the prompt line `$ `.
    let three = driver.pane_read(&pane, 3, false).expect("pane read");

    assert_eq!(three, "199\n200\n$");

    let last = three.lines().last().expect("a last line");

    assert_eq!(driver.pane_read(&pane, 1, false).expect("pane read"), last);

    let all = driver.pane_read(&pane, 0, false).expect("pane read");

    assert!(all.lines().count() >= 3, "got {all:?}");
    assert!(all.ends_with(last), "got {all:?}");

    close(&driver, &pane);
}

/// Renaming changes the label and neither the handle nor the hc name.
fn should_keep_handle_when_renamed(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn_shell(&driver, "rename-1");
    let before = pane.clone();

    driver.pane_rename(&pane, "renamed-x").expect("pane rename");

    let panes = driver.pane_list(None).expect("pane list");
    let found = panes
        .iter()
        .find(|p| p.handle == pane)
        .expect("the handle is still listed");

    assert_eq!(found.label.as_deref(), Some("renamed-x"));
    assert_eq!(pane, before);
    assert_eq!(cores.get(&pane)["name"], "rename-1");

    close(&driver, &pane);
}

/// The name and the label are set on spawn; a name hc rejects still
/// spawns, with a label only.
fn should_set_name_and_label_when_spawned(cores: &Cores) {
    let driver = cores.driver();
    let plain = spawn_shell(&driver, "impl-003");
    let got = cores.get(&plain);

    assert_eq!(got["name"], "impl-003");
    assert_eq!(got["labels"]["md.label"], "impl-003");

    close(&driver, &plain);

    let spaced = spawn_shell(&driver, "Impl 003");
    let got = cores.get(&spaced);

    assert!(got["name"].is_null(), "got {got}");
    assert_eq!(got["labels"]["md.label"], "Impl 003");

    close(&driver, &spaced);

    let before = cores.live();
    let id_like = spawn_shell(&driver, "t7x");
    let got = cores.get(&id_like);

    assert!(got["name"].is_null(), "got {got}");
    assert_eq!(got["labels"]["md.label"], "t7x");
    assert_eq!(cores.live(), before + 1);

    close(&driver, &id_like);
}

/// A taken name is a usage error that leaves the first terminal alone.
fn should_refuse_spawn_when_name_is_taken(cores: &Cores) {
    let driver = cores.driver();
    let first = spawn_shell(&driver, "taken-1");
    let before = cores.live();
    let err = try_spawn(&driver, "taken-1", None, &["bash"])
        .expect_err("spawn must fail");

    assert!(
        matches!(&err, Error::Usage(m) if m.starts_with("name_taken:")),
        "got {err:?}"
    );
    assert_eq!(cores.get(&first)["state"], "live");
    assert_eq!(cores.live(), before);

    close(&driver, &first);

    let third = spawn_shell(&driver, "taken-1");

    assert_ne!(third, first);

    close(&driver, &third);
}

/// A spawn without a command is refused before hc runs.
fn should_refuse_spawn_when_command_is_empty(cores: &Cores) {
    let driver = cores.driver();
    let before = cores.live();
    let err = try_spawn(&driver, "empty-1", None, &[]).expect_err("must fail");

    assert!(
        matches!(&err, Error::Usage(m) if m == "a command is required on hc"),
        "got {err:?}"
    );
    assert_eq!(cores.live(), before);
}

/// A workspace is the `herdr.space` label; the tab is `herdr.tab`.
fn should_place_in_space_when_workspace_given(cores: &Cores) {
    let driver = cores.driver();
    let x = try_spawn(&driver, "space-x", Some("s1"), &["sleep", "60"])
        .expect("pane spawn");
    let y = spawn(&driver, "space-y", &["sleep", "60"]);

    // The tab equals X's space on purpose: only the space may filter.
    hc(
        &cores.a,
        &["label", "--json", &y.0, "herdr.space=s2", "herdr.tab=s1"],
    );

    let in_s1 = driver.pane_list(Some("s1")).expect("pane list");

    assert_eq!(in_s1.len(), 1, "got {in_s1:?}");
    assert_eq!(in_s1[0].handle, x);
    assert_eq!(in_s1[0].workspace_id, "s1");

    let in_s2 = driver.pane_list(Some("s2")).expect("pane list");

    assert_eq!(in_s2.len(), 1, "got {in_s2:?}");
    assert_eq!(in_s2[0].handle, y);
    assert_eq!(in_s2[0].tab_id, "s1");

    let listed: Vec<String> = driver
        .pane_list(None)
        .expect("pane list")
        .into_iter()
        .map(|p| p.handle.0)
        .collect();

    assert_eq!(listed, addresses(&cores.a, false));
    assert!(listed.contains(&x.0) && listed.contains(&y.0));

    close(&driver, &x);
    close(&driver, &y);
}

/// The listing is empty with no terminal, shows an unlabelled terminal
/// bare, and keeps two equal labels apart by handle.
fn should_list_zero_one_and_two_terminals(cores: &Cores) {
    let driver = cores.driver();

    assert!(driver.pane_list(None).expect("pane list").is_empty());

    let started =
        hc_json(&cores.a, &["run", "--json", "--bg", "--", "sleep", "60"]);
    let bare = Handle(started["address"].as_str().expect("address").into());
    let listed = driver.pane_list(None).expect("pane list");

    assert_eq!(listed.len(), 1, "got {listed:?}");
    assert_eq!(listed[0].handle, bare);
    assert_eq!(listed[0].label, None);
    assert_eq!(listed[0].workspace_id, "");
    assert_eq!(listed[0].tab_id, "");
    assert!(listed[0].cwd.is_some(), "got {:?}", listed[0]);
    assert_eq!(listed[0].agent, None);

    let d1 = spawn(&driver, "dup-label", &["sleep", "60"]);
    let d2 = spawn(&driver, "Dup Label", &["sleep", "60"]);

    driver.pane_rename(&d2, "dup-label").expect("pane rename");

    let listed = driver.pane_list(None).expect("pane list");
    let dups: Vec<&Handle> = listed
        .iter()
        .filter(|p| p.label.as_deref() == Some("dup-label"))
        .map(|p| &p.handle)
        .collect();

    assert_eq!(dups.len(), 2, "got {listed:?}");
    assert_ne!(dups[0], dups[1]);

    let live = addresses(&cores.a, false);

    assert!(
        dups.iter().all(|h| live.contains(&h.0)),
        "{dups:?} {live:?}"
    );
    assert!(dups.contains(&&d1) && dups.contains(&&d2), "got {dups:?}");

    driver.pane_rename(&d2, "dup-2").expect("pane rename");

    let label_of = |pane: &Handle| {
        driver
            .pane_list(None)
            .expect("pane list")
            .into_iter()
            .find(|p| p.handle == *pane)
            .expect("the pane is listed")
            .label
    };

    assert_eq!(label_of(&d1).as_deref(), Some("dup-label"));
    assert_eq!(label_of(&d2).as_deref(), Some("dup-2"));
    assert_eq!(label_of(&bare), None);

    for pane in [&bare, &d1, &d2] {
        close(&driver, pane);
    }

    assert!(driver.pane_list(None).expect("pane list").is_empty());
}

/// An interrupt stops a running `sleep`, so the shell takes the next line.
fn should_stop_sleep_when_interrupted(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn_shell(&driver, "interrupt-1");
    let foreground = |name: &str| {
        until(|| {
            (cores.get(&pane)["foreground"]["name"] == name).then_some(())
        })
        .is_some()
    };

    prompt(&driver, &pane, "sleep 60");

    assert!(foreground("sleep"), "sleep never ran in the foreground");

    driver.pane_interrupt(&pane).expect("pane interrupt");

    assert!(foreground("bash"), "the shell never took the foreground");

    prompt(&driver, &pane, "echo hc-int-mar''ker");

    let out = read_until(&driver, &pane, 20, |o| has_line(o, "hc-int-marker"));

    assert!(has_line(&out, "hc-int-marker"), "no marker in {out:?}");

    close(&driver, &pane);
}

/// A killed terminal is removed, not only ended.
fn should_remove_terminal_when_killed(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn_shell(&driver, "kill-1");

    driver.pane_kill(&pane).expect("pane kill");

    let panes = driver.pane_list(None).expect("pane list");

    assert!(panes.iter().all(|p| p.handle != pane), "got {panes:?}");

    let err = driver.pane_status(&pane).expect_err("status must fail");

    assert!(matches!(err, Error::NotFound(_)), "got {err:?}");
    assert!(
        !addresses(&cores.a, true).contains(&pane.0),
        "still listed by ls --all"
    );
}

/// A terminal whose program already exited is removed by kill.
fn should_remove_finished_terminal_when_killed(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn(&driver, "done-1", &["bash", "-c", "exit 0"]);
    let finished = until(|| {
        matches!(driver.pane_status(&pane), Err(Error::NotFound(_)))
            .then_some(())
    });

    assert!(finished.is_some(), "the terminal never finished");

    driver
        .pane_kill(&pane)
        .expect("kill of a finished terminal");

    assert!(
        !addresses(&cores.a, true).contains(&pane.0),
        "still listed by ls --all"
    );
}

/// A shell reports no agent state: `unknown` with no confidence.
fn should_report_unknown_when_pane_is_a_shell(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn_shell(&driver, "status-1");
    let result = driver.pane_status(&pane).expect("pane status");

    assert_eq!(result.status, Status::Unknown);
    assert_eq!(result.confidence, Confidence::None);
    assert_eq!(result.tail, None);

    close(&driver, &pane);
}

/// Without a reported state the patterns decide; a reported state wins
/// and nothing is read.
fn should_classify_with_patterns_when_no_status_reported(cores: &Cores) {
    let driver = cores.patterned();
    let pane = spawn_prompted_shell(&driver, "status-2");
    let idle =
        await_status(&driver, &pane, Status::Idle, Confidence::Heuristic);

    assert!(idle.tail.is_some(), "got {idle:?}");

    prompt(&driver, &pane, &report("working"));

    let working =
        await_status(&driver, &pane, Status::Working, Confidence::Native);

    assert_eq!(working.tail, None);

    close(&driver, &pane);
}

/// The states a program reports map to working, idle, and idle.
fn should_report_working_idle_and_done_when_agent_reports(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn_shell(&driver, "status-3");

    prompt(&driver, &pane, &report("working"));
    await_status(&driver, &pane, Status::Working, Confidence::Native);

    prompt(&driver, &pane, &report("idle"));
    await_status(&driver, &pane, Status::Idle, Confidence::Native);

    // Both idle and done map to idle, so wait for hc to hold `done`
    // before the driver is asked.
    prompt(&driver, &pane, &report("done"));

    let done = until(|| {
        (cores.get(&pane)["status"]["state"] == "done").then_some(())
    });

    assert!(done.is_some(), "hc never held done");
    await_status(&driver, &pane, Status::Idle, Confidence::Native);

    close(&driver, &pane);
}

/// Blocked and error both map to blocked, and a prompt to a blocked
/// program is refused.
fn should_report_blocked_when_agent_reports_blocked_or_error(cores: &Cores) {
    let driver = cores.driver();
    let blocked = spawn(
        &driver,
        "status-4",
        &[
            "bash",
            "-c",
            r#"printf "\033]7501;state=blocked:kind=permission\007"; sleep 60"#,
        ],
    );

    await_status(&driver, &blocked, Status::Blocked, Confidence::Native);

    let err = driver
        .pane_prompt(&blocked, "echo x")
        .expect_err("prompt must fail");

    assert!(
        matches!(&err, Error::Unexpected(m) if m.starts_with("blocked:")),
        "got {err:?}"
    );

    close(&driver, &blocked);

    let errored = spawn(
        &driver,
        "status-5",
        &[
            "bash",
            "-c",
            r#"printf "\033]7501;state=error\007"; sleep 60"#,
        ],
    );

    await_status(&driver, &errored, Status::Blocked, Confidence::Native);

    close(&driver, &errored);
}

/// A wait returns when a helper thread changes the state to idle.
fn should_return_when_state_changes_during_wait(cores: &Cores) {
    let driver = cores.driver();
    let pane = spawn_shell(&driver, "wait-1");

    prompt(&driver, &pane, &report("working"));
    await_status(&driver, &pane, Status::Working, Confidence::Native);

    let helper = {
        let session = cores.a.clone();
        let target = pane.clone();

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));

            Hc::new(Some(session), None).pane_prompt(&target, &report("idle"))
        })
    };
    let started = Instant::now();
    let result = wait_for(
        &driver,
        &pane,
        &[Status::Idle],
        Duration::from_secs(5),
        Duration::from_millis(100),
    );

    helper
        .join()
        .expect("helper thread")
        .expect("prompt in the helper");

    assert!(
        matches!(&result, Ok(r) if r.status == Status::Idle),
        "wait ended with {result:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(400));

    close(&driver, &pane);
}

/// An address that exists nowhere is `not_found` for status, read, and
/// prompt.
fn should_report_not_found_when_pane_missing(cores: &Cores) {
    let driver = cores.driver();
    let missing = Handle(MISSING.into());
    let errors = [
        driver.pane_status(&missing).map(drop),
        driver.pane_read(&missing, 5, false).map(drop),
        driver.pane_prompt(&missing, "echo x"),
    ];

    for err in errors {
        let err = err.expect_err("must fail");

        assert!(matches!(err, Error::NotFound(_)), "got {err:?}");
    }
}

/// With no `hc` on `PATH` the harness is unavailable.
fn should_fail_unavailable_when_hc_missing(cores: &Cores) {
    let empty = cores.dir.join("empty-bin");

    fs::create_dir_all(&empty).expect("create the empty directory");

    let driver = cores.driver();
    let err = with_env(&[("PATH", &empty.clone().into_os_string())], || {
        driver.pane_list(None)
    })
    .expect_err("list must fail");

    assert!(matches!(err, Error::HarnessUnavailable(_)), "got {err:?}");
}

/// Splits and workspaces belong to the app, so the driver refuses them.
fn should_refuse_split_and_workspace_when_hc(cores: &Cores) {
    let driver = cores.driver();
    let target = Handle("t1".into());
    let split = driver.pane_split(&SplitRequest {
        target: &target,
        direction: Direction::Right,
        name: "x",
        cwd: Path::new("/tmp"),
        command: &argv(&["bash"]),
    });
    let errors = [
        split.map(drop),
        driver.workspace_list().map(drop),
        driver.workspace_create("x", Path::new("/tmp")).map(drop),
        driver.workspace_close("s1"),
        driver.workspace_tag("s1", &[], &[]).map(drop),
    ];

    for err in errors {
        let err = err.expect_err("must fail");

        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
    }
}

/// `raw` hands its arguments to hc, and a failure is `unexpected`.
fn should_pass_args_through_when_raw(cores: &Cores) {
    let driver = cores.driver();
    let out = driver.raw(&argv(&["ls", "--json"])).expect("raw ls");
    let listed: Value = serde_json::from_str(&out).expect("ls prints JSON");

    assert!(listed["terminals"].is_array(), "got {listed}");

    let err = driver
        .raw(&argv(&["get", "--json", MISSING]))
        .expect_err("raw must fail");

    assert!(matches!(err, Error::Unexpected(_)), "got {err:?}");
}

/// A driver with a session reaches that core only, whatever the
/// environment holds.
fn should_reach_only_session_a_when_session_given(cores: &Cores) {
    let driver = cores.driver();
    let listed = driver.pane_list(None).expect("pane list");

    assert!(
        listed.iter().all(|p| p.handle.0 != cores.s_b),
        "the sentinel leaked into A: {listed:?}"
    );

    let session = OsString::from(&cores.b);
    let terminal = OsString::from(&cores.s_b);
    let spawned = with_env(
        &[("HC_SESSION", &session), ("HC_TERMINAL_ID", &terminal)],
        || try_spawn(&cores.driver(), "reach-1", None, &["sleep", "60"]),
    );
    let pane = spawned.expect("pane spawn");
    let got = cores.get(&pane);

    assert!(got["parent"].is_null(), "got {got}");

    let sentinel = get(&cores.b, &cores.s_b);

    assert_eq!(sentinel["state"], "live");
    assert_eq!(sentinel["turn"], 0);

    close(&driver, &pane);
}

/// Without a session the driver follows the ambient `HC_SESSION`; an
/// empty session means none.
fn should_inherit_session_when_none(cores: &Cores) {
    let session = OsString::from(&cores.b);

    for driver in [Hc::new(None, None), Hc::new(Some(String::new()), None)] {
        let listed =
            with_env(&[("HC_SESSION", &session)], || driver.pane_list(None))
                .expect("pane list");

        assert!(
            listed.iter().any(|p| p.handle.0 == cores.s_b),
            "the sentinel is missing: {listed:?}"
        );
    }
}

/// Runs the built CLI with the private environment.
fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_multiplexer-driver"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("running the CLI")
}

/// A missing pane is exit 4 `not_found`; a spawn without a command is
/// exit 2.
fn should_exit_4_when_cli_reads_missing_pane(cores: &Cores) {
    let output = cli(&[
        "--harness",
        "hc",
        "--session",
        &cores.a,
        "pane",
        "read",
        MISSING,
        "--lines",
        "3",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().last().expect("an error line");
    let record: Value = serde_json::from_str(last).expect("error JSON");

    assert_eq!(output.status.code(), Some(4), "stderr: {stderr}");
    assert_eq!(record["error"]["type"], "not_found");

    let output = cli(&[
        "--harness",
        "hc",
        "--session",
        &cores.a,
        "pane",
        "spawn",
        "--name",
        "x",
        "--",
    ]);

    assert_eq!(output.status.code(), Some(2));
}

/// What a scenario left behind: a terminal in A, or a changed sentinel
/// core.
fn leftovers(cores: &Cores) -> Result<(), String> {
    let left = addresses(&cores.a, true);
    let sentinel = terminals(&cores.b, true);
    let intact = sentinel.len() == 1
        && sentinel[0]["address"] == cores.s_b.as_str()
        && sentinel[0]["state"] == "live"
        && sentinel[0]["turn"] == 0;

    if !left.is_empty() {
        return Err(format!("terminals left in A: {left:?}"));
    }

    if !intact {
        return Err(format!("the sentinel core changed: {sentinel:?}"));
    }

    Ok(())
}

/// Ends and removes every terminal in A, so one failed scenario cannot
/// disturb the next.
fn sweep(cores: &Cores) {
    for address in addresses(&cores.a, true) {
        let _ = hc_output(&cores.a, &["kill", "--json", &address]); // may be finished
        let _ = hc_output(&cores.a, &["rm", "--json", &address]); // best effort
    }
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
        [$((stringify!($name), $name as fn(&Cores))),+]
    };
}

/// Runs every scenario, in order, against two private cores. A failed
/// scenario does not stop the others; all failures are reported at the
/// end. After every scenario core A must hold no terminal and the
/// sentinel in core B must be untouched.
#[test]
fn hc_driver_scenarios() {
    let Some(cores) = Cores::start() else {
        println!("skip: hc not installed");

        return;
    };
    let mut failures = Vec::new();

    println!();

    for (name, scenario) in scenarios![
        should_hold_private_sockets_when_cores_started,
        should_run_command_and_read_output_when_spawned,
        should_fail_unsupported_when_read_with_ansi,
        should_run_line_when_shell_is_prompted,
        should_bound_output_when_read_with_lines,
        should_keep_handle_when_renamed,
        should_set_name_and_label_when_spawned,
        should_refuse_spawn_when_name_is_taken,
        should_refuse_spawn_when_command_is_empty,
        should_place_in_space_when_workspace_given,
        should_list_zero_one_and_two_terminals,
        should_stop_sleep_when_interrupted,
        should_remove_terminal_when_killed,
        should_remove_finished_terminal_when_killed,
        should_report_unknown_when_pane_is_a_shell,
        should_classify_with_patterns_when_no_status_reported,
        should_report_working_idle_and_done_when_agent_reports,
        should_report_blocked_when_agent_reports_blocked_or_error,
        should_return_when_state_changes_during_wait,
        should_report_not_found_when_pane_missing,
        should_fail_unavailable_when_hc_missing,
        should_refuse_split_and_workspace_when_hc,
        should_pass_args_through_when_raw,
        should_reach_only_session_a_when_session_given,
        should_inherit_session_when_none,
        should_exit_4_when_cli_reads_missing_pane,
    ] {
        let caught =
            panic::catch_unwind(AssertUnwindSafe(|| scenario(&cores)));
        let guard = leftovers(&cores);

        sweep(&cores);

        match (caught, guard) {
            (Ok(()), Ok(())) => println!("ok: {name}"),
            (Err(payload), _) => {
                let text = panic_text(payload.as_ref());

                println!("FAILED: {name}: {text}");
                failures.push(format!("{name}: {text}"));
            }
            (Ok(()), Err(text)) => {
                println!("FAILED: {name}: {text}");
                failures.push(format!("{name}: {text}"));
            }
        }
    }

    assert!(failures.is_empty(), "failed scenarios: {failures:#?}");
}
