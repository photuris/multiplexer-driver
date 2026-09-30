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
