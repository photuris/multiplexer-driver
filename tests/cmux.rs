//! cmux integration test. One `#[test]` drives the ambient cmux app in
//! workspaces it creates itself (every name starts with
//! `md-test-<pid>-<nanos>`), runs every scenario in order, then closes
//! whatever it created. It skips when `cmux ping` does not answer. Only
//! shells run in panes, never a paid agent.

#![cfg(unix)]
// Helpers outside `#[test]` fns are not covered by clippy.toml's
// allow-expect-in-tests; a failed setup step must panic with a message.
#![expect(clippy::expect_used, reason = "test helpers panic on failure")]

use std::{
    collections::BTreeSet,
    io, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use multiplexer_driver::{
    cmux::Cmux,
    driver::{Driver, Error, SpawnRequest, SplitRequest},
    model::{Confidence, Direction, Handle, Status},
    patterns::Patterns,
    process::{CEILING, output_within},
};
use serde_json::Value;

/// Runs `cmux args…` quietly under the ceiling and returns stdout.
fn cmux_output(args: &[&str]) -> io::Result<std::process::Output> {
    let mut command = std::process::Command::new("cmux");

    command.args(args).env("CMUX_QUIET", "1");

    output_within(&mut command, CEILING)
}

/// Every `(workspace id, title)` in every window.
fn workspaces() -> Vec<(String, String)> {
    let Ok(output) =
        cmux_output(&["tree", "--all", "--json", "--id-format", "uuids"])
    else {
        return Vec::new();
    };
    let Ok(tree) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Vec::new();
    };

    tree["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|w| w["workspaces"].as_array().into_iter().flatten())
        .filter_map(|w| {
            Some((w["id"].as_str()?.to_string(), w["title"].as_str()?.into()))
        })
        .collect()
}

/// Closes the workspaces this test created, even when it panics.
#[derive(Debug)]
struct Cleanup {
    /// Name prefix of everything this test creates.
    ns: String,
    /// Workspaces that existed before the test; never closed.
    before: BTreeSet<String>,
    /// Workspaces the test created.
    created: Vec<String>,
}

impl Cleanup {
    /// Records the workspaces that exist now.
    fn new(ns: &str) -> Self {
        Self {
            ns: ns.to_string(),
            before: workspaces().into_iter().map(|(id, _)| id).collect(),
            created: Vec::new(),
        }
    }

    /// Remembers a workspace to close.
    fn register(&mut self, id: &str) {
        self.created.push(id.to_string());
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let close = |id: &str| {
            let _ = cmux_output(&["close-workspace", "--workspace", id]);
        };

        for id in &self.created {
            if !self.before.contains(id) {
                close(id);
            }
        }

        // A spawn that failed before its id was recorded.
        for (id, title) in workspaces() {
            if !self.before.contains(&id) && title.starts_with(&self.ns) {
                close(&id);
            }
        }
    }
}

/// Announces a passed scenario.
fn ok(name: &str) {
    eprintln!("scenario ok: {name}");
}

/// Polls `pane_read` until `check` accepts the output, for up to 5 s.
fn read_until(
    driver: &Cmux,
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

/// A command as a spawn request argument list.
fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// Asserts that some line of `out` equals `line`.
fn assert_line(out: &str, line: &str) {
    assert!(
        out.lines().any(|l| l == line),
        "no {line:?} line in {out:?}"
    );
}

/// Whether `cmux ping` answers.
fn reachable() -> bool {
    match cmux_output(&["ping"]) {
        Ok(output) => output.status.success(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) if e.kind() == io::ErrorKind::TimedOut => false,
        Err(e) => panic!("running cmux ping: {e}"),
    }
}

/// Runs every scenario, in order, against the ambient cmux.
#[test]
fn cmux_driver_scenarios() {
    if !reachable() {
        eprintln!("skipping: cmux is not reachable");
        return;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let ns = format!("md-test-{}-{nanos}", std::process::id());
    let mut guard = Cleanup::new(&ns);
    let driver = Cmux::new(None, None);
    let patterned = Cmux::new(
        None,
        Some(
            Patterns::from_json(r#"{"idle":["hello-42"]}"#).expect("patterns"),
        ),
    );

    // 1
    let ws_name = format!("{ns}-ws");
    let ws = driver
        .workspace_create(&ws_name, std::path::Path::new("/tmp"))
        .expect("workspace create");

    guard.register(&ws.workspace_id);
    assert_eq!(ws.label, ws_name);
    assert!(
        driver
            .workspace_list()
            .expect("workspace list")
            .iter()
            .any(|w| w.workspace_id == ws.workspace_id)
    );
    ok("should_create_and_list_workspace");

    // 2
    let name = format!("{ns}-impl");
    let command = argv(&["bash", "-c", "echo hello-$((40+2)); exec bash"]);
    let handle = driver
        .pane_spawn(&SpawnRequest {
            name: &name,
            workspace: None,
            cwd: std::path::Path::new("/tmp"),
            command: &command,
        })
        .expect("pane spawn");
    let panes = driver.pane_list(None).expect("pane list");
    let spawned = panes
        .iter()
        .find(|p| p.handle == handle)
        .expect("spawned pane is listed");

    guard.register(&spawned.workspace_id);

    let out =
        read_until(&driver, &handle, |o| o.lines().any(|l| l == "hello-42"));

    assert_line(&out, "hello-42");
    assert_eq!(spawned.label.as_deref(), Some(name.as_str()));
    assert_eq!(spawned.workspace_label.as_deref(), Some(name.as_str()));
    assert_eq!(spawned.agent, None);
    assert_eq!(spawned.status, Status::Unknown);
    assert_eq!(spawned.status_confidence, Confidence::None);

    let spawn_ws = spawned.workspace_id.clone();

    ok("should_spawn_workspace_and_list_pane");

    // 3
    let plain = driver.pane_status(&handle).expect("plain status");

    assert_eq!(plain.status, Status::Unknown);
    assert_eq!(plain.confidence, Confidence::None);
    assert_eq!(plain.tail, None);

    let guessed = patterned.pane_status(&handle).expect("pattern status");

    assert_eq!(guessed.status, Status::Idle);
    assert_eq!(guessed.confidence, Confidence::Heuristic);
    assert!(
        guessed
            .tail
            .as_deref()
            .is_some_and(|t| t.contains("hello-42")),
        "tail {:?}",
        guessed.tail
    );
    ok("should_report_status_by_patterns_only");

    // 4
    driver
        .pane_prompt(&handle, "echo prompt-$((1+1))")
        .expect("pane prompt");

    let out = read_until(&driver, &handle, |o| o.contains("prompt-2"));

    assert_line(&out, "prompt-2");
    ok("should_prompt_pane");

    // 5
    let new = driver
        .pane_split(&SplitRequest {
            target: &handle,
            direction: Direction::Down,
            name: &format!("{ns}-split"),
            cwd: std::path::Path::new("/usr"),
            command: &argv(&["bash"]),
        })
        .expect("pane split");

    assert_ne!(new, handle);
    driver.pane_prompt(&new, "pwd").expect("prompt pwd");

    let out = read_until(&driver, &new, |o| o.lines().any(|l| l == "/usr"));

    assert_line(&out, "/usr");
    driver
        .pane_prompt(&new, "echo split-$((2+2))")
        .expect("prompt split");

    let out = read_until(&driver, &new, |o| o.contains("split-4"));

    assert_line(&out, "split-4");
    driver.pane_rename(&new, "renamed").expect("pane rename");

    let panes = driver.pane_list(Some(&spawn_ws)).expect("pane list");

    assert!(
        panes
            .iter()
            .any(|p| p.handle == new && p.label.as_deref() == Some("renamed")),
        "got {panes:?}"
    );
    ok("should_split_with_own_cwd_and_rename");

    // 6
    driver
        .pane_prompt(&new, "sleep 30; echo never-1")
        .expect("prompt sleep");
    thread::sleep(Duration::from_secs(1));
    driver.pane_interrupt(&new).expect("pane interrupt");
    driver
        .pane_prompt(&new, "echo after-$((5+5))")
        .expect("prompt after");

    let out = read_until(&driver, &new, |o| o.contains("after-10"));

    assert_line(&out, "after-10");
    assert!(
        !out.lines().any(|l| l == "never-1"),
        "sleep was not interrupted: {out:?}"
    );
    ok("should_interrupt_running_command");

    // 7
    driver.pane_kill(&new).expect("pane kill split");

    let panes = driver.pane_list(Some(&spawn_ws)).expect("pane list");

    assert!(panes.iter().all(|p| p.handle != new), "got {panes:?}");
    assert!(panes.iter().any(|p| p.handle == handle), "got {panes:?}");
    ok("should_kill_only_split_pane");

    // 8
    driver.pane_kill(&handle).expect("pane kill last");
    assert!(
        driver
            .workspace_list()
            .expect("workspace list")
            .iter()
            .all(|w| w.workspace_id != spawn_ws)
    );
    ok("should_close_workspace_when_last_surface_killed");

    // 9
    driver.workspace_close(&ws.workspace_id).expect("close");
    assert!(
        driver
            .workspace_list()
            .expect("workspace list")
            .iter()
            .all(|w| w.workspace_id != ws.workspace_id)
    );
    ok("should_close_workspace");

    // 10
    let err = driver
        .pane_read(
            &Handle("00000000-0000-0000-0000-000000000000".into()),
            5,
            false,
        )
        .expect_err("read must fail");

    assert!(matches!(err, Error::NotFound(_)), "got {err:?}");
    ok("should_report_not_found_when_surface_missing");

    // 11
    let err = Cmux::new(Some("/nonexistent/md.sock".into()), None)
        .workspace_list()
        .expect_err("list must fail");

    assert!(matches!(err, Error::HarnessUnavailable(_)), "got {err:?}");
    ok("should_report_unavailable_when_socket_missing");
}
