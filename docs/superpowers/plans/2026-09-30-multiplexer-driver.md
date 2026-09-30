# multiplexer-driver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps
> use checkbox (`- [ ]`) syntax for tracking. This plan runs under the
> `overseer` skill: each task below is one implementer dispatch.

**Goal:** A Rust CLI, `multiplexer-driver`, that drives tmux and Herdr
workspaces, tabs, panes, and the coding agents inside them, with JSON
output for scripts and agents.

**Architecture:** One crate (lib + thin bin). A `Driver` trait with
default `Unsupported` methods, one implementation per harness
(`tmux.rs`, `herdr.rs`), each shelling out to the harness CLI through
one private run helper. Argv building and output parsing are pure
functions with unit tests. Shared, harness-independent pieces
(`model`, `text`, `patterns`, `agents`, `notify`, the wait loop) live
in their own modules.

**Tech Stack:** Rust 2024 (rustc 1.98), clap 4 (derive, env), serde,
serde_json, regex, thiserror, tracing, tracing-subscriber; dev: rstest,
insta.

**Spec:** `docs/superpowers/specs/2026-09-30-multiplexer-driver-design.md`.
Read it before your task. Section numbers below (§) refer to it. When
this plan and the spec disagree, the spec wins; report the conflict.

## Global Constraints

- Clean room: do not read `~/Projects/overseer-driver` (the Go
  version). Everything needed is in the spec and this plan.
- Binary and crate name: `multiplexer-driver`. Edition 2024,
  `rust-version = "1.98"`.
- Dependencies are exactly: `clap` (features `derive`, `env`),
  `serde` (feature `derive`), `serde_json`, `regex`, `thiserror`,
  `tracing`, `tracing-subscriber` (feature `env-filter`). Dev:
  `rstest`, `insta`. No `anyhow`, no `tokio`. Adding anything else
  needs a reason in the commit message.
- `rustfmt.toml`: `max_width = 79`. Lints per the table in Task 1.
  No `unwrap`/`expect`/`panic!` outside tests.
- Every item has a doc comment (`//!` per module, `///` per item,
  private ones included). `# Errors` on every public fn returning
  `Result`.
- Blank-line rules from `rust-style`: blank line before every control
  statement, `return`, and the tail expression unless first in block.
- Tests: `should_<expected>_when_<state>` names inside
  `mod <unit>` groups.
- Checks before every commit, all must pass:
  `cargo fmt --check`,
  `cargo clippy --all-targets --all-features --locked -- -D warnings`,
  `cargo test --locked`.
- Commit messages: lower case, imperative, one line, no emoji, no
  dashes, no AI attribution lines.
- Integration tests never touch the user's own tmux server or any
  Herdr session except one they created themselves (§10).
- Never launch a paid agent CLI (claude, codex, pi, …) in any test.
  Use `bash` or `sleep`.

## Amendments (plan critic round, 2026-09-30)

These amend the tasks below. Where an amendment and a task step
disagree, the amendment wins. IDs match `.overseer/review/plan.md`.

### Task 1

- **A1-1 (P-03) `src/process.rs`, new module, add `pub mod process;`
  to `lib.rs`.** Every harness subprocess runs through it with a hard
  ceiling, so no call can hang:

```rust
//! Runs harness subprocesses with a hard time ceiling so no call can
//! hang (§4.2).

use std::{
    io::{self, Read},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Longest any single harness subprocess may run.
pub const CEILING: Duration = Duration::from_secs(30);

/// Runs `command` to completion and collects its output, killing it
/// and returning an [`io::ErrorKind::TimedOut`] error past `limit`.
///
/// # Errors
///
/// Spawn failures (e.g. [`io::ErrorKind::NotFound`] for a missing
/// binary), wait failures, and the timeout above.
pub fn output_within(command: &mut Command, limit: Duration) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = thread::spawn(move || read_all(stdout));
    let err_reader = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + limit;

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }

        if Instant::now() >= deadline {
            let _ = child.kill(); // already past the ceiling; best effort
            let _ = child.wait(); // reap; the result changes nothing

            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no exit within {limit:?}"),
            ));
        }

        thread::sleep(Duration::from_millis(10));
    };

    Ok(Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
}

/// Reads a child pipe to its end; a missing pipe or read error gives
/// what was read so far.
fn read_all(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buffer = Vec::new();

    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buffer); // partial output is still useful
    }

    buffer
}
```

  Unit tests: `should_capture_stdout_when_child_exits` (`sh -c 'echo
  hi'` → stdout `hi\n`, success) and `should_kill_when_child_outlives_limit`
  (`sleep 5`, limit 100 ms → `ErrorKind::TimedOut`, elapsed < 2 s).
  Both drivers call `output_within(&mut cmd, process::CEILING)`.
  `TimedOut` maps to `HarnessUnavailable("<harness> timed out: …")`,
  `NotFound` to `HarnessUnavailable("<harness> is not installed")`.

- **A1-2 (P-02, P-04):** `wait_for` in Step 6 is already fixed in
  place, with 7 tests. Use it exactly.

### Task 2

- **A2-1 (P-06):** in `run`, load `--patterns` only when the harness is
  tmux. On Herdr the flag is accepted and ignored, even if the file is
  missing or invalid. Test in `tests/cli.rs`: `--harness herdr pane
  status w1:p1 --patterns /nonexistent` must not exit 2 (it exits 5
  from the stub today; any code but 2 passes).
- **A2-2 (P-15):** `log_level`: `#[arg(long, global = true, env =
  "RUST_LOG", default_value = "warn")]`. The flag beats the variable.
- **A2-3 (P-16):** `sound`: `#[arg(long, value_parser = ["none",
  "done", "request"])]`. Test: `notify --message m --sound loud` →
  exit 2 with a `usage` record.
- **A2-4 (P-18):** the two name-only `impl Driver` stubs in `tmux.rs`
  and `herdr.rs` are the required exception to the no-stubs rule.
- **A2-5 (P-09, exit 6):** factor spawn/split printing into
  `fn print_start(out: &mut dyn Write, outcome: Result<Handle>) ->
  Result<()>` in `lib.rs`: `Ok(h)` prints `{"handle":h,"started":true}`,
  `Err(StartFailed{handle,..})` prints `{"handle":handle,"started":false}`
  and returns the error, any other `Err` prints nothing and returns
  it. Unit tests for all three branches assert the exact stdout bytes.
- **A2-6 (P-17):** every verb's doc comment states its output shape in
  one sentence and one runnable example (`Example: multiplexer-driver
  …`). `pane read` help also carries the note from spec §7.2 that the
  reset code closing a dim run can land at the start of the next line
  (the text must contain the word "reset"). Snapshot the help of every
  noun and every verb with one `rstest` table in `tests/cli.rs` (4
  noun-level + 16 verb-level cases).
- **A2-7 (P-04):** `parse_duration` is fixed in place (checked
  multiplication, overflow cases added). Also map an overflowing
  `u64` parse to the same error.

### Tasks 3 and 4 (tmux)

- **A3-1 (P-03):** `run_once` uses `process::output_within` (A1-1).
- **A3-2 (P-07):** add `fn is_no_server(stderr: &str) -> bool`: true
  for `no server running…`, and for `error connecting to …` only when
  the text contains `No such file or directory` or `Connection
  refused`. `classify_error` still maps all `error connecting to`
  text to `HarnessUnavailable`. `workspace_list` and `pane_list`
  return `Ok(vec![])` only when the error message satisfies
  `is_no_server`. A missing binary stays `HarnessUnavailable`.
  Unit cases: both no-server texts → true; `error connecting to
  /x (Permission denied)` → false.
- **A3-3 (P-05):** `raw` runs through a lower-level `fn output(&self,
  args) -> Result<Output>` (session applied, ceiling applied, spawn
  failures mapped). A nonzero exit becomes `Unexpected(<stderr>)`,
  never `NotFound` or `Usage`. `run` is `output` plus
  `classify_error`. Integration test
  `should_fail_unexpected_when_raw_command_fails`: `raw(["kill-pane",
  "-t", "%999"])` → kind `unexpected`.
- **A3-4 (P-08):** `Server::start` returns `None` (skip) only when
  spawning tmux fails with `io::ErrorKind::NotFound`. Any other
  startup failure panics with the captured stderr. Record the socket
  path at start (`display-message -p '#{socket_path}'`), and in `Drop`
  run `kill-server` and then `fs::remove_file(socket_path)` (tmux
  leaves the socket file behind, verified). The absent-server test in
  Task 4 also skips when tmux is not installed.
- **A4-1 (P-14):** tags: `show-options -t <id>` gives only the key
  names (`fn parse_tag_keys(&str) -> Vec<String>`, keep names starting
  `@md-tag-`, strip the prefix). Read each value with `show-options -v
  -t <id> @md-tag-<k>` and trim exactly one trailing `\n`. Unquoted
  `show-options` output escapes values (verified: `a"b\c d` prints as
  `"a\"b\\c d"`), so never parse values from it. Replace the
  `parse_tags` unit test with a `parse_tag_keys` test, and add
  integration test `should_round_trip_tag_values_when_escaped` with the
  values `a"b\c d`, `it's`, and the empty string.
- **A4-2 (P-09):** add subprocess tests to `tests/tmux.rs` that run the
  built binary (`env!("CARGO_BIN_EXE_multiplexer-driver")`) against the
  private server: `should_exit_4_when_cli_reads_missing_pane`,
  `should_exit_7_when_cli_wait_times_out` (patterns file that never
  matches, `--timeout 300ms`), and `should_print_jsonl_when_cli_lists_panes`
  (with `--pretty`: every stdout line parses as one JSON object, no
  line starts with `[`, line count equals the pane count). Assert the
  last stderr line's `error.type` for the two error tests.

### Tasks 5 and 6 (Herdr)

- **A5-1 (P-03):** `run` uses `process::output_within` (A1-1).
- **A5-2 (P-11, also spec §4.4):** when `session` is `Some`, remove
  every environment variable whose name starts with `HERDR_` from the
  child (`env::vars()` filtered by prefix, `env_remove` each), then set
  `HERDR_SOCKET_PATH`. The test harness does the same for the server
  and for its own direct `herdr` calls: remove all `HERDR_*`, not a
  fixed list.
- **A5-3 (P-10):** `classify_error` maps exactly `pane_not_found`,
  `workspace_not_found`, `agent_not_found` to `NotFound`. Add case
  `#[case::unknown_not_found(r#"{"error":{"code":"weird_not_found","message":"m"}}"#, "unexpected")]`.
- **A5-4 (P-05):** as A3-3: `raw` maps a nonzero exit to
  `Unexpected(<stderr>)`.
- **A5-5 (P-12):** scenario 1 runs `["bash", "-c", "echo
  hello-$((40+2)); exec bash"]` and asserts a line equal to `hello-42`
  in the read output (the typed command holds `$((40+2))`, not `42`).
