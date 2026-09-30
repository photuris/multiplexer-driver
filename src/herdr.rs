//! Herdr driver (§8). Handles are Herdr pane IDs (`w1:p2`).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env,
    io::{self, ErrorKind},
    path::Path,
    process::{Command, Output},
};

use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    agents::is_known_kind,
    driver::{Driver, Error, Result, SpawnRequest, SplitRequest},
    model::{
        AgentSession, Confidence, Direction, Handle, PaneRecord,
        SessionRefKind, Status, StatusResult, WorkspaceRecord,
    },
    process::{CEILING, output_within},
    text::trim_trailing_blank_lines,
};

/// Drives one Herdr server.
#[derive(Debug)]
pub struct Herdr {
    /// Session name; `None` lets the ambient environment pick the
    /// server.
    session: Option<String>,
}

impl Herdr {
    /// Creates a driver for the Herdr session `session` (or the
    /// ambient one).
    pub fn new(session: Option<String>) -> Self {
        Self { session }
    }

    /// Runs `herdr args…` under the ceiling and returns the raw
    /// output. With a session, the command is `herdr --session <name>
    /// args…` and every `HERDR_*` variable is removed from the child,
    /// so the caller's own session never leaks in (§13.1).
    fn output<S: AsRef<str>>(&self, args: &[S]) -> Result<Output> {
        let mut command = Command::new("herdr");

        if let Some(name) = &self.session {
            command.args(["--session", name]);

            for (key, _) in env::vars_os() {
                if key.to_string_lossy().starts_with("HERDR_") {
                    command.env_remove(key);
                }
            }
        }

        command.args(args.iter().map(AsRef::as_ref));

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

    /// Starts `command` in the existing `pane` (§8.2). `line` is the
    /// shell line from [`launch_line`]; it is `None` for a known agent
    /// kind. Every failure here is [`Error::StartFailed`] carrying the
    /// pane.
    fn start_in_pane(
        &self,
        pane: &str,
        name: &str,
        command: &[String],
        line: Option<&str>,
    ) -> Result<Handle> {
        let handle = Handle(pane.to_string());

        self.start_command(pane, name, command, line).map_err(|e| {
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
        line: Option<&str>,
    ) -> Result<()> {
        let Some(first) = command.first() else {
            return Err(Error::Usage("a command is required".into()));
        };

        let Some(line) = line else {
            self.run(&agent_start_args(name, first, pane, &command[1..]))?;

            return Ok(());
        };

        self.run(&["pane", "run", pane, line])?;
        self.run(&["pane", "rename", pane, name])?;

        Ok(())
    }
}

impl Driver for Herdr {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn workspace_list(&self) -> Result<Vec<WorkspaceRecord>> {
        let list: WorkspaceList = self.run_json(&["workspace", "list"])?;

        Ok(list
            .result
            .workspaces
            .into_iter()
            .map(WorkspaceRecord::from)
            .collect())
    }

    fn workspace_create(
        &self,
        label: &str,
        cwd: &Path,
    ) -> Result<WorkspaceRecord> {
        let created: WorkspaceCreated =
            self.run_json(&workspace_create_args(label, cwd))?;

        Ok(WorkspaceRecord::from(created.result.workspace))
    }

    fn workspace_close(&self, id: &str) -> Result<()> {
        self.run(&["workspace", "close", id])?;

        Ok(())
    }

    fn workspace_tag(
        &self,
        id: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<BTreeMap<String, String>> {
        self.run(&workspace_tag_args(id, set, clear))?;

        let got: WorkspaceGet = self.run_json(&["workspace", "get", id])?;

        Ok(got.result.workspace.tokens)
    }

    fn pane_list(&self, workspace: Option<&str>) -> Result<Vec<PaneRecord>> {
        let panes: PaneList = self.run_json(&pane_list_args(workspace))?;
        let panes = panes.result.panes;
        let workspaces: WorkspaceList =
            self.run_json(&["workspace", "list"])?;
        let in_use: BTreeSet<&str> =
            panes.iter().map(|p| p.workspace_id.as_str()).collect();
        let mut tabs = HashMap::new();

        for id in in_use {
            let list: TabList = self.run_json(&tab_list_args(id))?;

            tabs.extend(tab_labels(&list.result.tabs));
        }

        Ok(build_pane_records(
            panes,
            &workspace_labels(&workspaces.result.workspaces),
            &tabs,
        ))
    }

    fn pane_spawn(&self, request: &SpawnRequest<'_>) -> Result<Handle> {
        if request.command.is_empty() {
            return Err(Error::Usage("a command is required on herdr".into()));
        }

        let line = launch_line(request.command)?;
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
            line.as_deref(),
        )
    }

    fn pane_split(&self, request: &SplitRequest<'_>) -> Result<Handle> {
        if request.command.is_empty() {
            return Err(Error::Usage("a command is required on herdr".into()));
        }

        let line = launch_line(request.command)?;
        let split: PaneGet = self.run_json(&split_args(
            &request.target.0,
            request.direction,
            request.cwd,
        ))?;

        self.start_in_pane(
            &split.result.pane.pane_id,
            request.name,
            request.command,
            line.as_deref(),
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
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();

            return Err(match classify_error(stderr) {
                err @ Error::HarnessUnavailable(_) => err,
                _ => Error::Unexpected(stderr.to_string()),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The shell line `pane run` gets for `command`, built before any tab
/// or pane exists so a rejected command creates nothing. `None` for a
/// known agent kind, which `agent start` takes as argv.
///
/// # Errors
///
/// [`Error::Usage`] when `command` is empty, or (on Windows) when
/// PowerShell cannot carry an argument.
fn launch_line(command: &[String]) -> Result<Option<String>> {
    match command.first() {
        Some(first) if is_known_kind(first) => Ok(None),
        _ => command_line(command).map(Some),
    }
}

/// Renders `command` for the pane's shell: PowerShell on Windows
/// (§13.2), POSIX elsewhere.
///
/// # Errors
///
/// [`Error::Usage`] when PowerShell cannot carry an argument.
#[cfg(windows)]
fn command_line(command: &[String]) -> Result<String> {
    crate::text::powershell_join(command)
}

/// Renders `command` for the pane's shell: PowerShell on Windows
/// (§13.2), POSIX elsewhere.
///
/// # Errors
///
/// Never fails on this platform.
#[cfg(not(windows))]
fn command_line(command: &[String]) -> Result<String> {
    Ok(crate::text::shell_join(command))
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

/// Arguments for `workspace create` rooted at `cwd`.
fn workspace_create_args(label: &str, cwd: &Path) -> Vec<String> {
    let mut args = owned(&["workspace", "create", "--label", label]);

    args.extend(["--cwd".into(), cwd.to_string_lossy().into_owned()]);
    args.push("--no-focus".into());

    args
}

/// Arguments for one `workspace report-metadata` call that sets the
/// `set` tokens and clears the `clear` tokens.
fn workspace_tag_args(
    id: &str,
    set: &[(String, String)],
    clear: &[String],
) -> Vec<String> {
    let mut args = owned(&["workspace", "report-metadata", id]);

    args.extend(owned(&["--source", "multiplexer-driver"]));

    for (key, value) in set {
        args.extend(["--token".into(), format!("{key}={value}")]);
    }

    for key in clear {
        args.extend(["--clear-token".into(), key.clone()]);
    }

    args
}

/// Arguments for `pane list`, limited to `workspace` when given.
fn pane_list_args(workspace: Option<&str>) -> Vec<String> {
    let mut args = owned(&["pane", "list"]);

    if let Some(ws) = workspace {
        args.extend(["--workspace".into(), ws.to_string()]);
    }

    args
}

/// Arguments for `tab list` of `workspace`.
fn tab_list_args(workspace: &str) -> Vec<String> {
    owned(&["tab", "list", "--workspace", workspace])
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

/// `{"result":{…}}` from `pane list`.
#[derive(Debug, Deserialize)]
struct PaneList {
    /// Result body.
    result: PaneListResult,
}

/// Body of [`PaneList`].
#[derive(Debug, Deserialize)]
struct PaneListResult {
    /// Every listed pane.
    panes: Vec<PaneListItem>,
}

/// One entry of `pane list`.
#[derive(Debug, Deserialize)]
struct PaneListItem {
    /// Pane ID.
    pane_id: String,
    /// Workspace ID.
    workspace_id: String,
    /// Tab ID.
    tab_id: String,
    /// Working directory.
    #[serde(default)]
    cwd: Option<String>,
    /// Label, present only after a rename.
    #[serde(default)]
    label: Option<String>,
    /// Agent kind, when Herdr recognizes one.
    #[serde(default)]
    agent: Option<String>,
    /// Agent conversation reference.
    #[serde(default)]
    agent_session: Option<HerdrSession>,
    /// Agent status string.
    #[serde(default)]
    agent_status: String,
}

/// An agent session as Herdr prints it.
#[derive(Debug, Deserialize)]
struct HerdrSession {
    /// Whether `value` is an ID or a path.
    kind: SessionRefKind,
    /// The ID or path.
    value: String,
}

/// `{"result":{…}}` from `workspace list`.
#[derive(Debug, Deserialize)]
struct WorkspaceList {
    /// Result body.
    result: WorkspaceListResult,
}

/// Body of [`WorkspaceList`].
#[derive(Debug, Deserialize)]
struct WorkspaceListResult {
    /// Every workspace.
    workspaces: Vec<WorkspaceItem>,
}

/// One workspace as Herdr prints it.
#[derive(Debug, Deserialize)]
struct WorkspaceItem {
    /// Workspace ID.
    workspace_id: String,
    /// Display label.
    label: String,
    /// Whether the user looks at it.
    #[serde(default)]
    focused: bool,
    /// Metadata tokens; absent when empty.
    #[serde(default)]
    tokens: BTreeMap<String, String>,
}

impl From<WorkspaceItem> for WorkspaceRecord {
    /// Maps `tokens` to `tags`.
    fn from(item: WorkspaceItem) -> Self {
        Self {
            workspace_id: item.workspace_id,
            label: item.label,
            focused: item.focused,
            tags: item.tokens,
        }
    }
}

/// `{"result":{"workspace":…}}` from `workspace create`.
#[derive(Debug, Deserialize)]
struct WorkspaceCreated {
    /// Result body.
    result: WorkspaceResult,
}

/// `{"result":{"workspace":…}}` from `workspace get`.
#[derive(Debug, Deserialize)]
struct WorkspaceGet {
    /// Result body.
    result: WorkspaceResult,
}

/// Body of [`WorkspaceCreated`] and [`WorkspaceGet`].
#[derive(Debug, Deserialize)]
struct WorkspaceResult {
    /// The workspace.
    workspace: WorkspaceItem,
}

/// `{"result":{…}}` from `tab list`.
#[derive(Debug, Deserialize)]
struct TabList {
    /// Result body.
    result: TabListResult,
}

/// Body of [`TabList`].
#[derive(Debug, Deserialize)]
struct TabListResult {
    /// Tabs of one workspace.
    tabs: Vec<TabItem>,
}

/// One tab as Herdr prints it.
#[derive(Debug, Deserialize)]
struct TabItem {
    /// Tab ID.
    tab_id: String,
    /// Display label.
    label: String,
}

/// Workspace ID to label.
fn workspace_labels(items: &[WorkspaceItem]) -> HashMap<String, String> {
    items
        .iter()
        .map(|w| (w.workspace_id.clone(), w.label.clone()))
        .collect()
}

/// Tab ID to label.
fn tab_labels(items: &[TabItem]) -> HashMap<String, String> {
    items
        .iter()
        .map(|t| (t.tab_id.clone(), t.label.clone()))
        .collect()
}

/// Builds pane records from `pane list` output plus workspace and tab
/// labels (§8.1). A session means `native` session confidence; the
/// status is always native.
fn build_pane_records(
    panes: Vec<PaneListItem>,
    workspace_labels: &HashMap<String, String>,
    tab_labels: &HashMap<String, String>,
) -> Vec<PaneRecord> {
    panes
        .into_iter()
        .map(|pane| {
            let session_confidence = if pane.agent_session.is_some() {
                Confidence::Native
            } else {
                Confidence::None
            };

            PaneRecord {
                handle: Handle(pane.pane_id),
                workspace_label: workspace_labels
                    .get(&pane.workspace_id)
                    .cloned(),
                workspace_id: pane.workspace_id,
                tab_label: tab_labels.get(&pane.tab_id).cloned(),
                tab_id: pane.tab_id,
                label: pane.label,
                cwd: pane.cwd,
                agent: pane.agent,
                agent_session: pane.agent_session.map(|s| AgentSession {
                    kind: s.kind,
                    value: s.value,
                }),
                session_confidence,
                status: map_status(&pane.agent_status),
                status_confidence: Confidence::Native,
            }
        })
        .collect()
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
/// [`Error::NotFound`], and `server_not_running` is
/// [`Error::HarnessUnavailable`]. Anything else is
/// [`Error::Unexpected`].
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
        "server_not_running" => Error::HarnessUnavailable(text),
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
        #[case::server_down(
            r#"{"error":{"code":"server_not_running","message":"m"}}"#,
            "harness_unavailable"
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

    mod list_args {
        use super::*;

        /// Owned argument strings from literals.
        fn v(items: &[&str]) -> Vec<String> {
            owned(items)
        }

        #[test]
        fn should_create_workspace_without_focus() {
            assert_eq!(
                workspace_create_args("lbl", Path::new("/tmp")),
                v(&[
                    "workspace",
                    "create",
                    "--label",
                    "lbl",
                    "--cwd",
                    "/tmp",
                    "--no-focus"
                ])
            );
        }

        #[test]
        fn should_report_one_token_flag_per_pair_when_tagging() {
            assert_eq!(
                workspace_tag_args(
                    "w1",
                    &[("a".into(), "1".into()), ("b".into(), "x=y".into())],
                    &["c".into()]
                ),
                v(&[
                    "workspace",
                    "report-metadata",
                    "w1",
                    "--source",
                    "multiplexer-driver",
                    "--token",
                    "a=1",
                    "--token",
                    "b=x=y",
                    "--clear-token",
                    "c"
                ])
            );
        }

        #[rstest]
        #[case::all(None, &["pane", "list"])]
        #[case::one(Some("w1"), &["pane", "list", "--workspace", "w1"])]
        fn should_scope_pane_list_when_workspace_given(
            #[case] workspace: Option<&str>,
            #[case] expected: &[&str],
        ) {
            assert_eq!(pane_list_args(workspace), v(expected));
        }

        #[test]
        fn should_scope_tab_list_to_workspace() {
            assert_eq!(
                tab_list_args("w3"),
                v(&["tab", "list", "--workspace", "w3"])
            );
        }
    }

    mod build_pane_records {
        use super::*;

        /// Parses the fixtures and joins them.
        fn records() -> Vec<PaneRecord> {
            let panes: PaneList = serde_json::from_str(include_str!(
                "../tests/fixtures/herdr/pane_list.json"
            ))
            .unwrap();
            let workspaces: WorkspaceList = serde_json::from_str(
                include_str!("../tests/fixtures/herdr/workspace_list.json"),
            )
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
                    "handle": "w1:p1",
                    "workspace_id": "w1",
                    "workspace_label": "probe",
                    "tab_id": "w1:t1",
                    "tab_label": "impl",
                    "label": "impl-1",
                    "cwd": "/home/u/proj",
                    "agent": "claude",
                    "agent_session": {
                        "kind": "id",
                        "value": "a6b4756a-1f0f-42b1-85da-9393980017be"
                    },
                    "session_confidence": "native",
                    "status": "idle",
                    "status_confidence": "native"
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
                (
                    shell.agent.as_deref(),
                    shell.session_confidence,
                    shell.tab_label.as_deref()
                ),
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
            let records: Vec<WorkspaceRecord> = list
                .result
                .workspaces
                .into_iter()
                .map(WorkspaceRecord::from)
                .collect();

            assert_eq!(
                (
                    records[0].tags.get("project").map(String::as_str),
                    records[1].tags.len()
                ),
                (Some("x"), 0)
            );
        }

        #[test]
        fn should_read_workspace_when_created() {
            let created: WorkspaceCreated = serde_json::from_str(
                include_str!("../tests/fixtures/herdr/workspace_create.json"),
            )
            .unwrap();

            assert_eq!(created.result.workspace.workspace_id, "w1");
        }
    }
}
