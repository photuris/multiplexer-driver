//! tmux driver (§7). Workspaces are sessions, tabs are windows, and
//! handles are tmux pane IDs (`%12`).

use std::{
    env, io,
    process::{Command, Output},
    thread,
    time::Duration,
};

use crate::{
    agents::is_known_kind,
    driver::{Driver, Error, Result, SpawnRequest, SplitRequest},
    model::{Confidence, Direction, Handle, Status, StatusResult},
    patterns::Patterns,
    process,
    text::{shell_join, trim_trailing_blank_lines},
};

/// The pane format every pane-creating command prints.
const PANE_ID: &str = "#{pane_id}";

/// Lines of output `pane status` classifies.
const STATUS_LINES: u32 = 15;

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
        let output = self.output(args)?;

        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);

        Err(classify_error(stderr.trim()))
    }

    /// Runs tmux once under the time ceiling and returns its raw
    /// output, whatever the exit status. Only spawn failures and the
    /// timeout are errors.
    fn output(&self, args: &[&str]) -> Result<Output> {
        let mut command = Command::new("tmux");

        command.args(self.base_args()).args(args);

        process::output_within(&mut command, process::CEILING).map_err(|e| {
            match e.kind() {
                io::ErrorKind::NotFound => {
                    Error::HarnessUnavailable("tmux is not installed".into())
                }
                io::ErrorKind::TimedOut => {
                    Error::HarnessUnavailable(format!("tmux timed out: {e}"))
                }
                _ => Error::HarnessUnavailable(format!("running tmux: {e}")),
            }
        })
    }

    /// Workspace for `spawn` without `--workspace`: the caller's
    /// session, only when running inside tmux on the ambient server
    /// (§7.2).
    fn default_workspace(&self) -> Result<String> {
        if self.session.is_some() || env::var_os("TMUX").is_none() {
            return Err(Error::Usage(
                "--workspace is required outside the current tmux server"
                    .into(),
            ));
        }

        Ok(self
            .run(&["display-message", "-p", "#{session_id}"])?
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
    fn set_pane_option(
        &self,
        pane: &str,
        key: &str,
        value: &str,
    ) -> Result<()> {
        self.run(&["set-option", "-p", "-t", pane, key, value])
            .map(|_| ())
    }
}

impl Driver for Tmux {
    fn name(&self) -> &'static str {
        "tmux"
    }

    fn pane_spawn(&self, request: &SpawnRequest<'_>) -> Result<Handle> {
        let workspace = match request.workspace {
            Some(ws) => ws.to_string(),
            None => self.default_workspace()?,
        };
        let cwd = request.cwd.to_string_lossy();
        let joined = shell_join(request.command);
        let mut args = vec![
            "new-window",
            "-d",
            "-t",
            &workspace,
            "-n",
            request.name,
            "-c",
            &cwd,
            "-P",
            "-F",
            PANE_ID,
        ];

        if !request.command.is_empty() {
            args.push(&joined);
        }

        let handle = Handle(self.run(&args)?.trim().to_string());

        self.label_new_pane(&handle, request.name, request.command)?;

        Ok(handle)
    }

    fn pane_split(&self, request: &SplitRequest<'_>) -> Result<Handle> {
        let cwd = request.cwd.to_string_lossy();
        let joined = shell_join(request.command);
        let flag = match request.direction {
            Direction::Right => "-h",
            Direction::Down => "-v",
        };
        let mut args = vec![
            "split-window",
            "-d",
            "-t",
            &request.target.0,
            flag,
            "-c",
            &cwd,
            "-P",
            "-F",
            PANE_ID,
        ];

        if !request.command.is_empty() {
            args.push(&joined);
        }

        let handle = Handle(self.run(&args)?.trim().to_string());

        self.label_new_pane(&handle, request.name, request.command)?;

        Ok(handle)
    }

    fn pane_read(
        &self,
        target: &Handle,
        lines: u32,
        ansi: bool,
    ) -> Result<String> {
        let start = format!("-{lines}");
        let mut args =
            vec!["capture-pane", "-p", "-t", &target.0, "-S", &start];

        if ansi {
            args.extend(["-e", "-J"]);
        }

        Ok(trim_trailing_blank_lines(&self.run(&args)?))
    }

    fn pane_prompt(&self, target: &Handle, text: &str) -> Result<()> {
        // `--` keeps text that starts with `-` from parsing as a flag.
        self.run(&["send-keys", "-t", &target.0, "-l", "--", text])?;
        self.run(&["send-keys", "-t", &target.0, "Enter"])?;

        Ok(())
    }

    fn pane_rename(&self, target: &Handle, label: &str) -> Result<()> {
        self.set_pane_option(&target.0, "@md-label", label)?;
        self.run(&["select-pane", "-t", &target.0, "-T", label])?;

        Ok(())
    }

    fn pane_interrupt(&self, target: &Handle) -> Result<()> {
        self.run(&["send-keys", "-t", &target.0, "C-c"])?;

        Ok(())
    }

    fn pane_kill(&self, target: &Handle) -> Result<()> {
        self.run(&["kill-pane", "-t", &target.0])?;

        Ok(())
    }

    fn pane_status(&self, target: &Handle) -> Result<StatusResult> {
        let tail = self.pane_read(target, STATUS_LINES, false)?;
        let (status, confidence) = self
            .patterns
            .as_ref()
            .map_or((Status::Unknown, Confidence::None), |p| {
                p.classify(&tail)
            });

        Ok(StatusResult {
            handle: target.clone(),
            status,
            confidence,
            tail: Some(tail),
        })
    }

    fn raw(&self, args: &[String]) -> Result<String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.output(&args)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            return Err(Error::Unexpected(stderr.trim().to_string()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod classify_error {
        use super::*;

        #[rstest]
        #[case::pane("can't find pane: %99", "not_found")]
        #[case::session("can't find session: $99", "not_found")]
        #[case::window("can't find window: @9", "not_found")]
        #[case::no_server(
            "no server running on /tmp/tmux-1000/x",
            "harness_unavailable"
        )]
        #[case::no_socket(
            "error connecting to /tmp/tmux-1000/x (No such file or directory)",
            "harness_unavailable"
        )]
        #[case::permission(
            "error connecting to /x (Permission denied)",
            "harness_unavailable"
        )]
        #[case::duplicate("duplicate session: ws", "usage")]
        #[case::exiting("server exited unexpectedly", "unexpected")]
        #[case::other("something odd", "unexpected")]
        fn should_classify_tmux_stderr(
            #[case] stderr: &str,
            #[case] kind: &str,
        ) {
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
}