- **A5-6 (P-09, P-11):** four more scenarios in Task 5 (12 in total):
  `should_route_only_to_test_session` (the driver's `raw(["pane",
  "list"])` output does not contain the value of the ambient
  `HERDR_PANE_ID`, when that variable is set in the test process),
  `should_fail_unexpected_when_raw_command_fails` (`raw(["pane",
  "get", "<ws>:p99"])` → `unexpected`),
  `should_exit_3_when_cli_session_missing` (binary with `--harness
  herdr --session md-test-absent-<pid> pane read w1:p1` → exit 3,
  last stderr `harness_unavailable`), and
  `should_exit_4_when_cli_reads_missing_pane` (binary with the test
  session, `pane read <ws>:p99` → exit 4). Task 6 then adds its 4, so
  16 in total.

## File map

| File | Responsibility | Task |
|---|---|---|
| `Cargo.toml`, `rustfmt.toml`, `clippy.toml`, `.gitignore`, `LICENSE` | project config | 1 |
| `src/model.rs` | serde record types, `Handle`, enums | 1 |
| `src/driver.rs` | `Error`, `Result`, `Driver` trait, requests, `wait_for` | 1 |
| `src/text.rs` | blank-line trim, shell join | 1 |
| `src/patterns.rs` | patterns file load + classify | 1 |
| `src/agents.rs` | known kinds, resume argv | 1 |
| `src/process.rs` | subprocess with hard ceiling | 1 |
| `src/lib.rs` | crate docs, module list, `Harness`, `driver_for`, `run` | 1 (modules), 2 (run) |
| `src/cli.rs` | clap definitions, duration parser | 2 |
| `src/notify.rs` | notify | 2 |
| `src/main.rs` | parse, tracing, exit mapping | 2 |
| `src/tmux.rs` | tmux driver | 2 (stub), 3, 4 |
| `src/herdr.rs` | Herdr driver | 2 (stub), 5, 6 |
| `tests/cli.rs` | exit codes, output shape, help snapshots | 2 |
| `tests/tmux.rs` | tmux integration | 3, 4 |
| `tests/herdr.rs` | Herdr integration | 5, 6 |
| `tests/fixtures/herdr/*.json` | captured Herdr output | 5, 6 |
| `README.md`, `AGENTS.md`, `CLAUDE.md`, `docs/overseer-migration.md` | docs | 7 |

## Task order and parallelism

```
Task 1 → Task 2 ─┬→ Task 3 → Task 4 ─┬→ Task 7
                 └→ Task 5 → Task 6 ─┘
```

Tasks 3–4 (tmux lane) and 5–6 (Herdr lane) touch disjoint files and
may run in parallel.

---

### Task 1: Core library (config, model, errors, trait, text, patterns, agents)

**Files:**
- Create: `Cargo.toml`, `rustfmt.toml`, `clippy.toml`, `.gitignore`,
  `LICENSE` (MIT, copyright holder "Joshua"), `src/lib.rs`,
  `src/model.rs`, `src/driver.rs`, `src/text.rs`, `src/patterns.rs`,
  `src/agents.rs`

**Interfaces:**
- Consumes: nothing.
- Produces (exact; later tasks rely on these names):
  - `model::{Handle, Status, Confidence, SessionRefKind, AgentSession,
    WorkspaceRecord, PaneRecord, StatusResult, StartResult, Direction}`
  - `driver::{Error, Result, Driver, SpawnRequest, SplitRequest,
    unsupported, wait_for}`
  - `text::{trim_trailing_blank_lines, shell_join}`
  - `patterns::Patterns` with `Patterns::load(&Path) -> Result<Patterns>`,
    `Patterns::from_json(&str) -> Result<Patterns>`,
    `Patterns::classify(&self, tail: &str) -> (Status, Confidence)`
  - `agents::{KNOWN_KINDS, is_known_kind, resume_argv}`

- [ ] **Step 1: Write project config**

`Cargo.toml`:

```toml
[package]
name = "multiplexer-driver"
version = "0.1.0"
edition = "2024"
rust-version = "1.98"
description = "Drive tmux and Herdr workspaces, panes, and coding agents from one JSON CLI"
license = "MIT"

[dependencies]
clap = { version = "4", features = ["derive", "env"] }
regex = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[dev-dependencies]
insta = "1"
rstest = "0.26"

[lints.rust]
missing_docs = "warn"

[lints.clippy]
all = { level = "deny", priority = -1 }
expect_used = "deny"
missing_docs_in_private_items = "warn"
missing_errors_doc = "warn"
missing_panics_doc = "warn"
unwrap_used = "deny"
```

If `rstest = "0.26"` does not resolve, use the newest published
version and say so in the commit message.

`rustfmt.toml`:

```toml
max_width = 79
```

`clippy.toml`:

```toml
allow-expect-in-tests = true
allow-unwrap-in-tests = true
```

`.gitignore`:

```
/target
*.snap.new
```

- [ ] **Step 2: Write `src/lib.rs` (module list only for now)**

```rust
//! Drive terminal multiplexer harnesses (tmux, Herdr) and the coding
//! agents inside them through one vocabulary of workspaces, tabs, and
//! panes. See the design spec in `docs/superpowers/specs/`.

pub mod agents;
pub mod driver;
pub mod model;
pub mod patterns;
pub mod text;
```

- [ ] **Step 3: Write `src/model.rs`**

```rust
//! Serializable records and small value types shared by every driver
//! and printed by the CLI. Field names are the JSON contract (§5.3).

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

/// A stable pane address: a tmux pane ID (`%12`) or a Herdr pane ID
/// (`w1:p2`). It never changes on rename.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Handle(pub String);

impl fmt::Display for Handle {
    /// Writes the raw handle string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An agent's coarse execution state.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The agent is busy.
    Working,
    /// The agent waits for input.
    Idle,
    /// The agent waits on a dialog or permission prompt.
    Blocked,
    /// The state cannot be determined.
    #[value(skip)]
    Unknown,
}

/// How far a reported value can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// The harness tracks it directly.
    Native,
    /// Derived from pattern matching on pane text.
    Heuristic,
    /// No signal at all.
    None,
}

/// What an agent session reference is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionRefKind {
    /// A conversation ID.
    Id,
    /// A session file path.
    Path,
}

/// A reference to an agent's conversation, used to resume it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Whether `value` is an ID or a path.
    pub kind: SessionRefKind,
    /// The ID or path.
    pub value: String,
}

/// One workspace (Herdr workspace, tmux session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceRecord {
    /// Harness workspace ID (`w1`, `$3`).
    pub workspace_id: String,
    /// Display label.
    pub label: String,
    /// Whether the user currently looks at it.
    pub focused: bool,
    /// Tags set through `workspace tag` (and, on Herdr, other tools).
    pub tags: BTreeMap<String, String>,
}

/// One pane with everything needed to rebuild it later (§5.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneRecord {
    /// The pane handle.
    pub handle: Handle,
    /// Workspace ID.
    pub workspace_id: String,
    /// Workspace label, when known.
    pub workspace_label: Option<String>,
    /// Tab ID (Herdr tab, tmux window).
    pub tab_id: String,
    /// Tab label, when known.
    pub tab_label: Option<String>,
    /// Pane label, when one was set.
    pub label: Option<String>,
    /// Current working directory of the pane.
    pub cwd: Option<String>,
    /// Agent kind (`claude`, `codex`, …), when known.
    pub agent: Option<String>,
    /// Agent conversation reference, when known.
    pub agent_session: Option<AgentSession>,
    /// Trust in `agent_session`.
    pub session_confidence: Confidence,
    /// Agent status.
    pub status: Status,
    /// Trust in `status`.
    pub status_confidence: Confidence,
}

/// The answer to `pane status` and `pane wait`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusResult {
    /// The pane asked about.
    pub handle: Handle,
    /// Its status.
    pub status: Status,
    /// Trust in `status`.
    pub confidence: Confidence,
    /// Last lines of output for non-native results, else `None`.
    pub tail: Option<String>,
}

/// The answer to `pane spawn` and `pane split`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartResult {
    /// The new pane.
    pub handle: Handle,
    /// False when the pane exists but its command did not start.
    pub started: bool,
}

/// Which side of the target pane a split opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Direction {
    /// To the right.
    Right,
    /// Below.
    Down,
}
```

Note: `model.rs` uses `clap::ValueEnum`, so `clap` is already a
dependency here. That is intended (one crate).

- [ ] **Step 4: Write the failing tests for `model`**

Append to `src/model.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    mod pane_record {
        use super::*;

        #[test]
        fn should_serialize_nulls_when_fields_unknown() {
            let record = PaneRecord {
                handle: Handle("%1".into()),
                workspace_id: "$0".into(),
                workspace_label: None,
                tab_id: "@0".into(),
                tab_label: None,
                label: None,
                cwd: None,
                agent: None,
                agent_session: None,
                session_confidence: Confidence::None,
                status: Status::Unknown,
                status_confidence: Confidence::None,
            };

            let json = serde_json::to_string(&record).unwrap();

            assert_eq!(
                json,
                r#"{"handle":"%1","workspace_id":"$0","workspace_label":null,"tab_id":"@0","tab_label":null,"label":null,"cwd":null,"agent":null,"agent_session":null,"session_confidence":"none","status":"unknown","status_confidence":"none"}"#,
            );
        }
    }
}
```

- [ ] **Step 5: Run the model test**

Run: `cargo test --locked model::`
Expected: PASS (the types already exist; this pins the JSON contract).
If it fails, fix the serde attributes, not the expected string.

- [ ] **Step 6: Write `src/driver.rs` with tests first**

```rust
//! The harness abstraction: one error type, one trait, and the shared
//! wait loop (§4.2, §5.5).

use std::{
    collections::BTreeMap,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use crate::model::{
    Confidence, Direction, Handle, PaneRecord, Status, StatusResult,
    WorkspaceRecord,
};

/// Result alias using [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every failure the CLI reports. One variant per error `type`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bad flags, arguments, or input files.
    #[error("{0}")]
    Usage(String),
    /// The harness binary, server, or session is not reachable.
    #[error("{0}")]
    HarnessUnavailable(String),
    /// The target workspace or pane does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The harness cannot do this operation.
    #[error("{0}")]
    Unsupported(String),
    /// The pane exists but its command did not start.
    #[error("{message}")]
    StartFailed {
        /// The pane that was created.
        handle: Handle,
        /// What went wrong.
        message: String,
    },
    /// `pane wait` reached its deadline.
    #[error("{0}")]
    Timeout(String),
    /// Anything else, including unparseable harness output.
    #[error("{0}")]
    Unexpected(String),
}

impl Error {
    /// The `type` string of the JSON error record.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::HarnessUnavailable(_) => "harness_unavailable",
            Self::NotFound(_) => "not_found",
            Self::Unsupported(_) => "unsupported",
            Self::StartFailed { .. } => "start_failed",
            Self::Timeout(_) => "timeout",
            Self::Unexpected(_) => "unexpected",
        }
    }

    /// The process exit code for this error (§5.5).
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Unexpected(_) => 1,
            Self::Usage(_) => 2,
            Self::HarnessUnavailable(_) => 3,
            Self::NotFound(_) => 4,
            Self::Unsupported(_) => 5,
            Self::StartFailed { .. } => 6,
            Self::Timeout(_) => 7,
        }
    }
}

/// Builds the error a driver returns for an operation it lacks.
pub fn unsupported(harness: &str, operation: &str) -> Error {
    Error::Unsupported(format!("{operation} is not supported on {harness}"))
}

/// Arguments for [`Driver::pane_spawn`].
#[derive(Debug)]
pub struct SpawnRequest<'a> {
    /// Label for the new tab and pane.
    pub name: &'a str,
    /// Target workspace; `None` means the driver's default.
    pub workspace: Option<&'a str>,
    /// Working directory for the command.
    pub cwd: &'a Path,
    /// Command and arguments; may be empty on tmux (default shell).
    pub command: &'a [String],
}

/// Arguments for [`Driver::pane_split`].
#[derive(Debug)]
pub struct SplitRequest<'a> {
    /// Pane to split.
    pub target: &'a Handle,
    /// Side of the new pane.
    pub direction: Direction,
    /// Label for the new pane.
    pub name: &'a str,
    /// Working directory for the command.
    pub cwd: &'a Path,
    /// Command and arguments; may be empty on tmux.
    pub command: &'a [String],
}

/// One harness backend. Every method defaults to [`Error::Unsupported`]
/// so a new harness implements only what it can do.
pub trait Driver {
    /// Harness name for messages (`tmux`, `herdr`).
    fn name(&self) -> &'static str;

    /// Lists every workspace.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn workspace_list(&self) -> Result<Vec<WorkspaceRecord>> {
        Err(unsupported(self.name(), "workspace list"))
    }

    /// Creates a workspace with `label` rooted at `cwd`.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn workspace_create(
        &self,
        _label: &str,
        _cwd: &Path,
    ) -> Result<WorkspaceRecord> {
        Err(unsupported(self.name(), "workspace create"))
    }

    /// Closes the workspace `id` and everything in it.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn workspace_close(&self, _id: &str) -> Result<()> {
        Err(unsupported(self.name(), "workspace close"))
    }

    /// Sets `set` pairs and removes `clear` keys, then returns the
    /// workspace's tags as they are now.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn workspace_tag(
        &self,
        _id: &str,
        _set: &[(String, String)],
        _clear: &[String],
    ) -> Result<BTreeMap<String, String>> {
        Err(unsupported(self.name(), "workspace tag"))
    }

    /// Lists every pane, optionally only those in `workspace`.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn pane_list(&self, _workspace: Option<&str>) -> Result<Vec<PaneRecord>> {
        Err(unsupported(self.name(), "pane list"))
    }

    /// Opens a new tab running the command and returns its pane.
    ///
    /// # Errors
    ///
    /// [`Error::StartFailed`] when the pane exists but the command did
    /// not start; other harness failures; `Unsupported` by default.
    fn pane_spawn(&self, _request: &SpawnRequest<'_>) -> Result<Handle> {
        Err(unsupported(self.name(), "pane spawn"))
    }

    /// Opens a new pane beside the target and returns it.
    ///
    /// # Errors
    ///
    /// As [`Driver::pane_spawn`].
    fn pane_split(&self, _request: &SplitRequest<'_>) -> Result<Handle> {
        Err(unsupported(self.name(), "pane split"))
    }

    /// Returns the last `lines` lines of output, trailing blank lines
    /// trimmed; with `ansi`, escape codes are kept.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn pane_read(
        &self,
        _target: &Handle,
        _lines: u32,
        _ansi: bool,
    ) -> Result<String> {
        Err(unsupported(self.name(), "pane read"))
    }

    /// Sends `text` and submits it. Does not wait.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn pane_prompt(&self, _target: &Handle, _text: &str) -> Result<()> {
        Err(unsupported(self.name(), "pane prompt"))
    }

    /// Sets the pane label. The handle does not change.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn pane_rename(&self, _target: &Handle, _label: &str) -> Result<()> {
        Err(unsupported(self.name(), "pane rename"))
    }

    /// Sends Ctrl-C.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn pane_interrupt(&self, _target: &Handle) -> Result<()> {
        Err(unsupported(self.name(), "pane interrupt"))
    }

    /// Closes this pane only, never its tab or workspace.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn pane_kill(&self, _target: &Handle) -> Result<()> {
        Err(unsupported(self.name(), "pane kill"))
    }

    /// Reports the agent status in the pane.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn pane_status(&self, _target: &Handle) -> Result<StatusResult> {
        Err(unsupported(self.name(), "pane status"))
    }

    /// Runs the harness binary with `args` (session applied) and
    /// returns its stdout unchanged.
    ///
    /// # Errors
    ///
    /// Harness failures; [`Error::Unsupported`] by default.
    fn raw(&self, _args: &[String]) -> Result<String> {
        Err(unsupported(self.name(), "raw"))
    }
}

/// Polls [`Driver::pane_status`] every `interval` (never sleeping past
/// the deadline) until the status is in `until`, and returns that
/// result. A result that arrives after the deadline is a timeout, even
/// when it matches.
///
/// # Errors
///
/// [`Error::Usage`] when the status has no signal (`Confidence::None`,
/// e.g. tmux without patterns) or `timeout` overflows the clock,
/// [`Error::Timeout`] past `timeout`, and any error from `pane_status`.
pub fn wait_for(
    driver: &dyn Driver,
    target: &Handle,
    until: &[Status],
    timeout: Duration,
    interval: Duration,
) -> Result<StatusResult> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        Error::Usage(format!("--timeout {timeout:?} is too large"))
    })?;

    loop {
        let result = driver.pane_status(target)?;

        if result.confidence == Confidence::None {
            return Err(Error::Usage(format!(
                "{} has no status signal for {target}; pass --patterns",
                driver.name()
            )));
        }

        let now = Instant::now();

        if now > deadline {
            return Err(Error::Timeout(format!(
                "{target} did not reach {until:?} within {timeout:?}"
            )));
        }

        if until.contains(&result.status) {
            return Ok(result);
        }

        thread::sleep(interval.min(deadline - now));
    }
}
```

Tests to append (write them, run, they must pass):

```rust
#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// A driver whose `pane_status` replays a fixed script.
    struct Scripted {
        /// Statuses to return, in order; the last one repeats.
        script: RefCell<Vec<(Status, Confidence)>>,
        /// How long each `pane_status` call takes.
        delay: Duration,
    }

    impl Driver for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn pane_status(&self, target: &Handle) -> Result<StatusResult> {
            thread::sleep(self.delay);

            let mut script = self.script.borrow_mut();
            let (status, confidence) = if script.len() > 1 {
                script.remove(0)
            } else {
                script[0]
            };

            Ok(StatusResult {
                handle: target.clone(),
                status,
                confidence,
                tail: None,
            })
        }
    }

    /// Builds a [`Scripted`] driver.
    fn scripted(script: Vec<(Status, Confidence)>) -> Scripted {
        Scripted {
            script: RefCell::new(script),
            delay: Duration::ZERO,
        }
    }

    mod error {
        use super::*;

        #[test]
        fn should_map_every_variant_to_its_exit_code() {
            let codes: Vec<(u8, &str)> = [
                Error::Unexpected(String::new()),
                Error::Usage(String::new()),
                Error::HarnessUnavailable(String::new()),
                Error::NotFound(String::new()),
                Error::Unsupported(String::new()),
                Error::StartFailed {
                    handle: Handle("h".into()),
                    message: String::new(),
                },
                Error::Timeout(String::new()),
            ]
            .iter()
            .map(|e| (e.exit_code(), e.kind()))
            .collect();

            assert_eq!(
                codes,
                vec![
                    (1, "unexpected"),
                    (2, "usage"),
                    (3, "harness_unavailable"),
                    (4, "not_found"),
                    (5, "unsupported"),
                    (6, "start_failed"),
                    (7, "timeout"),
                ]
            );
        }
    }

    mod wait_for {
        use super::*;

        #[test]
        fn should_return_when_status_reaches_until() {
            let driver = scripted(vec![
                (Status::Working, Confidence::Native),
                (Status::Idle, Confidence::Native),
            ]);

            let result = wait_for(
                &driver,
                &Handle("p".into()),
                &[Status::Idle],
                Duration::from_secs(1),
                Duration::from_millis(1),
            )
            .unwrap();

            assert_eq!(result.status, Status::Idle);
        }

        #[test]
        fn should_time_out_when_status_never_matches() {
            let driver = scripted(vec![(Status::Working, Confidence::Native)]);

            let err = wait_for(
                &driver,
                &Handle("p".into()),
                &[Status::Idle],
                Duration::from_millis(20),
                Duration::from_millis(5),
            )
            .unwrap_err();

            assert!(matches!(err, Error::Timeout(_)), "got {err:?}");
        }

        #[test]
        fn should_time_out_near_deadline_when_timeout_below_interval() {
            let driver = scripted(vec![(Status::Working, Confidence::Native)]);
            let started = Instant::now();

            let err = wait_for(
                &driver,
                &Handle("p".into()),
                &[Status::Idle],
                Duration::from_millis(30),
                Duration::from_millis(500),
            )
            .unwrap_err();

            assert!(
                matches!(err, Error::Timeout(_))
                    && started.elapsed() < Duration::from_millis(250),
                "got {err:?} after {:?}",
                started.elapsed()
            );
        }

        #[test]
        fn should_time_out_when_match_arrives_after_deadline() {
            let driver = Scripted {
                script: RefCell::new(vec![(Status::Idle, Confidence::Native)]),
                delay: Duration::from_millis(60),
            };

            let err = wait_for(
                &driver,
                &Handle("p".into()),
                &[Status::Idle],
                Duration::from_millis(20),
                Duration::from_millis(5),
            )
            .unwrap_err();

            assert!(matches!(err, Error::Timeout(_)), "got {err:?}");
        }

        #[test]
        fn should_fail_usage_when_timeout_overflows() {
            let driver = scripted(vec![(Status::Idle, Confidence::Native)]);

            let err = wait_for(
                &driver,
                &Handle("p".into()),
                &[Status::Idle],
                Duration::MAX,
                Duration::from_millis(5),
            )
            .unwrap_err();

            assert!(matches!(err, Error::Usage(_)), "got {err:?}");
        }

        #[test]
        fn should_fail_usage_when_status_has_no_signal() {
            let driver = scripted(vec![(Status::Unknown, Confidence::None)]);

            let err = wait_for(
                &driver,
                &Handle("p".into()),
                &[Status::Idle],
                Duration::from_secs(1),
                Duration::from_millis(1),
            )
            .unwrap_err();

            assert!(matches!(err, Error::Usage(_)), "got {err:?}");
        }
    }
}
```

Run: `cargo test --locked driver::`
Expected: PASS (7 tests).

- [ ] **Step 7: Write `src/text.rs` test-first**

Tests (write first, run, see them fail to compile, then implement):

```rust
#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod trim_trailing_blank_lines {
        use super::*;

        #[rstest]
        #[case::padded("a\nb\n\n  \n\t\n", "a\nb")]
        #[case::inner_blank_kept("a\n\nb\n", "a\n\nb")]
        #[case::all_blank("\n\n", "")]
        #[case::empty("", "")]
        #[case::no_newline("a", "a")]
        fn should_drop_only_trailing_blank_lines(
            #[case] input: &str,
            #[case] expected: &str,
        ) {
            assert_eq!(trim_trailing_blank_lines(input), expected);
        }
    }

    mod shell_join {
        use super::*;

        #[rstest]
        #[case::safe(&["claude", "--model", "opus"], "claude --model opus")]
        #[case::space(&["echo", "a b"], "echo 'a b'")]
        #[case::quote(&["echo", "it's"], r"echo 'it'\''s'")]
        #[case::empty_arg(&["echo", ""], "echo ''")]
        #[case::path_and_eq(&["./x.sh", "K=v"], "./x.sh K=v")]
        #[case::dollar(&["echo", "$HOME"], "echo '$HOME'")]
        fn should_quote_only_unsafe_args(
            #[case] args: &[&str],
            #[case] expected: &str,
        ) {
            let owned: Vec<String> =
                args.iter().map(|s| (*s).to_string()).collect();

            assert_eq!(shell_join(&owned), expected);
        }
    }
}
```

Implementation (§6):

```rust
//! Text helpers for terminal captures and shell command lines (§6).

use std::sync::LazyLock;

use regex::Regex;

/// Arguments made only of these characters need no quoting.
static SHELL_SAFE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_./=-]+$").ok());

/// Removes the trailing blank lines a terminal capture pads its output
/// with. Inner blank lines stay.
pub fn trim_trailing_blank_lines(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |i| i + 1);

    lines[..end].join("\n")
}

/// Renders `args` as one POSIX shell command line.
pub fn shell_join(args: &[String]) -> String {
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// Quotes one argument unless it is non-empty and shell-safe.
fn quote(arg: &str) -> String {
    let safe = SHELL_SAFE.as_ref().is_some_and(|re| re.is_match(arg));

    if safe {
        return arg.to_string();
    }

    format!("'{}'", arg.replace('\'', r"'\''"))
}
```

(`LazyLock<Option<Regex>>` keeps `unwrap` out of non-test code. The
regex is a constant, so `None` never happens, and an unsafe fallback
of "always quote" is still correct.)

Run: `cargo test --locked text::`
Expected: PASS (11 cases).

- [ ] **Step 8: Write `src/patterns.rs` test-first**

Tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    mod classify {
        use super::*;

        #[test]
        fn should_report_none_when_no_patterns() {
            let patterns = Patterns::default();

            assert_eq!(
                patterns.classify("anything"),
                (Status::Unknown, Confidence::None)
            );
        }

        #[test]
        fn should_prefer_blocked_when_both_match() {
            let patterns = Patterns::from_json(
                r#"{"idle":["\\$\\s*$"],"blocked":["Trust this folder\\?"]}"#,
            )
            .unwrap();

            assert_eq!(
                patterns.classify("Trust this folder?\n$ "),
                (Status::Blocked, Confidence::Heuristic)
            );
        }

        #[test]
        fn should_report_idle_when_idle_matches() {
            let patterns =
                Patterns::from_json(r#"{"idle":["\\$\\s*$"]}"#).unwrap();

            assert_eq!(
                patterns.classify("done\n$ "),
                (Status::Idle, Confidence::Heuristic)
            );
        }

        #[test]
        fn should_report_working_when_nothing_matches() {
            let patterns =
                Patterns::from_json(r#"{"idle":["^never$"]}"#).unwrap();

            assert_eq!(
                patterns.classify("compiling"),
                (Status::Working, Confidence::Heuristic)
            );
        }
    }

    mod from_json {
        use super::*;

        #[test]
        fn should_fail_usage_when_regex_invalid() {
            let err = Patterns::from_json(r#"{"idle":["("]}"#).unwrap_err();

            assert!(matches!(err, Error::Usage(_)), "got {err:?}");
        }
    }
}
```

Implementation:

```rust
//! The status patterns file: regexes that classify a pane's recent
//! output on harnesses without native status (§7.3).

use std::{fs, path::Path};

use regex::Regex;
use serde::Deserialize;

use crate::{
    driver::{Error, Result},
    model::{Confidence, Status},
};

/// Compiled idle and blocked patterns. The default has none.
#[derive(Debug, Default)]
pub struct Patterns {
    /// Matches output that means "waiting for input".
    idle: Vec<Regex>,
    /// Matches output that means "stuck on a dialog".
    blocked: Vec<Regex>,
}

/// The file format before compilation.
#[derive(Deserialize)]
struct PatternsFile {
    /// Idle regexes.
    #[serde(default)]
    idle: Vec<String>,
    /// Blocked regexes.
    #[serde(default)]
    blocked: Vec<String>,
}

impl Patterns {
    /// Reads and compiles the patterns file at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] when the file is unreadable, not JSON, or holds
    /// an invalid regex.
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| {
            Error::Usage(format!("reading --patterns {}: {e}", path.display()))
        })?;

        Self::from_json(&text)
    }

    /// Compiles patterns from JSON text.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] on bad JSON or an invalid regex.
    pub fn from_json(text: &str) -> Result<Self> {
        let file: PatternsFile = serde_json::from_str(text)
            .map_err(|e| Error::Usage(format!("parsing --patterns: {e}")))?;

        Ok(Self {
            idle: compile(&file.idle)?,
            blocked: compile(&file.blocked)?,
        })
    }

    /// Classifies `tail`: blocked first, then idle, else working.
    pub fn classify(&self, tail: &str) -> (Status, Confidence) {
        if self.idle.is_empty() && self.blocked.is_empty() {
            return (Status::Unknown, Confidence::None);
        }

        if self.blocked.iter().any(|re| re.is_match(tail)) {
            return (Status::Blocked, Confidence::Heuristic);
        }

        if self.idle.iter().any(|re| re.is_match(tail)) {
            return (Status::Idle, Confidence::Heuristic);
        }

        (Status::Working, Confidence::Heuristic)
    }
}

/// Compiles each source regex.
fn compile(sources: &[String]) -> Result<Vec<Regex>> {
    sources
        .iter()
        .map(|s| {
            Regex::new(s).map_err(|e| {
                Error::Usage(format!("invalid pattern {s:?}: {e}"))
            })
        })
        .collect()
}
```

Note: the idle example `\$\s*$` must match at end of the *whole* tail,
so the regex is not multi-line. That is the intended behavior.

Run: `cargo test --locked patterns::`
Expected: PASS (5 tests).

- [ ] **Step 9: Write `src/agents.rs` test-first**

Tests:

```rust
#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod resume_argv {
        use super::*;

        #[rstest]
        #[case::claude("claude", "abc", &["claude", "--resume", "abc"])]
        #[case::codex("codex", "abc", &["codex", "resume", "abc"])]
        #[case::pi("pi", "/s/x.jsonl", &["pi", "--session", "/s/x.jsonl"])]
        fn should_build_argv_when_kind_known(
            #[case] kind: &str,
            #[case] reference: &str,
            #[case] expected: &[&str],
        ) {
            assert_eq!(resume_argv(kind, reference).unwrap(), expected);
        }

        #[test]
        fn should_fail_unsupported_when_kind_unknown() {
            let err = resume_argv("gemini", "abc").unwrap_err();

            assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
        }
    }

    mod is_known_kind {
        use super::*;

        #[test]
        fn should_know_all_22_herdr_kinds() {
            assert_eq!(KNOWN_KINDS.len(), 22);
            assert!(is_known_kind("maki") && !is_known_kind("bash"));
        }
    }
}
```

Implementation:

```rust
//! Agent knowledge shared by every driver: which agent kinds exist and
//! how to resume a conversation (§4.5).

use crate::driver::{Error, Result};

/// Agent kinds `herdr agent start --kind` accepts (Herdr 0.8.2). The
/// tmux driver uses the same list to recognize an agent command.
pub const KNOWN_KINDS: [&str; 22] = [
    "pi", "claude", "codex", "gemini", "cursor", "devin", "agy", "cline",
    "omp", "mastracode", "opencode", "copilot", "kimi", "kiro", "droid",
    "amp", "grok", "hermes", "kilo", "qodercli", "qwen", "maki",
];

/// Whether `command` names a known agent kind.
pub fn is_known_kind(command: &str) -> bool {
    KNOWN_KINDS.contains(&command)
}

/// Returns the argv that resumes `reference` for agent `kind`.
/// Verified against each tool's `--help` on 2026-09-30.
///
/// # Errors
///
/// [`Error::Unsupported`] for a kind with no verified resume form.
pub fn resume_argv(kind: &str, reference: &str) -> Result<Vec<String>> {
    let parts: [&str; 2] = match kind {
        "claude" => ["claude", "--resume"],
        "codex" => ["codex", "resume"],
        "pi" => ["pi", "--session"],
        _ => {
            return Err(Error::Unsupported(format!(
                "no verified resume command for agent kind {kind:?}"
            )));
        }
    };

    Ok(vec![parts[0].into(), parts[1].into(), reference.into()])
}
```

Run: `cargo test --locked agents::`
Expected: PASS.

- [ ] **Step 10: Run all checks and commit**

Run: `cargo fmt && cargo fmt --check && cargo clippy --all-targets
--all-features --locked -- -D warnings && cargo test --locked`
(first run without `--locked` once to create `Cargo.lock`).
Expected: all pass, no warnings. Fix any missing doc comment warnings.

```bash
git add Cargo.toml Cargo.lock rustfmt.toml clippy.toml .gitignore LICENSE src
git commit -m "add core library: model, driver trait, text, patterns, agents"
```

---

### Task 2: CLI, dispatch, notify, driver stubs

**Files:**
- Create: `src/cli.rs`, `src/main.rs`, `src/notify.rs`, `src/tmux.rs`
  (stub), `src/herdr.rs` (stub), `tests/cli.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: everything Task 1 produces.
- Produces:
  - `cli::Cli` (clap `Parser`), `cli::parse_duration(&str) ->
    std::result::Result<Duration, String>`
  - `lib::Harness` enum (`Tmux`, `Herdr`, clap `ValueEnum`)
  - `lib::driver_for(harness: Harness, session: Option<&str>,
    patterns: Option<Patterns>) -> Result<Box<dyn Driver>>`
  - `lib::run(cli: Cli, out: &mut dyn std::io::Write) -> Result<()>`
  - `lib::error_record(err: &Error) -> String` (one JSON line, no `\n`)
  - `tmux::Tmux::new(session: Option<String>, patterns: Option<Patterns>)
    -> Tmux` and `herdr::Herdr::new(session: Option<String>) -> Herdr`,
    both `impl Driver` with only `name()` for now.
  - `notify::{send, NotifyResult}`

- [ ] **Step 1: Write the driver stubs**

`src/tmux.rs`:

```rust
//! tmux driver (§7). Workspaces are sessions, tabs are windows, and
//! handles are tmux pane IDs (`%12`).

use crate::{driver::Driver, patterns::Patterns};

/// Drives one tmux server.
#[derive(Debug)]
pub struct Tmux {
    /// Socket name for `-L`; `None` uses the ambient server.
    session: Option<String>,
    /// Status patterns; `None` means status has no signal.
    patterns: Option<Patterns>,
}

impl Tmux {
    /// Creates a driver for the server named `session` (or the
    /// ambient one).
    pub fn new(session: Option<String>, patterns: Option<Patterns>) -> Self {
        Self { session, patterns }
    }
}

impl Driver for Tmux {
    fn name(&self) -> &'static str {
        "tmux"
    }
}
```

`src/herdr.rs`:

```rust
//! Herdr driver (§8). Handles are Herdr pane IDs (`w1:p2`).

use crate::driver::Driver;

/// Drives one Herdr server.
#[derive(Debug)]
pub struct Herdr {
    /// Session name; `None` uses the ambient `HERDR_SOCKET_PATH`.
    session: Option<String>,
}

impl Herdr {
    /// Creates a driver for the Herdr session `session` (or the
    /// ambient one).
    pub fn new(session: Option<String>) -> Self {
        Self { session }
    }
}

impl Driver for Herdr {
    fn name(&self) -> &'static str {
        "herdr"
    }
}
```

Unused fields warn. Add `#[expect(dead_code, reason = "filled in by
Task 3/5")]` on each struct. Tasks 3 and 5 remove it.

- [ ] **Step 2: Write `src/cli.rs`**

Command tree exactly as §5.2. Use clap derive. Global args with
`global = true`. Skeleton (complete it for every command in §5.2; each
variant field has a `///` doc comment, which clap turns into help):

```rust
//! Command-line definitions. Help text comes from these doc comments
//! (§5.6).

use std::{path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand};

use crate::{
    Harness,
    model::{Direction, Status},
};

/// Drive tmux and Herdr workspaces, panes, and coding agents.
///
/// stdout carries JSON (one object, or JSON Lines for lists). On failure
/// the last stderr line is {"error":{"type":…,"message":…}}.
///
/// Exit codes: 0 ok, 1 unexpected, 2 usage, 3 harness_unavailable,
/// 4 not_found, 5 unsupported, 6 start_failed (handle still on
/// stdout), 7 timeout, 130 interrupted.
#[derive(Debug, Parser)]
#[command(name = "multiplexer-driver", version)]
pub struct Cli {
    /// Harness to drive.
    #[arg(long, global = true, env = "MULTIPLEXER_DRIVER_HARNESS")]
    pub harness: Option<Harness>,
    /// Harness server to use: a Herdr session name or a tmux -L socket
    /// name. Unset uses the ambient server.
    #[arg(long, global = true, env = "MULTIPLEXER_DRIVER_SESSION")]
    pub session: Option<String>,
    /// Indent single-object output. Lists stay JSON Lines.
    #[arg(long, global = true)]
    pub pretty: bool,
    /// Diagnostics level on stderr.
    #[arg(long, global = true, default_value = "warn")]
    pub log_level: String,
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level nouns.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Workspaces (Herdr workspaces, tmux sessions).
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// Panes and the agents in them.
    #[command(subcommand)]
    Pane(PaneCommand),
    /// Agent knowledge that needs no harness.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Show a desktop notification to the user.
    Notify(NotifyArgs),
    /// Run the harness CLI directly, with --session applied; prints its
    /// stdout unchanged. Example: multiplexer-driver --harness herdr raw -- tab list --workspace w1
    Raw {
        /// Arguments passed to the harness binary.
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
}

/// `workspace` verbs.
#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// List workspaces as JSON Lines of WorkspaceRecord.
    List,
    /// Create a workspace; prints its WorkspaceRecord.
    Create {
        /// Workspace label (tmux session name).
        #[arg(long)]
        label: String,
        /// Root directory; defaults to the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Close a workspace and everything in it.
    Close {
        /// Workspace ID.
        workspace: String,
    },
    /// Set and clear workspace tags; prints the resulting tags.
    Tag {
        /// Workspace ID.
        workspace: String,
        /// key=value pairs to set.
        pairs: Vec<String>,
        /// Keys to remove (repeatable).
        #[arg(long = "clear")]
        clear: Vec<String>,
    },
}

/// `pane` verbs. Fill in all of: List{workspace}, Spawn(SpawnArgs),
/// Split(SplitArgs), Read{handle, lines=30, ansi}, Prompt{handle, text},
/// Rename{handle, label}, Interrupt{handle}, Kill{handle},
/// Status{handle, patterns}, Wait{handle, timeout (value_parser =
/// parse_duration, required), until (repeatable Status), patterns}.
#[derive(Debug, Subcommand)]
pub enum PaneCommand {
    // Write every variant listed above, each with a doc comment that
    // states the output shape and one example.
}

// SpawnArgs: name (required), workspace: Option<String>,
// cwd: Option<PathBuf>, command: Vec<String> (#[arg(last = true)]).
// SplitArgs: handle (positional), name, direction (default Right),
// cwd, command (last = true).
// AgentCommand::ResumeArgs { kind: String, session_ref: String }.
// NotifyArgs { message, title (default "Overseer"), sound: Option<String> }.

/// Parses `500ms`, `60s`, or `5m` into a [`Duration`].
///
/// # Errors
///
/// A message for clap when the text has no valid unit or number.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{text:?}: missing unit (ms, s, m)"))?;
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{text:?}: missing number"))?;

    match unit {
        "ms" => Ok(Duration::from_millis(n)),
        "s" => Ok(Duration::from_secs(n)),
        "m" => n
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(|| format!("{text:?}: too large")),
        _ => Err(format!("{text:?}: unit must be ms, s, or m")),
    }
}
```

The comments inside `PaneCommand` above are instructions to you, not
code to keep. Write every variant. Non-empty rule (§5.2): for
`--text`, `--label`, `--name`, `--message` use
`#[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]`.
`--until` default when empty: `[Idle, Blocked]` (applied in `run`).

Tests for `parse_duration` in `cli.rs`:

```rust
#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod parse_duration {
        use super::*;

        #[rstest]
        #[case::ms("500ms", Duration::from_millis(500))]
        #[case::s("60s", Duration::from_secs(60))]
        #[case::m("5m", Duration::from_secs(300))]
        fn should_parse_when_unit_valid(
            #[case] text: &str,
            #[case] expected: Duration,
        ) {
            assert_eq!(parse_duration(text).unwrap(), expected);
        }

        #[rstest]
        #[case::no_unit("60")]
        #[case::bad_unit("5h")]
        #[case::no_number("s")]
        #[case::minutes_overflow("307445734561825861m")]
        #[case::number_overflow("99999999999999999999s")]
        fn should_fail_when_text_invalid(#[case] text: &str) {
            assert!(parse_duration(text).is_err(), "accepted {text:?}");
        }
    }
}
```

- [ ] **Step 3: Write `src/notify.rs` (§9)**

```rust
//! Desktop notifications for the user, independent of any harness
//! (§9).

use std::{env, process::Command};

use serde::Serialize;

use crate::driver::{Error, Result};

/// How, or whether, a notification was shown.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct NotifyResult {
    /// Whether the user saw it.
    pub sent: bool,
    /// `herdr`, `notify-send`, `osascript`, or `none`.
    pub method: &'static str,
    /// Herdr's reason when it did not show.
    pub reason: Option<String>,
}
```

Implement `pub fn send(title: &str, message: &str, sound: Option<&str>)
-> Result<NotifyResult>` with the four steps of §9. Split into private
fns `via_herdr`, `via_notify_send`, `via_osascript`, each returning
`Result<Option<NotifyResult>>` (`None` = tool not available, try the
next). "On PATH" means `Command::new(tool)` spawn fails with
`io::ErrorKind::NotFound` → `None`. For Herdr, parse stdout
`{"result":{"shown":bool,"reason":string?}}` with a private
`#[derive(Deserialize)]` struct. `osascript` string escaping:

```rust
/// Quotes `s` as an AppleScript string literal.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
```

Unit tests: `applescript_string` with a quote and a backslash, and the
Herdr stdout parser (put the parse in `fn parse_herdr_shown(&str) ->
Result<NotifyResult>`), cases `{"result":{"shown":true}}` and
`{"result":{"shown":false,"reason":"disabled"}}`. Do not call real
notifiers in tests.

- [ ] **Step 4: Write `src/lib.rs` dispatch**

Add to `lib.rs`: `pub mod cli; pub mod herdr; pub mod notify; pub mod
tmux;`, plus:

```rust
/// Harnesses this build can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Harness {
    /// tmux.
    Tmux,
    /// Herdr.
    Herdr,
}

/// Builds the driver for `harness`.
///
/// # Errors
///
/// None today; the `Result` leaves room for harness probes.
pub fn driver_for(
    harness: Harness,
    session: Option<&str>,
    patterns: Option<Patterns>,
) -> Result<Box<dyn Driver>> {
    let session = session.map(str::to_string);

    Ok(match harness {
        Harness::Tmux => Box::new(tmux::Tmux::new(session, patterns)),
        Harness::Herdr => Box::new(herdr::Herdr::new(session)),
    })
}

/// Renders the one-line JSON error record for `err` (§5.5).
pub fn error_record(err: &Error) -> String {
    serde_json::json!({
        "error": { "type": err.kind(), "message": err.to_string() }
    })
    .to_string()
}
```

`pub fn run(cli: Cli, out: &mut dyn Write) -> Result<()>` does:

1. `notify`, `agent resume-args`: no driver; call `notify::send` /
   `agents::resume_argv` and print `{"argv":[…]}`.
2. Otherwise require `cli.harness` (missing → `Error::Usage("--harness
   is required")`). Load `--patterns` with `Patterns::load` when given
   (only `pane status` / `pane wait` carry it). Build the driver.
3. Map each command to one driver call. Default `--cwd` is
   `std::env::current_dir()` (failure → `Unexpected`). Parse tag pairs
   `k=v`, keys matching `^[A-Za-z0-9_-]+$`, else `Usage`. `tag` with no
   pairs and no `--clear` → `Usage`.
4. `pane wait`: `driver::wait_for(&*driver, &handle, &until, timeout,
   Duration::from_millis(500))`.
5. `pane spawn`/`split`: on `Ok(h)` print `StartResult{h, true}`. On
   `Err(Error::StartFailed{handle, ..})` print `StartResult{handle,
   false}` and then return the error.
6. Printing: private `fn print_one(out, &impl Serialize, pretty)` (compact
   or `to_string_pretty`, then `\n`) and `fn print_lines(out,
   &[impl Serialize])` (one compact line each, ignores `--pretty`).
   `raw` writes the harness stdout bytes as is. Map `io::Error` from
   `out` to `Unexpected`.

Output shapes per §5.2 (`{"workspace_id":…,"closed":true}`,
`{"handle":…,"sent":true}`, `{"handle":…,"label":…}`,
`{"handle":…,"interrupted":true}`, `{"handle":…,"killed":true}`,
`{"handle":…,"output":…}`, `{"workspace_id":…,"tags":{…}}`). Use
`serde_json::json!` for these small ones.

- [ ] **Step 5: Write `src/main.rs`**

```rust
//! Entry point: parse arguments, install tracing, run, and map the
//! outcome to an exit code (§5.5).

use std::{
    io::{self, Write},
    process::ExitCode,
};

use clap::Parser;
use multiplexer_driver::{cli::Cli, driver::Error, error_record, run};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => return clap_exit(&e),
    };

    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(EnvFilter::new(&cli.log_level))
        .init();

    let mut stdout = io::stdout().lock();
    let outcome = run(cli, &mut stdout);
    let _ = stdout.flush(); // nothing useful to do if stdout is gone

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => report(&err),
    }
}

/// Prints the error record as the last stderr line.
fn report(err: &Error) -> ExitCode {
    eprintln!("{}", error_record(err));

    ExitCode::from(err.exit_code())
}

/// Handles clap's help, version, and usage errors.
fn clap_exit(e: &clap::Error) -> ExitCode {
    use clap::error::ErrorKind;

    if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion)
    {
        let _ = e.print(); // help to stdout; nothing to do on failure

        return ExitCode::SUCCESS;
    }

    let _ = e.print(); // clap's message to stderr
    report(&Error::Usage(e.kind().to_string()))
}
```

`eprintln!` here prints the error record, which is program output
(the contract), not a diagnostic. That is allowed.

- [ ] **Step 6: Write `tests/cli.rs`**

Integration tests run the built binary via
`env!("CARGO_BIN_EXE_multiplexer-driver")` with `std::process::Command`.
Clear `MULTIPLEXER_DRIVER_HARNESS` and `MULTIPLEXER_DRIVER_SESSION`
from the child env in a helper `fn md(args: &[&str]) -> Output`.

Tests (each one assertion where possible):

1. `should_exit_2_with_usage_record_when_harness_missing`:
   `md(&["pane","read","%1"])` → code 2, last stderr line parses as
   JSON with `error.type == "usage"`.
2. `should_exit_2_when_flag_unknown`: `md(&["pane","read","--bogus"])`
   → code 2 and last stderr line has `"type":"usage"`.
3. `should_print_argv_when_resume_kind_known`:
   `md(&["agent","resume-args","--kind","claude","--session-ref","x"])`
   → stdout exactly `{"argv":["claude","--resume","x"]}\n`, stderr
   empty, code 0.
4. `should_exit_5_when_resume_kind_unknown`: kind `gemini` → code 5.
5. `should_exit_2_when_wait_timeout_missing`:
   `md(&["--harness","tmux","pane","wait","%1"])` → code 2.
6. `should_emit_no_bom_or_cr_when_printing`: stdout of test 3 has no
   `\r` and does not start with `\u{feff}`.
7. Help snapshots with `insta::assert_snapshot!`: stdout of
   `--help`, `workspace --help`, `pane --help`, `agent --help`. Accept
   them with `cargo insta accept` (or `INSTA_UPDATE=always cargo test`
   once) and commit the `tests/snapshots/*.snap` files. Read the
   snapshots before accepting: every verb must be present with its
   doc line.

- [ ] **Step 7: Run all checks, commit**

Run the three checks from Global Constraints. Expected: pass.

```bash
git add src tests
git commit -m "add CLI, dispatch, notify, and driver stubs"
```

---

### Task 3: tmux pane operations

**Files:**
- Modify: `src/tmux.rs`
- Create: `tests/tmux.rs`

**Interfaces:**
- Consumes: `Driver`, `Error`, `SpawnRequest`, `SplitRequest`,
  `Handle`, `StatusResult`, `text::{shell_join,
  trim_trailing_blank_lines}`, `agents::is_known_kind`,
  `Patterns::classify`, `tmux::Tmux::new` from Tasks 1–2.
- Produces: `impl Driver for Tmux` methods `pane_spawn`, `pane_split`,
  `pane_read`, `pane_prompt`, `pane_rename`, `pane_interrupt`,
  `pane_kill`, `pane_status`, `raw`. Private helpers other tmux tasks
  reuse: `fn run(&self, args: &[&str]) -> Result<String>` (applies
  `-L`, retries the server-exit race), `fn classify_error(stderr: &str)
  -> Error`, `fn set_pane_option(&self, pane: &str, key: &str, value:
  &str) -> Result<()>`.

Spec sections: §7.1, §7.2 (pane rows), §7.4, §7.5.

- [ ] **Step 1: Write unit tests for the pure helpers**

In `src/tmux.rs` `#[cfg(test)] mod tests`:

```rust
mod classify_error {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::pane("can't find pane: %99", "not_found")]
    #[case::session("can't find session: $99", "not_found")]
    #[case::window("can't find window: @9", "not_found")]
    #[case::no_server("no server running on /tmp/tmux-1000/x", "harness_unavailable")]
    #[case::no_socket(
        "error connecting to /tmp/tmux-1000/x (No such file or directory)",
        "harness_unavailable"
    )]
    #[case::duplicate("duplicate session: ws", "usage")]
    #[case::other("something odd", "unexpected")]
    fn should_classify_tmux_stderr(#[case] stderr: &str, #[case] kind: &str) {
        assert_eq!(classify_error(stderr).kind(), kind);
    }
}

mod base_args {
    use super::*;

    #[test]
    fn should_prefix_socket_when_session_set() {
        let tmux = Tmux::new(Some("t1".into()), None);

        assert_eq!(tmux.base_args(), vec!["-L", "t1"]);
    }

    #[test]
    fn should_add_nothing_when_session_unset() {
        assert!(Tmux::new(None, None).base_args().is_empty());
    }
}
```

Run: `cargo test --locked tmux::` → FAIL (functions missing).

- [ ] **Step 2: Implement run helper and error classification**

```rust
/// The pane format every pane-creating command prints.
const PANE_ID: &str = "#{pane_id}";

impl Tmux {
    /// Global arguments placed before every subcommand.
    fn base_args(&self) -> Vec<&str> {
        match &self.session {
            Some(name) => vec!["-L", name.as_str()],
            None => Vec::new(),
        }
    }

    /// Runs tmux with `args`, retrying once after 200 ms when the
    /// server was exiting (§7.5).
    fn run(&self, args: &[&str]) -> Result<String> {
        match self.run_once(args) {
            Err(Error::Unexpected(msg))
                if msg.contains("server exited unexpectedly") =>
            {
                thread::sleep(Duration::from_millis(200));

                self.run_once(args)
            }
            other => other,
        }
    }

    /// Runs tmux once and classifies a failure from its stderr.
    fn run_once(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("tmux")
            .args(self.base_args())
            .args(args)
            .output()
            .map_err(|e| {
                Error::HarnessUnavailable(format!("running tmux: {e}"))
            })?;

        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);

        Err(classify_error(stderr.trim()))
    }
}

/// Maps tmux stderr text to an [`Error`] (§7.4).
fn classify_error(stderr: &str) -> Error {
    let message = stderr.to_string();

    if stderr.starts_with("can't find ") {
        return Error::NotFound(message);
    }

    if stderr.starts_with("no server running")
        || stderr.starts_with("error connecting to")
    {
        return Error::HarnessUnavailable(message);
    }

    if stderr.starts_with("duplicate session") {
        return Error::Usage(message);
    }

    Error::Unexpected(message)
}
```

(The "server exited unexpectedly" text is classified `Unexpected`,
which `run` checks for the retry.)

Run: `cargo test --locked tmux::` → PASS.

- [ ] **Step 3: Write the integration test harness**

`tests/tmux.rs`:

```rust
//! tmux integration tests. Each test owns a private tmux server
//! (`-L md-test-<pid>-<n>`) and kills it on drop, so the user's own
//! server is never touched.

use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
    thread,
    time::Duration,
};

use multiplexer_driver::{
    driver::{Driver, SpawnRequest, SplitRequest},
    model::{Direction, Handle},
    tmux::Tmux,
};

/// Counter that makes each test's socket name unique.
static NEXT: AtomicU32 = AtomicU32::new(0);

/// A private tmux server with one session, `ws`, killed on drop.
struct Server {
    /// Socket name.
    name: String,
}

impl Server {
    /// Starts a server, or returns `None` when tmux is not installed.
    fn start() -> Option<Self> {
        let name = format!(
            "md-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let status = Command::new("tmux")
            .args(["-L", &name, "new-session", "-d", "-s", "ws", "-c", "/tmp"])
            .status()
            .ok()?;

        status.success().then_some(Self { name })
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
            .unwrap();

        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux") // best effort cleanup
            .args(["-L", &self.name, "kill-server"])
            .status();
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
```

- [ ] **Step 4: Write failing integration tests**

```rust
#[test]
fn should_spawn_window_and_read_output_when_command_given() {
    let server = server!();
    let driver = server.driver();
    let ws = server.tmux(&["display-message", "-p", "-t", "ws", "#{session_id}"]);
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
    let ws = server.tmux(&["display-message", "-p", "-t", "ws", "#{session_id}"]);
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
        server.tmux(&["display-message", "-p", "-t", &handle.0, "#{@md-label}"]),
        "impl-2"
    );
}

#[test]
fn should_keep_sibling_when_split_pane_killed() {
    let server = server!();
    let driver = server.driver();
    let first = Handle(server.tmux(&["display-message", "-p", "-t", "ws", "#{pane_id}"]));
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

    assert_eq!(server.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]), first.0);
}

#[test]
fn should_type_text_and_submit_when_prompted() {
    let server = server!();
    let driver = server.driver();
    let pane = Handle(server.tmux(&["display-message", "-p", "-t", "ws", "#{pane_id}"]));

    driver.pane_prompt(&pane, "echo prompted-$((40+2))").unwrap();
    thread::sleep(Duration::from_millis(300));

    assert!(driver.pane_read(&pane, 10, false).unwrap().contains("prompted-42"));
}

#[test]
fn should_keep_handle_and_set_label_when_renamed() {
    let server = server!();
    let driver = server.driver();
    let pane = Handle(server.tmux(&["display-message", "-p", "-t", "ws", "#{pane_id}"]));

    driver.pane_rename(&pane, "renamed").unwrap();

    assert_eq!(
        server.tmux(&["display-message", "-p", "-t", &pane.0, "#{@md-label}"]),
        "renamed"
    );
}

#[test]
fn should_report_not_found_when_pane_missing() {
    let server = server!();

    let err = server.driver().pane_read(&Handle("%999".into()), 5, false).unwrap_err();

    assert_eq!(err.kind(), "not_found");
}

#[test]
fn should_report_none_confidence_when_no_patterns() {
    let server = server!();
    let pane = Handle(server.tmux(&["display-message", "-p", "-t", "ws", "#{pane_id}"]));

    let result = server.driver().pane_status(&pane).unwrap();

    assert_eq!(serde_json::to_value(&result).unwrap()["confidence"], "none");
}

#[test]
fn should_classify_idle_when_pattern_matches() {
    let server = server!();
    let pane = Handle(server.tmux(&["display-message", "-p", "-t", "ws", "#{pane_id}"]));
    let patterns = multiplexer_driver::patterns::Patterns::from_json(
        r#"{"idle":["md-2-ready"]}"#,
    )
    .unwrap();
    let driver = Tmux::new(Some(server.name.clone()), Some(patterns));

    // The pattern matches only the output, not the typed command line.
    driver.pane_prompt(&pane, "echo md-$((1+1))-ready").unwrap();
    thread::sleep(Duration::from_millis(300));

    assert_eq!(
        serde_json::to_value(driver.pane_status(&pane).unwrap()).unwrap()["status"],
        "idle"
    );
}

#[test]
fn should_pass_args_through_when_raw() {
    let server = server!();

    let out = server.driver().raw(&cmd(&["list-sessions", "-F", "#{session_name}"])).unwrap();

    assert_eq!(out.trim(), "ws");
}
```

`rustfmt` will rewrap the long lines. `serde_json` is needed in tests:
it is already a normal dependency, so integration tests can use it.

Run: `cargo test --locked --test tmux` → FAIL (Unsupported).

- [ ] **Step 5: Implement the pane operations**

In `impl Driver for Tmux`, following §7.2 exactly:

```rust
fn pane_spawn(&self, request: &SpawnRequest<'_>) -> Result<Handle> {
    let workspace = match request.workspace {
        Some(ws) => ws.to_string(),
        None => self.default_workspace()?,
    };
    let cwd = request.cwd.to_string_lossy();
    let joined = shell_join(request.command);
    let mut args = vec![
        "new-window", "-d", "-t", &workspace, "-n", request.name,
        "-c", &cwd, "-P", "-F", PANE_ID,
    ];

    if !request.command.is_empty() {
        args.push(&joined);
    }

    let handle = Handle(self.run(&args)?.trim().to_string());

    self.label_new_pane(&handle, request.name, request.command)?;

    Ok(handle)
}
```

`pane_split` is the same with `split-window -d -t <target> -h|-v -c
<cwd> -P -F #{pane_id} [cmd]` (`Right` → `-h`, `Down` → `-v`).

```rust
/// Workspace for `spawn` without `--workspace`: the caller's session,
/// only when running inside tmux on the ambient server (§7.2).
fn default_workspace(&self) -> Result<String> {
    if self.session.is_some() || env::var_os("TMUX").is_none() {
        return Err(Error::Usage(
            "--workspace is required outside the current tmux server"
                .into(),
        ));
    }

    Ok(self.run(&["display-message", "-p", "#{session_id}"])?
        .trim()
        .to_string())
}

/// Sets `@md-label` and, for a known agent, `@md-agent` on a new
/// pane. A failure here is `StartFailed`: the pane already exists.
fn label_new_pane(
    &self,
    handle: &Handle,
    name: &str,
    command: &[String],
) -> Result<()> {
    let start_failed = |e: Error| Error::StartFailed {
        handle: handle.clone(),
        message: e.to_string(),
    };

    self.set_pane_option(&handle.0, "@md-label", name)
        .map_err(start_failed)?;

    if let Some(kind) = command.first().filter(|c| is_known_kind(c)) {
        self.set_pane_option(&handle.0, "@md-agent", kind)
            .map_err(start_failed)?;
    }

    Ok(())
}

/// Sets a pane-scoped user option.
fn set_pane_option(&self, pane: &str, key: &str, value: &str) -> Result<()> {
    self.run(&["set-option", "-p", "-t", pane, key, value])
        .map(|_| ())
}
```

The rest:

- `pane_read`: `capture-pane -p -t <h> -S -<lines>` plus `-e -J` with
  `ansi`, then `trim_trailing_blank_lines`.
- `pane_prompt`: `send-keys -t <h> -l <text>`, then `send-keys -t <h>
  Enter`.
- `pane_rename`: `set_pane_option(h, "@md-label", label)` then
  `select-pane -t <h> -T <label>`.
- `pane_interrupt`: `send-keys -t <h> C-c`.
- `pane_kill`: `kill-pane -t <h>`.
- `pane_status`: `tail = self.pane_read(h, 15, false)?`, then
  `patterns.classify(&tail)` (no patterns → `(Unknown, None)`),
  `tail: Some(tail)`.
- `raw`: `self.run(&args_as_str)`.

Remove the `#[expect(dead_code)]` from `Tmux`.

Run: `cargo test --locked --test tmux` → PASS (9 tests).

- [ ] **Step 6: Run all checks, commit**

```bash
git add src/tmux.rs tests/tmux.rs
git commit -m "implement tmux pane operations"
```

---

### Task 4: tmux listing and workspaces

**Files:**
- Modify: `src/tmux.rs`, `tests/tmux.rs`

**Interfaces:**
- Consumes: Task 3's `Tmux::run`, `classify_error`, `set_pane_option`,
  test `Server` harness.
- Produces: `workspace_list`, `workspace_create`, `workspace_close`,
  `workspace_tag`, `pane_list` on `Tmux`.

Spec: §7.2 (workspace and list rows), §7.4.

- [ ] **Step 1: Unit tests for the pure parsers**

```rust
mod parse_panes {
    use super::*;

    #[test]
    fn should_fill_record_when_line_complete() {
        let line = "%1\t$0\tws\t@1\ttabby\timpl-1\t/tmp\tclaude\tnode";

        let records = parse_panes(line);

        assert_eq!(
            serde_json::to_value(&records[0]).unwrap(),
            serde_json::json!({
                "handle":"%1","workspace_id":"$0","workspace_label":"ws",
                "tab_id":"@1","tab_label":"tabby","label":"impl-1",
                "cwd":"/tmp","agent":"claude","agent_session":null,
                "session_confidence":"none","status":"unknown",
                "status_confidence":"none"
            })
        );
    }

    #[test]
    fn should_detect_agent_from_command_when_option_empty() {
        let records = parse_panes("%0\t$0\tws\t@0\tw\t\t/tmp\t\tcodex");

        assert_eq!(records[0].agent.as_deref(), Some("codex"));
    }

    #[test]
    fn should_leave_agent_null_when_command_unknown() {
        let records = parse_panes("%0\t$0\tws\t@0\tw\t\t/tmp\t\tbash");

        assert_eq!(records[0].agent, None);
    }
}

mod parse_tags {
    use super::*;

    #[test]
    fn should_keep_only_md_tag_options() {
        let tags = parse_tags("@md-tag-project x\n@other y\nstatus on\n@md-tag-k \"a b\"\n");

        assert_eq!(
            tags.into_iter().collect::<Vec<_>>(),
            vec![("k".to_string(), "a b".to_string()), ("project".into(), "x".into())]
        );
    }
}
```

`show-options` quotes values containing spaces with double quotes.
`parse_tags` strips one pair of surrounding `"` when present. For an
exact round trip regardless of quoting, you may instead read each
value with `show-options -v -t <ws> @md-tag-<k>`. Either is fine; the
test above must pass.

Run: `cargo test --locked tmux::` → FAIL.

- [ ] **Step 2: Implement parsers and list/workspace methods**

Pane format (tab-separated, in this order):

```rust
/// `list-panes` format: one tab-separated line per pane.
const PANE_FORMAT: &str = "#{pane_id}\t#{session_id}\t#{session_name}\t\
#{window_id}\t#{window_name}\t#{@md-label}\t#{pane_current_path}\t\
#{@md-agent}\t#{pane_current_command}";
```

- `parse_panes(&str) -> Vec<PaneRecord>`: split lines, skip blank,
  split on `\t` (skip a line with fewer than 9 fields, and log
  `tracing::warn!`). Empty string field → `None`. `agent` is
  `@md-agent` if non-empty, else `pane_current_command` when
  `is_known_kind`, else `None`. Fixed: `agent_session: None`,
  `session_confidence: None`, `status: Unknown`, `status_confidence:
  None`.
- `pane_list(ws)`: `list-panes -a -F PANE_FORMAT`, or with a filter
  `list-panes -s -t <ws> -F PANE_FORMAT`. A `HarnessUnavailable` error
  → `Ok(vec![])`.
- `workspace_list`: `list-sessions -F
  "#{session_id}\t#{session_name}\t#{session_attached}"`; `focused` is
  attached count `> 0`; tags from `show-options -t <id>` through
  `parse_tags`. `HarnessUnavailable` → `Ok(vec![])`.
- `workspace_create(label, cwd)`: `new-session -d -s <label> -c <cwd>
  -P -F "#{session_id}"`, then return its record (label = `label`,
  `focused: false`, empty tags). Note: with no server running,
  `new-session` starts one. That is correct.
- `workspace_close(id)`: `kill-session -t <id>`.
- `workspace_tag(id, set, clear)`: `set-option -t <id> @md-tag-<k>
  <v>` per pair, `set-option -u -t <id> @md-tag-<k>` per clear, then
  return `parse_tags(show-options -t <id>)`.

- [ ] **Step 3: Integration tests**

Add to `tests/tmux.rs`:

```rust
#[test]
fn should_round_trip_tags_when_set_and_cleared() {
    let server = server!();
    let driver = server.driver();
    let ws = server.tmux(&["display-message", "-p", "-t", "ws", "#{session_id}"]);

    driver
        .workspace_tag(&ws, &[("project".into(), "x".into()), ("tmp".into(), "y".into())], &[])
        .unwrap();
    let tags = driver.workspace_tag(&ws, &[], &["tmp".into()]).unwrap();

    assert_eq!(tags.into_iter().collect::<Vec<_>>(), vec![("project".into(), "x".into())]);
}

#[test]
fn should_list_every_pane_with_labels_when_listing() {
    let server = server!();
    let driver = server.driver();
    let ws = server.tmux(&["display-message", "-p", "-t", "ws", "#{session_id}"]);
    let command = cmd(&["sleep", "30"]);
    driver
        .pane_spawn(&SpawnRequest {
            name: "lbl",
            workspace: Some(&ws),
            cwd: Path::new("/tmp"),
            command: &command,
        })
        .unwrap();

    let labels: Vec<Option<String>> =
        driver.pane_list(None).unwrap().into_iter().map(|p| p.label).collect();

    assert_eq!(labels, vec![None, Some("lbl".into())]);
}

#[test]
fn should_create_list_and_close_workspace() {
    let server = server!();
    let driver = server.driver();

    let created = driver.workspace_create("second", Path::new("/tmp")).unwrap();
    driver.workspace_close(&created.workspace_id).unwrap();
    let labels: Vec<String> =
        driver.workspace_list().unwrap().into_iter().map(|w| w.label).collect();

    assert_eq!(labels, vec!["ws".to_string()]);
}

#[test]
fn should_list_nothing_when_server_absent() {
    let driver = Tmux::new(Some(format!("md-test-absent-{}", std::process::id())), None);

    assert!(driver.pane_list(None).unwrap().is_empty());
}

#[test]
fn should_return_idle_when_waited_pattern_appears() {
    let server = server!();
    let pane = Handle(server.tmux(&["display-message", "-p", "-t", "ws", "#{pane_id}"]));
    let patterns = multiplexer_driver::patterns::Patterns::from_json(
        r#"{"idle":["md-2-waited"]}"#,
    )
    .unwrap();
    let driver = Tmux::new(Some(server.name.clone()), Some(patterns));
    // The pattern matches only the output, not the typed command line.
    driver.pane_prompt(&pane, "sleep 1; echo md-$((1+1))-waited").unwrap();

    let result = multiplexer_driver::driver::wait_for(
        &driver,
        &pane,
        &[multiplexer_driver::model::Status::Idle],
        Duration::from_secs(10),
        Duration::from_millis(200),
    )
    .unwrap();

    assert_eq!(result.status, multiplexer_driver::model::Status::Idle);
}
```

Run: `cargo test --locked --test tmux` → PASS.

- [ ] **Step 4: Run all checks, commit**

```bash
git add src/tmux.rs tests/tmux.rs
git commit -m "add tmux workspaces, tags, and pane listing"
```

---

### Task 5: Herdr pane operations and test harness

**Files:**
- Modify: `src/herdr.rs`
- Create: `tests/herdr.rs`, `tests/fixtures/herdr/pane_get.json`,
  `tests/fixtures/herdr/tab_create.json`,
  `tests/fixtures/herdr/pane_split.json`,
  `tests/fixtures/herdr/error_pane_not_found.json`

**Interfaces:**
- Consumes: Tasks 1–2 (`Driver`, `Error`, requests, models, `text`,
  `agents::is_known_kind`, `Herdr::new`).
- Produces: `impl Driver for Herdr`: `pane_spawn`, `pane_split`,
  `pane_read`, `pane_prompt`, `pane_rename`, `pane_interrupt`,
  `pane_kill`, `pane_status`, `raw`. Private helpers Task 6 reuses:
  `fn run(&self, args: &[&str]) -> Result<String>`,
  `fn run_json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T>`,
  `fn classify_error(stderr: &str) -> Error`,
  `fn socket_path(session: &str) -> PathBuf`,
  `fn map_status(&str) -> Status`.
  Test harness in `tests/herdr.rs`: `struct Server` with
  `fn start() -> Option<Server>`, `fn driver(&self) -> Herdr`,
  `fn herdr(&self, args: &[&str]) -> String`.

Spec: §4.4, §8.

- [ ] **Step 1: Save fixtures (captured live on 2026-09-30)**

`tests/fixtures/herdr/pane_get.json`:

```json
{"id":"cli:pane:get","result":{"pane":{"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"93a98fb6-766d-46e6-bdde-668a7ae81b87"},"agent_status":"working","cwd":"/home/u/proj","focused":false,"foreground_cwd":"/home/u/proj","pane_id":"w1R:p2","revision":9,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":63},"tab_id":"w1R:t2","terminal_id":"term_65cb61f4ef48a7f","terminal_title":"x","terminal_title_stripped":"x","workspace_id":"w1R"},"type":"pane_info"}}
```

`tests/fixtures/herdr/tab_create.json`:

```json
{"id":"cli:tab:create","result":{"root_pane":{"agent_status":"unknown","cwd":"/tmp","focused":false,"foreground_cwd":"/tmp","pane_id":"w1:p2","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":39},"tab_id":"w1:t2","terminal_id":"term_65cb6e155c85b2","workspace_id":"w1"},"tab":{"agent_status":"unknown","focused":false,"label":"t2","number":2,"pane_count":1,"tab_id":"w1:t2","workspace_id":"w1"},"type":"tab_created"}}
```

`tests/fixtures/herdr/pane_split.json`:

```json
{"id":"cli:pane:split","result":{"pane":{"agent_status":"unknown","cwd":"/tmp","focused":false,"foreground_cwd":"/tmp","pane_id":"w1:p3","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":39},"tab_id":"w1:t1","terminal_id":"term_65cb6e155d4ea3","workspace_id":"w1"},"type":"pane_info"}}
```

`tests/fixtures/herdr/error_pane_not_found.json`:

```json
{"error":{"code":"pane_not_found","message":"pane w1:p99 not found"},"id":"cli:pane:get"}
```

- [ ] **Step 2: Unit tests for pure helpers**

```rust
#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod classify_error {
        use super::*;

        #[rstest]
        #[case::pane(include_str!("../tests/fixtures/herdr/error_pane_not_found.json"), "not_found")]
        #[case::workspace(r#"{"error":{"code":"workspace_not_found","message":"m"}}"#, "not_found")]
        #[case::agent(r#"{"error":{"code":"agent_not_found","message":"m"}}"#, "not_found")]
        #[case::other_code(r#"{"error":{"code":"weird","message":"m"}}"#, "unexpected")]
        #[case::not_json("boom", "unexpected")]
        fn should_classify_by_code(#[case] stderr: &str, #[case] kind: &str) {
            assert_eq!(classify_error(stderr).kind(), kind);
        }

        #[test]
        fn should_include_code_and_message_when_envelope() {
            let err = classify_error(include_str!(
                "../tests/fixtures/herdr/error_pane_not_found.json"
            ));

            assert_eq!(err.to_string(), "pane_not_found: pane w1:p99 not found");
        }
    }

    mod map_status {
        use super::*;

        #[rstest]
        #[case("working", Status::Working)]
        #[case("idle", Status::Idle)]
        #[case("done", Status::Idle)]
        #[case("blocked", Status::Blocked)]
        #[case("unknown", Status::Unknown)]
        #[case("new-state", Status::Unknown)]
        fn should_map_herdr_status(#[case] raw: &str, #[case] expected: Status) {
            assert_eq!(map_status(raw), expected);
        }
    }

    mod socket_path {
        use super::*;

        #[test]
        fn should_use_sessions_dir_when_named() {
            let path = socket_path_in(Path::new("/c"), "personal");

            assert_eq!(path, PathBuf::from("/c/herdr/sessions/personal/herdr.sock"));
        }

        #[test]
        fn should_use_root_socket_when_default() {
            assert_eq!(
                socket_path_in(Path::new("/c"), "default"),
                PathBuf::from("/c/herdr/herdr.sock")
            );
        }
    }

    mod parse {
        use super::*;

        #[test]
        fn should_read_root_pane_when_tab_created() {
            let created: TabCreated = serde_json::from_str(include_str!(
                "../tests/fixtures/herdr/tab_create.json"
            ))
            .unwrap();

            assert_eq!(created.result.root_pane.pane_id, "w1:p2");
        }

        #[test]
        fn should_read_status_when_pane_get() {
            let got: PaneGet = serde_json::from_str(include_str!(
                "../tests/fixtures/herdr/pane_get.json"
            ))
            .unwrap();

            assert_eq!(got.result.pane.agent_status, "working");
        }
    }
}
```

`socket_path(session)` resolves the config dir (`$XDG_CONFIG_HOME`, else
`$HOME/.config`) and calls the pure `socket_path_in(config_dir,
session)`, which the tests cover.

Run: `cargo test --locked herdr::` → FAIL.

- [ ] **Step 3: Implement helpers and response structs**

```rust
/// One pane object as Herdr prints it (only the fields we read).
#[derive(Debug, Deserialize)]
struct PaneJson {
    /// Pane ID.
    pane_id: String,
    /// Agent status string.
    #[serde(default)]
    agent_status: String,
}

/// `{"result":{"pane":…}}` from `pane get` and `pane split`.
#[derive(Debug, Deserialize)]
struct PaneGet {
    /// Result body.
    result: PaneGetResult,
}

/// Body of [`PaneGet`].
#[derive(Debug, Deserialize)]
struct PaneGetResult {
    /// The pane.
    pane: PaneJson,
}

/// `{"result":{"root_pane":…}}` from `tab create`.
#[derive(Debug, Deserialize)]
struct TabCreated {
    /// Result body.
    result: TabCreatedResult,
}

/// Body of [`TabCreated`].
#[derive(Debug, Deserialize)]
struct TabCreatedResult {
    /// The new tab's first pane.
    root_pane: PaneJson,
}

/// Herdr's failure envelope on stderr.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    /// The error.
    error: ErrorBody,
}

/// Body of [`ErrorEnvelope`].
#[derive(Debug, Deserialize)]
struct ErrorBody {
    /// Machine code, e.g. `pane_not_found`.
    code: String,
    /// Human message.
    message: String,
}
```

`run(&self, args)`:

1. Build `Command::new("herdr").args(args)`.
2. If `self.session` is `Some(name)`: compute `socket_path(name)`; if
   the file does not exist, return
   `HarnessUnavailable(format!("herdr session {name:?}: no socket at {}"))`;
   else `.env("HERDR_SOCKET_PATH", path)`.
3. Spawn failure → `HarnessUnavailable("running herdr: …")`.
4. Non-zero exit → `Err(classify_error(stderr.trim()))`.
5. Success → stdout as `String`.

`classify_error(stderr)`: parse `ErrorEnvelope`. If it parses, message
is `"{code}: {message}"`, and `code` ending in `_not_found` →
`NotFound`, else `Unexpected`. If it does not parse → `Unexpected(raw
stderr)`.

`run_json`: `run`, then `serde_json::from_str`, parse failure →
`Unexpected(format!("parsing herdr output: {e}"))`.

Run: `cargo test --locked herdr::` → PASS.

- [ ] **Step 4: Write the integration harness**

`tests/herdr.rs`:

```rust
//! Herdr integration tests. The binary starts its own headless Herdr
//! server in a throwaway session (`md-test-<pid>`), with every HERDR_*
//! variable removed, and deletes it at the end. No other session is
//! touched. Only shells run in panes, never a paid agent.

use std::{
    env,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use multiplexer_driver::herdr::Herdr;

/// The shared test server. Tests take the lock to run one at a time.
static SERVER: OnceLock<Option<Mutex<Server>>> = OnceLock::new();

/// A headless Herdr server for this test binary.
struct Server {
    /// Session name.
    name: String,
    /// Server process.
    child: Child,
}
```

Rules for the harness (write it to satisfy them):

- Session name `md-test-<pid>`. Assert in code that it starts with
  `md-test-` before any destructive call.
- Start: `Command::new("herdr").args(["--session", &name, "server"])`,
  with `env_remove` for `HERDR_SOCKET_PATH`, `HERDR_ENV`,
  `HERDR_SESSION`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`,
  `HERDR_PANE_ID`, stdout/stderr to `Stdio::null()`. Spawn error
  `NotFound` → skip all tests (`None`).
- Wait up to 5 s for the socket file (`$XDG_CONFIG_HOME` or
  `$HOME/.config` + `/herdr/sessions/<name>/herdr.sock`).
- `fn herdr(&self, args)`: runs `herdr` with `HERDR_SOCKET_PATH` set
  to that socket and the other `HERDR_*` removed, returns stdout.
- Create one workspace for tests in `start()`: `workspace create
  --label md-test --cwd /tmp --no-focus`; keep its ID and root pane ID
  (parse `result.workspace.workspace_id` and
  `result.root_pane.pane_id` with `serde_json::Value`).
- Cleanup: Rust does not drop statics. Register cleanup so it runs
  after the last test: the simplest correct way is a single `#[test]`
  per file that runs every scenario in sequence (call private fns),
  with the `Server` as a local value whose `Drop` runs `herdr server
  stop` (socket env set), waits for the child up to 5 s (then kills
  it), and runs `herdr session delete <name>`. Use that design; drop
  the `OnceLock` above if you do. Each scenario fn names its behavior
  (`should_…`) and the test prints which scenario failed.

- [ ] **Step 5: Scenarios (write failing, then implement)**

In the single test, run these scenarios in order:

1. `should_spawn_raw_command_and_read_output`: `pane_spawn` with
   `workspace: Some(ws)`, `command: ["bash","-c","echo hello-md; exec
   bash"]`, name `raw-1` → handle; sleep 500 ms; `pane_read(h, 10,
   false)` contains `hello-md`.
2. `should_split_and_kill_only_new_pane`: `pane_split` from the root
   pane, `Direction::Right`, `["bash"]` → new handle; `pane_kill(new)`;
   `herdr pane get <root>` still succeeds.
3. `should_keep_handle_when_renamed`: `pane_rename(root, "renamed")`;
   `herdr pane get <root>` JSON has `result.pane.label == "renamed"`.
4. `should_report_native_unknown_when_no_agent`: `pane_status(root)` →
   `status == Unknown`, `confidence == Native`, `tail == None`.
5. `should_report_not_found_when_prompting_non_agent`:
   `pane_prompt(root, "hi")` → `Err`, `kind() == "not_found"`.
6. `should_report_not_found_when_pane_missing`: `pane_read(&Handle("w1:p99"),…)`
   → `not_found`. (Use the real workspace prefix: `<ws>:p99`.)
7. `should_fail_unavailable_when_session_missing`:
   `Herdr::new(Some("md-test-absent".into())).pane_read(…)` →
   `harness_unavailable`.
8. `should_pass_args_through_when_raw`: `raw(["tab","list","--workspace",ws])`
   output contains `"tab_list"`.

- [ ] **Step 6: Implement the operations (§8.1, §8.2)**

- `pane_spawn`: workspace = `request.workspace` or
  `env::var("HERDR_WORKSPACE_ID")`; neither → `Usage("--workspace is
  required outside a Herdr pane")`. Empty command → `Usage`. Run
  `tab create --workspace <ws> --cwd <cwd> --label <name> --no-focus`
  as `TabCreated`, then `self.start_in_pane(&pane_id, name, command)`.
- `pane_split`: empty command → `Usage`. `pane split --pane <target>
  --direction right|down --cwd <cwd> --no-focus` as `PaneGet`, then
  `start_in_pane`.
- `start_in_pane(pane, name, command) -> Result<Handle>`: every error
  inside becomes `StartFailed { handle: pane, message }`. Known kind
  (`is_known_kind(&command[0])`): `agent start <name> --kind <kind>
  --pane <pane>` plus `--` and `command[1..]` when non-empty.
  Otherwise `pane run <pane> <shell_join(command)>` then `pane rename
  <pane> <name>`.
- `pane_read`: `pane read <h> --source visible --lines <n>` plus
  `--format ansi` if `ansi`; trim trailing blank lines. Put this
  comment on the `--source visible` line: `// Required: on herdr
  0.8.2, --lines with the default source silently returns nothing.`
- `pane_prompt`: `agent prompt <h> <text>`.
- `pane_rename`: `pane rename <h> <label>`.
- `pane_interrupt`: `agent send-keys <h> ctrl+c`.
- `pane_kill`: `pane close <h>`.
- `pane_status`: `pane get <h>` as `PaneGet`; `map_status`;
  `Confidence::Native`; `tail: None`.
- `raw`: `self.run(args)`.

Remove the `#[expect(dead_code)]` on `Herdr`.

Run: `cargo test --locked --test herdr` → PASS. Then confirm cleanup:
`herdr session list` shows no `md-test-*` session.

- [ ] **Step 7: Run all checks, commit**

```bash
git add src/herdr.rs tests/herdr.rs tests/fixtures
git commit -m "implement herdr pane operations with isolated test server"
```

---

### Task 6: Herdr listing and workspaces

**Files:**
- Modify: `src/herdr.rs`, `tests/herdr.rs`
- Create: `tests/fixtures/herdr/workspace_list.json`,
  `tests/fixtures/herdr/workspace_create.json`,
  `tests/fixtures/herdr/pane_list.json`,
  `tests/fixtures/herdr/tab_list.json`

**Interfaces:**
- Consumes: Task 5's `run`, `run_json`, `classify_error`,
  `map_status`, the test harness.
- Produces: `workspace_list`, `workspace_create`, `workspace_close`,
  `workspace_tag`, `pane_list` on `Herdr`.

Spec: §8.1 (workspace rows, pane list).

- [ ] **Step 1: Save fixtures (captured live on 2026-09-30)**

`workspace_list.json`:

```json
{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[{"active_tab_id":"w1:t1","agent_status":"unknown","focused":true,"label":"probe","number":1,"pane_count":3,"tab_count":2,"tokens":{"project":"x"},"workspace_id":"w1"},{"active_tab_id":"w3:t1","agent_status":"done","focused":false,"label":"other","number":2,"pane_count":1,"tab_count":1,"workspace_id":"w3"}]}}
```

`workspace_create.json`:

```json
{"id":"cli:workspace:create","result":{"root_pane":{"agent_status":"unknown","cwd":"/tmp","focused":true,"foreground_cwd":"/tmp","pane_id":"w1:p1","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":40},"tab_id":"w1:t1","terminal_id":"term_65cb6e0d6b6cf1","workspace_id":"w1"},"tab":{"agent_status":"unknown","focused":true,"label":"1","number":1,"pane_count":1,"tab_id":"w1:t1","workspace_id":"w1"},"type":"workspace_created","workspace":{"active_tab_id":"w1:t1","agent_status":"unknown","focused":true,"label":"probe","number":1,"pane_count":1,"tab_count":1,"workspace_id":"w1"}}}
```

`pane_list.json`:

```json
{"id":"cli:pane:list","result":{"panes":[{"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"a6b4756a-1f0f-42b1-85da-9393980017be"},"agent_status":"done","cwd":"/home/u/proj","focused":true,"foreground_cwd":"/home/u/proj","label":"impl-1","pane_id":"w1:p1","revision":2,"tab_id":"w1:t1","terminal_id":"t1","workspace_id":"w1"},{"agent":"pi","agent_session":{"agent":"pi","kind":"path","source":"herdr:pi","value":"/home/u/.pi/agent/sessions/x.jsonl"},"agent_status":"working","cwd":"/home/u/proj","focused":false,"foreground_cwd":"/home/u/proj","pane_id":"w1:p2","revision":1,"tab_id":"w1:t2","terminal_id":"t2","workspace_id":"w1"},{"agent_status":"unknown","cwd":"/tmp","focused":false,"foreground_cwd":"/tmp","pane_id":"w3:p1","revision":1,"tab_id":"w3:t1","terminal_id":"t3","workspace_id":"w3"}],"type":"pane_list"}}
```

(Trimmed from a live capture: `scroll` and `terminal_title` fields are
dropped; the parser must ignore unknown fields anyway.)

`tab_list.json` (for workspace `w1`):

```json
{"id":"cli:tab:list","result":{"tabs":[{"agent_status":"done","focused":true,"label":"impl","number":1,"pane_count":1,"tab_id":"w1:t1","workspace_id":"w1"},{"agent_status":"working","focused":false,"label":"review","number":2,"pane_count":1,"tab_id":"w1:t2","workspace_id":"w1"}],"type":"tab_list"}}
```

- [ ] **Step 2: Unit tests for the join**

Put the join in a pure fn so it is testable without Herdr:

```rust
/// Builds pane records from `pane list` output plus workspace and tab
/// labels (§8.1).
fn build_pane_records(
    panes: Vec<PaneListItem>,
    workspace_labels: &HashMap<String, String>,
    tab_labels: &HashMap<String, String>,
) -> Vec<PaneRecord>
```

Tests:

```rust
mod build_pane_records {
    use super::*;

    /// Parses the fixtures and joins them.
    fn records() -> Vec<PaneRecord> {
        let panes: PaneList = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr/pane_list.json"
        ))
        .unwrap();
        let workspaces: WorkspaceList = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr/workspace_list.json"
        ))
        .unwrap();
        let tabs: TabList = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr/tab_list.json"
        ))
        .unwrap();

        build_pane_records(
            panes.result.panes,
            &workspace_labels(&workspaces.result.workspaces),
            &tab_labels(&tabs.result.tabs),
        )
    }

    #[test]
    fn should_join_labels_and_session_when_agent_present() {
        assert_eq!(
            serde_json::to_value(&records()[0]).unwrap(),
            serde_json::json!({
                "handle":"w1:p1","workspace_id":"w1","workspace_label":"probe",
                "tab_id":"w1:t1","tab_label":"impl","label":"impl-1",
                "cwd":"/home/u/proj","agent":"claude",
                "agent_session":{"kind":"id","value":"a6b4756a-1f0f-42b1-85da-9393980017be"},
                "session_confidence":"native","status":"idle",
                "status_confidence":"native"
            })
        );
    }

    #[test]
    fn should_report_path_session_when_pi() {
        assert_eq!(
            records()[1].agent_session.as_ref().map(|s| s.kind),
            Some(SessionRefKind::Path)
        );
    }

    #[test]
    fn should_leave_agent_fields_null_when_plain_shell() {
        let shell = &records()[2];

        assert_eq!(
            (shell.agent.as_deref(), shell.session_confidence, shell.tab_label.as_deref()),
            (None, Confidence::None, None)
        );
    }
}

mod workspace_records {
    use super::*;

    #[test]
    fn should_read_tokens_as_tags_when_present() {
        let list: WorkspaceList = serde_json::from_str(include_str!(
            "../tests/fixtures/herdr/workspace_list.json"
        ))
        .unwrap();

        let records: Vec<WorkspaceRecord> =
            list.result.workspaces.into_iter().map(WorkspaceRecord::from).collect();

        assert_eq!(
            (records[0].tags.get("project").map(String::as_str), records[1].tags.len()),
            (Some("x"), 0)
        );
    }
}
```

Note the third fixture pane is in workspace `w3`, whose tabs are not in
`tab_list.json`, so its `tab_label` is `None`. In the real driver,
`pane_list` calls `tab list --workspace <ws>` for every distinct
workspace ID among the panes.

Define the serde structs `PaneList`, `PaneListItem` (fields:
`pane_id`, `workspace_id`, `tab_id`, `cwd: Option<String>`, `label:
Option<String>`, `agent: Option<String>`, `agent_session:
Option<HerdrSession>`, `agent_status: String`), `HerdrSession` (`kind:
SessionRefKind`, `value: String`), `WorkspaceList`, `WorkspaceItem`
(`workspace_id`, `label`, `focused`, `#[serde(default)] tokens:
BTreeMap<String,String>`), `TabList`, `TabItem` (`tab_id`, `label`),
`WorkspaceCreated` (`result.workspace: WorkspaceItem`),
`WorkspaceGet` (`result.workspace: WorkspaceItem`), plus `impl
From<WorkspaceItem> for WorkspaceRecord`, and helpers
`workspace_labels(&[WorkspaceItem]) -> HashMap<String,String>` and
`tab_labels(&[TabItem]) -> HashMap<String,String>`. Serde ignores
unknown fields by default; keep it that way.

Run: `cargo test --locked herdr::` → FAIL, implement, → PASS.

- [ ] **Step 3: Implement the driver methods**

- `workspace_list`: `workspace list` → map `WorkspaceRecord::from`.
- `workspace_create(label, cwd)`: `workspace create --label <label>
  --cwd <cwd> --no-focus` → `WorkspaceCreated` →
  `WorkspaceRecord::from(result.workspace)`.
- `workspace_close(id)`: `workspace close <id>`.
- `workspace_tag(id, set, clear)`: one `workspace report-metadata <id>
  --source multiplexer-driver` call with `--token k=v` per pair and
  `--clear-token k` per clear; then `workspace get <id>` →
  `result.workspace.tokens`.
- `pane_list(ws)`: `pane list` (plus `--workspace <ws>` when given),
  `workspace list` for labels, `tab list --workspace <id>` for each
  distinct workspace among the panes; then `build_pane_records`.

- [ ] **Step 4: Integration scenarios**

Add to the single Herdr test's scenario sequence:

1. `should_list_created_workspace_with_tags`: `workspace_create("md-ws2",
   "/tmp")`; `workspace_tag(id, [("project","x")], [])` returns
   `{"project":"x"}`; `workspace_list()` has a record with that ID,
   label `md-ws2`, and tag `project=x`.
2. `should_clear_tag`: `workspace_tag(id, [], ["project"])` returns an
   empty map.
3. `should_list_every_pane_with_labels`: `pane_list(Some(test_ws))`
   includes the root pane with `label == Some("renamed")` (from Task 5
   scenario 3) and `workspace_label == Some("md-test")`.
4. `should_close_workspace`: `workspace_close(id)`; `workspace_list()`
   no longer has it.

Run: `cargo test --locked --test herdr` → PASS. Confirm no `md-test-*`
session remains (`herdr session list`).

- [ ] **Step 5: Run all checks, commit**

```bash
git add src/herdr.rs tests/herdr.rs tests/fixtures
git commit -m "add herdr workspaces, tags, and joined pane listing"
```

---

### Task 7: Documentation and end-to-end check

**Files:**
- Create: `README.md`, `AGENTS.md`, `CLAUDE.md`,
  `docs/overseer-migration.md`

**Interfaces:**
- Consumes: the finished CLI.
- Produces: docs only.

Style: `writing-principles-technical` (STE-flavored), `markdown-style`
(wrap at 79 columns).

- [ ] **Step 1: End-to-end smoke test against a private tmux server**

Run and paste the output into the task result:

```bash
cargo build --locked
B=target/debug/multiplexer-driver
tmux -L md-smoke new-session -d -s smoke -c /tmp
export MULTIPLEXER_DRIVER_HARNESS=tmux MULTIPLEXER_DRIVER_SESSION=md-smoke
$B workspace list
WS=$($B workspace list | head -1 | jq -r .workspace_id)
H=$($B pane spawn --name smoke-1 --workspace "$WS" --cwd /tmp -- bash | jq -r .handle)
$B pane prompt "$H" --text 'echo smoke-ok'
sleep 0.5; $B pane read "$H" --lines 5
$B workspace tag "$WS" project=smoke
$B pane list | jq -c '{handle,label,tab_label}'
$B pane read %999; echo "exit=$?"
$B pane kill "$H"
tmux -L md-smoke kill-server
```

Expected: JSON everywhere on stdout, `smoke-ok` in the read, `exit=4`
with a `not_found` record on stderr.

- [ ] **Step 2: Write `README.md`**

Sections: what it is (two sentences), install (`cargo install --path
.`), quickstart (five commands from Step 1), command list (copy from
`--help`), output contract (§5.4), exit-code table (§5.5 verbatim),
harness notes (tmux: IDs and `@md-*` options, status needs
`--patterns`; Herdr: `--session`, native status), one PowerShell
example (`multiplexer-driver … | % { $_ | ConvertFrom-Json }` and
`$LASTEXITCODE`), `raw` escape hatch example, development checks,
license MIT.

- [ ] **Step 3: Write `AGENTS.md` and `CLAUDE.md`**

`CLAUDE.md` contains exactly `@AGENTS.md`. `AGENTS.md`: conventions
(the Global Constraints above, short form), then "Design decisions
worth knowing before changing things", one bullet each, with the
reason:

- `pane prompt` does not wait. `pane wait` is separate, requires a
  timeout, and refuses to run without a status signal.
- Status degrades honestly: tmux without patterns is `unknown`/`none`.
- Handles are stable harness IDs and survive rename.
- `pane kill` closes one pane, never its tab or workspace.
- tmux labels live in `@md-label`, not the pane title (agents rewrite
  titles).
- Herdr `pane read` needs `--source visible` (the 0.8.2 bug, found by
  an 8/8 failure rate, confirmed live; re-test before removing).
- Herdr `agent start` only knows fixed kinds. Others fall back to
  `pane run` and lose native prompt and status until Herdr detects an
  agent from the screen.
- tmux server-exit race and its one retry.
- `notify` trusts Herdr's `shown`, not its exit code.
- Herdr tags are `tokens`, readable, merged across sources.
- Wait is one poll loop for all harnesses, not Herdr's `agent wait`.
- Integration tests use private servers only (tmux `-L`, Herdr
  `md-test-<pid>` session).
- Open item: tmux conversation IDs (§12 of the spec).

- [ ] **Step 4: Write `docs/overseer-migration.md`**

A table mapping each old `overseer-driver` command (as used in the
overseer skill's `resources/tmux.md`, `herdr.md`, `judge.md`) to the
new one:

| Old | New |
|---|---|
| `overseer-driver spawn --harness H --name N -- cmd` | `multiplexer-driver --harness H pane spawn --name N -- cmd` |
| `overseer-driver split --harness H --target T --name N --direction D -- cmd` | `multiplexer-driver --harness H pane split T --name N --direction D -- cmd` |
| `overseer-driver read --harness H --target T --lines N [--ansi]` | `multiplexer-driver --harness H pane read T --lines N [--ansi]` |
| `overseer-driver prompt --harness H --target T --text X` | `multiplexer-driver --harness H pane prompt T --text X` |
| `overseer-driver list --harness H` → `{"handles":[…]}` | `multiplexer-driver --harness H pane list` → JSONL `PaneRecord` (`jq -r .handle`) |
| `overseer-driver rename --harness H --target T --label L` | `multiplexer-driver --harness H pane rename T --label L` |
| `overseer-driver interrupt --harness H --target T` | `multiplexer-driver --harness H pane interrupt T` |
| `overseer-driver interrupt --harness H --target T --kill` | `multiplexer-driver --harness H pane kill T` |
| `overseer-driver status --harness H --target T [--patterns F]` | `multiplexer-driver --harness H pane status T [--patterns F]` |
| `overseer-driver notify --message M [--title T] [--sound S]` | `multiplexer-driver notify --message M [--title T] [--sound S]` |

Then list the behavior changes a skill author must know:

- tmux handles are now `%N` pane IDs, not `session:window.pane`.
- tmux `spawn` opens a window in a session, not a new session. Outside
  tmux it needs `--workspace`.
- `list` covers every pane, not only agents (filter with
  `jq 'select(.agent)'`).
- Failures now end stderr with a JSON record, and exit codes are typed.
- New: `workspace …`, `pane wait`, `agent resume-args`, `--session`.

State at the top: this project does not edit the overseer repo. The
table is input for that separate change.

- [ ] **Step 5: Run all checks, commit**

```bash
git add README.md AGENTS.md CLAUDE.md docs/overseer-migration.md
git commit -m "add README, agent docs, and overseer migration map"
```
