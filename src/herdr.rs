//! Herdr driver (§8). Handles are Herdr pane IDs (`w1:p2`).

use std::{
    env,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    agents::is_known_kind,
    driver::{Driver, Error, Result, SpawnRequest, SplitRequest},
    model::{Confidence, Direction, Handle, Status, StatusResult},
    process::{CEILING, output_within},
    text::{shell_join, trim_trailing_blank_lines},
};

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

    /// Runs `herdr args…` under the ceiling and returns the raw
    /// output. With a session, every `HERDR_*` variable is removed
    /// from the child and `HERDR_SOCKET_PATH` is set to the session's
    /// socket, so the caller's own session never leaks in (§4.4).
    fn output<S: AsRef<str>>(&self, args: &[S]) -> Result<Output> {
        let mut command = Command::new("herdr");

        command.args(args.iter().map(AsRef::as_ref));

        if let Some(name) = &self.session {
            let socket = socket_path(name)?;

            if !socket.exists() {
                return Err(Error::HarnessUnavailable(format!(
                    "herdr session {name:?}: no socket at {}",
                    socket.display()
                )));
            }

            for (key, _) in env::vars_os() {
                if key.to_string_lossy().starts_with("HERDR_") {
                    command.env_remove(key);
                }
            }

            command.env("HERDR_SOCKET_PATH", socket);
        }

        output_within(&mut command, CEILING).map_err(|e| spawn_error(&e))
    }

    /// Runs `herdr args…` and returns stdout; a failing exit is
    /// classified from its stderr envelope.
    fn run<S: AsRef<str>>(&self, args: &[S]) -> Result<String> {
        let output = self.output(args)?;

        if !output.status.success() {
            return Err(classify_error(
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs `herdr args…` and parses its JSON stdout as `T`.
    fn run_json<T: DeserializeOwned, S: AsRef<str>>(
        &self,
        args: &[S],
    ) -> Result<T> {
        let stdout = self.run(args)?;

        serde_json::from_str(&stdout).map_err(|e| {
            Error::Unexpected(format!("parsing herdr output: {e}"))
        })
    }

    /// Starts `command` in the existing `pane` (§8.2). Every failure
    /// here is [`Error::StartFailed`] carrying the pane.
    fn start_in_pane(
        &self,
        pane: &str,
        name: &str,
        command: &[String],
    ) -> Result<Handle> {
        let handle = Handle(pane.to_string());

        self.start_command(pane, name, command).map_err(|e| {
            Error::StartFailed {
                handle: handle.clone(),
                message: e.to_string(),
            }
        })?;

        Ok(handle)
    }

    /// The fallible body of [`Herdr::start_in_pane`].
    fn start_command(
        &self,
        pane: &str,
        name: &str,
        command: &[String],
    ) -> Result<()> {
        let Some(first) = command.first() else {
            return Err(Error::Usage("a command is required".into()));
        };

        if is_known_kind(first) {
            self.run(&agent_start_args(name, first, pane, &command[1..]))?;

            return Ok(());
        }

        self.run(&["pane", "run", pane, &shell_join(command)])?;
        self.run(&["pane", "rename", pane, name])?;

        Ok(())
    }
}

impl Driver for Herdr {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn pane_spawn(&self, request: &SpawnRequest<'_>) -> Result<Handle> {
        if request.command.is_empty() {
            return Err(Error::Usage("a command is required on herdr".into()));
        }

        let workspace = match request.workspace {
            Some(ws) => ws.to_string(),
            None => env::var("HERDR_WORKSPACE_ID").map_err(|_| {
                Error::Usage(
                    "--workspace is required outside a Herdr pane".into(),
                )
            })?,
        };
        let created: TabCreated = self.run_json(&tab_create_args(
            &workspace,
            request.cwd,
            request.name,
        ))?;

        self.start_in_pane(
            &created.result.root_pane.pane_id,
            request.name,
            request.command,
        )
    }

    fn pane_split(&self, request: &SplitRequest<'_>) -> Result<Handle> {
        if request.command.is_empty() {
            return Err(Error::Usage("a command is required on herdr".into()));
        }

        let split: PaneGet = self.run_json(&split_args(
            &request.target.0,
            request.direction,
            request.cwd,
        ))?;

        self.start_in_pane(
            &split.result.pane.pane_id,
            request.name,
            request.command,
        )
    }

    fn pane_read(
        &self,
        target: &Handle,
        lines: u32,
        ansi: bool,
    ) -> Result<String> {
        let out = self.run(&read_args(&target.0, lines, ansi))?;

        Ok(trim_trailing_blank_lines(&out))
    }

    fn pane_prompt(&self, target: &Handle, text: &str) -> Result<()> {
        self.run(&["agent", "prompt", &target.0, text])?;

        Ok(())
    }

    fn pane_rename(&self, target: &Handle, label: &str) -> Result<()> {
        self.run(&["pane", "rename", &target.0, label])?;

        Ok(())
    }

    fn pane_interrupt(&self, target: &Handle) -> Result<()> {
        self.run(&["agent", "send-keys", &target.0, "ctrl+c"])?;

        Ok(())
    }

    fn pane_kill(&self, target: &Handle) -> Result<()> {
        self.run(&["pane", "close", &target.0])?;

        Ok(())
    }

    fn pane_status(&self, target: &Handle) -> Result<StatusResult> {
        let got: PaneGet = self.run_json(&["pane", "get", &target.0])?;

        Ok(StatusResult {
            handle: target.clone(),
            status: map_status(&got.result.pane.agent_status),
            confidence: Confidence::Native,
            tail: None,
        })
    }

    fn raw(&self, args: &[String]) -> Result<String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.output(&args)?;

        if !output.status.success() {
            return Err(Error::Unexpected(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Maps a subprocess failure to an [`Error`]: a missing binary, a
/// call past the ceiling, and any other spawn failure are all
/// [`Error::HarnessUnavailable`] (§4.2).
fn spawn_error(e: &io::Error) -> Error {
    match e.kind() {
        ErrorKind::NotFound => {
            Error::HarnessUnavailable("herdr is not installed".into())
        }
        ErrorKind::TimedOut => {
            Error::HarnessUnavailable(format!("herdr timed out: {e}"))
        }
        _ => Error::HarnessUnavailable(format!("running herdr: {e}")),
    }
}

/// Converts string slices to owned argument strings.
fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

/// Arguments for `tab create` in `workspace`.
fn tab_create_args(workspace: &str, cwd: &Path, label: &str) -> Vec<String> {
    let mut args = owned(&["tab", "create", "--workspace", workspace]);

    args.extend(["--cwd".into(), cwd.to_string_lossy().into_owned()]);
    args.extend(owned(&["--label", label, "--no-focus"]));

    args
}

/// Arguments for `pane split` of `target`.
fn split_args(target: &str, direction: Direction, cwd: &Path) -> Vec<String> {
    let direction = match direction {
        Direction::Right => "right",
        Direction::Down => "down",
    };
    let mut args = owned(&["pane", "split", "--pane", target]);

    args.extend(owned(&["--direction", direction, "--cwd"]));
    args.extend([cwd.to_string_lossy().into_owned(), "--no-focus".into()]);

    args
}

/// Arguments for `pane read` of the last `lines` visible lines.
fn read_args(target: &str, lines: u32, ansi: bool) -> Vec<String> {
    let mut args = owned(&["pane", "read", target]);

    // Required: on herdr 0.8.2, --lines with the default source
    // silently returns nothing.
    args.extend(owned(&["--source", "visible", "--lines"]));
    args.push(lines.to_string());

    if ansi {
        args.extend(owned(&["--format", "ansi"]));
    }

    args
}

/// Arguments for `agent start` of a known kind, forwarding `rest`
/// after `--` when it is not empty.
fn agent_start_args(
    name: &str,
    kind: &str,
    pane: &str,
    rest: &[String],
) -> Vec<String> {
    let mut args = owned(&["agent", "start", name, "--kind", kind]);

    args.extend(owned(&["--pane", pane]));

    if !rest.is_empty() {
        args.push("--".into());
        args.extend(rest.iter().cloned());
    }

    args
}

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

/// Turns Herdr's stderr into an [`Error`]: the envelope becomes
/// `"<code>: <message>"`, and exactly `pane_not_found`,
/// `workspace_not_found`, and `agent_not_found` are
/// [`Error::NotFound`]. Anything else is [`Error::Unexpected`].
fn classify_error(stderr: &str) -> Error {
    let stderr = stderr.trim();
    let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(stderr) else {
        return Error::Unexpected(stderr.to_string());
    };
    let ErrorBody { code, message } = envelope.error;
    let text = format!("{code}: {message}");

    match code.as_str() {
        "pane_not_found" | "workspace_not_found" | "agent_not_found" => {
            Error::NotFound(text)
        }
        _ => Error::Unexpected(text),
    }
}

/// Maps Herdr's agent status onto ours. `done` only means the user
/// has not looked yet, so it is `idle`.
fn map_status(raw: &str) -> Status {
    match raw {
        "working" => Status::Working,
        "idle" | "done" => Status::Idle,
        "blocked" => Status::Blocked,
        _ => Status::Unknown,
    }
}

/// The socket file of the Herdr session `session`.
///
/// # Errors
///
/// [`Error::HarnessUnavailable`] when neither `XDG_CONFIG_HOME` nor
/// `HOME` is set.
fn socket_path(session: &str) -> Result<PathBuf> {
    let config = match env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".config"))
            .ok_or_else(|| {
                Error::HarnessUnavailable(
                    "neither XDG_CONFIG_HOME nor HOME is set".into(),
                )
            })?,
    };

    Ok(socket_path_in(&config, session))
}

/// The socket file of `session` under the config directory `config`;
/// the session `default` uses the root socket.
fn socket_path_in(config: &Path, session: &str) -> PathBuf {
    let root = config.join("herdr");

    if session == "default" {
        return root.join("herdr.sock");
    }

    root.join("sessions").join(session).join("herdr.sock")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod classify_error {
        use super::*;

        #[rstest]
        #[case::pane(include_str!("../tests/fixtures/herdr/error_pane_not_found.json"), "not_found")]
        #[case::workspace(
            r#"{"error":{"code":"workspace_not_found","message":"m"}}"#,
            "not_found"
        )]
        #[case::agent(
            r#"{"error":{"code":"agent_not_found","message":"m"}}"#,
            "not_found"
        )]
        #[case::other_code(
            r#"{"error":{"code":"weird","message":"m"}}"#,
            "unexpected"
        )]
        #[case::unknown_not_found(
            r#"{"error":{"code":"weird_not_found","message":"m"}}"#,
            "unexpected"
        )]
        #[case::not_json("boom", "unexpected")]
        fn should_classify_by_code(#[case] stderr: &str, #[case] kind: &str) {
            assert_eq!(classify_error(stderr).kind(), kind);
        }

        #[test]
        fn should_include_code_and_message_when_envelope() {
            let err = classify_error(include_str!(
                "../tests/fixtures/herdr/error_pane_not_found.json"
            ));

            assert_eq!(
                err.to_string(),
                "pane_not_found: pane w1:p99 not found"
            );
        }
    }

    mod spawn_error {
        use super::*;

        #[rstest]
        #[case::missing(ErrorKind::NotFound, "herdr is not installed")]
        #[case::timeout(ErrorKind::TimedOut, "herdr timed out: ")]
        #[case::other(ErrorKind::PermissionDenied, "running herdr: ")]
        fn should_be_unavailable_when_spawn_fails(
            #[case] kind: ErrorKind,
            #[case] prefix: &str,
        ) {
            let err = spawn_error(&io::Error::new(kind, "x"));

            assert_eq!(err.kind(), "harness_unavailable");
            assert!(err.to_string().starts_with(prefix), "got {err}");
        }
    }

    mod args {
        use super::*;

        /// Owned argument strings from literals.
        fn v(items: &[&str]) -> Vec<String> {
            owned(items)
        }

        #[test]
        fn should_forward_args_after_separator_when_known_kind_has_rest() {
            assert_eq!(
                agent_start_args(
                    "n",
                    "claude",
                    "w1:p2",
                    &v(&["--model", "x"])
                ),
                v(&[
                    "agent", "start", "n", "--kind", "claude", "--pane",
                    "w1:p2", "--", "--model", "x"
                ])
            );
        }

        #[test]
        fn should_omit_separator_when_known_kind_has_no_rest() {
            assert_eq!(
                agent_start_args("n", "claude", "w1:p2", &[]),
                v(&[
                    "agent", "start", "n", "--kind", "claude", "--pane",
                    "w1:p2"
                ])
            );
        }

        #[rstest]
        #[case::right(Direction::Right, "right")]
        #[case::down(Direction::Down, "down")]
        fn should_pass_direction_when_splitting(
            #[case] direction: Direction,
            #[case] word: &str,
        ) {
            assert_eq!(
                split_args("w1:p1", direction, Path::new("/tmp")),
                v(&[
                    "pane",
                    "split",
                    "--pane",
                    "w1:p1",
                    "--direction",
                    word,
                    "--cwd",
                    "/tmp",
                    "--no-focus"
                ])
            );
        }

        #[test]
        fn should_read_visible_source_when_plain() {
            assert_eq!(
                read_args("w1:p1", 20, false),
                v(&[
                    "pane", "read", "w1:p1", "--source", "visible", "--lines",
                    "20"
                ])
            );
        }

        #[test]
        fn should_add_ansi_format_when_ansi() {
            assert_eq!(
                read_args("w1:p1", 5, true),
                v(&[
                    "pane", "read", "w1:p1", "--source", "visible", "--lines",
                    "5", "--format", "ansi"
                ])
            );
        }

        #[test]
        fn should_label_tab_without_focus_when_creating() {
            assert_eq!(
                tab_create_args("w1", Path::new("/tmp"), "lbl"),
                v(&[
                    "tab",
                    "create",
                    "--workspace",
                    "w1",
                    "--cwd",
                    "/tmp",
                    "--label",
                    "lbl",
                    "--no-focus"
                ])
            );
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
        fn should_map_herdr_status(
            #[case] raw: &str,
            #[case] expected: Status,
        ) {
            assert_eq!(map_status(raw), expected);
        }
    }

    mod socket_path {
        use super::*;

        #[test]
        fn should_use_sessions_dir_when_named() {
            let path = socket_path_in(Path::new("/c"), "personal");

            assert_eq!(
                path,
                PathBuf::from("/c/herdr/sessions/personal/herdr.sock")
            );
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

        #[test]
        fn should_read_pane_when_split() {
            let got: PaneGet = serde_json::from_str(include_str!(
                "../tests/fixtures/herdr/pane_split.json"
            ))
            .unwrap();

            assert_eq!(got.result.pane.pane_id, "w1:p3");
        }
    }
}
