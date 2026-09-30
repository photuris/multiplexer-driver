//! Herdr driver (§8). Handles are Herdr pane IDs (`w1:p2`).

use std::{
    env,
    io::ErrorKind,
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
    fn output(&self, args: &[&str]) -> Result<Output> {
        let mut command = Command::new("herdr");

        command.args(args);

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

        output_within(&mut command, CEILING).map_err(|e| {
            if e.kind() == ErrorKind::TimedOut {
                Error::Unexpected(format!("running herdr: {e}"))
            } else {
                Error::HarnessUnavailable(format!("running herdr: {e}"))
            }
        })
    }

    /// Runs `herdr args…` and returns stdout; a failing exit is
    /// classified from its stderr envelope.
    fn run(&self, args: &[&str]) -> Result<String> {
        let output = self.output(args)?;

        if !output.status.success() {
            return Err(classify_error(
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs `herdr args…` and parses its JSON stdout as `T`.
    fn run_json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
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
            let mut args =
                vec!["agent", "start", name, "--kind", first, "--pane", pane];

            if command.len() > 1 {
                args.push("--");
                args.extend(command[1..].iter().map(String::as_str));
            }

            self.run(&args)?;

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
        let cwd = request.cwd.to_string_lossy();
        let created: TabCreated = self.run_json(&[
            "tab",
            "create",
            "--workspace",
            &workspace,
            "--cwd",
            &cwd,
            "--label",
            request.name,
            "--no-focus",
        ])?;

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

        let direction = match request.direction {
            Direction::Right => "right",
            Direction::Down => "down",
        };
        let cwd = request.cwd.to_string_lossy();
        let split: PaneGet = self.run_json(&[
            "pane",
            "split",
            "--pane",
            &request.target.0,
            "--direction",
            direction,
            "--cwd",
            &cwd,
            "--no-focus",
        ])?;

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
        let lines = lines.to_string();
        let mut args = vec![
            "pane", "read", &target.0,
            // Required: on herdr 0.8.2, --lines with the default source
            // silently returns nothing.
            "--source", "visible", "--lines", &lines,
        ];

        if ansi {
            args.extend(["--format", "ansi"]);
        }

        Ok(trim_trailing_blank_lines(&self.run(&args)?))
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
