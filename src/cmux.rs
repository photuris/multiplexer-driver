//! cmux driver. Handles are terminal surface UUIDs; workspaces are cmux
//! workspace UUIDs. Every argv here was verified against cmux 0.64.22.

use std::{
    collections::BTreeMap,
    io::{self, ErrorKind},
    path::Path,
    process::{Command, Output},
};

use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    driver::{Driver, Error, Result, SpawnRequest, SplitRequest, unsupported},
    model::{
        AgentSession, Confidence, Direction, Handle, PaneRecord,
        SessionRefKind, Status, StatusResult, WorkspaceRecord,
    },
    patterns::Patterns,
    process::{CEILING, output_within},
    text::{shell_join, trim_trailing_blank_lines},
};

/// stderr fragment of the refusal to close a workspace's only surface.
const LAST_SURFACE: &str = "Cannot close the last surface";

/// Drives one cmux app through its CLI.
#[derive(Debug)]
pub struct Cmux {
    /// Socket path; `None` lets the ambient environment pick the app.
    session: Option<String>,
    /// Status patterns for agents that report no usable lifecycle.
    patterns: Option<Patterns>,
}

impl Cmux {
    /// Creates a driver for the cmux socket `session` (or the ambient
    /// one), with optional fallback status `patterns`.
    pub fn new(session: Option<String>, patterns: Option<Patterns>) -> Self {
        Self { session, patterns }
    }

    /// Runs `cmux args…` under the ceiling and returns the raw output.
    /// With a session, the command is `cmux --socket <path> args…` and
    /// both socket variables are removed from the child, so the caller's
    /// own socket never leaks in. `CMUX_QUIET=1` silences deprecation
    /// notices on stderr.
    fn output<S: AsRef<str>>(&self, args: &[S]) -> Result<Output> {
        let mut command = Command::new("cmux");

        command.env("CMUX_QUIET", "1");

        if let Some(path) = &self.session {
            command.args(["--socket", path]);
            command.env_remove("CMUX_SOCKET_PATH");
            command.env_remove("CMUX_SOCKET");
        }

        command.args(args.iter().map(AsRef::as_ref));

        output_within(&mut command, CEILING).map_err(|e| spawn_error(&e))
    }

    /// Runs `cmux args…` and returns stdout; a failing exit is
    /// classified from its stderr.
    fn run<S: AsRef<str>>(&self, args: &[S]) -> Result<String> {
        let output = self.output(args)?;

        if !output.status.success() {
            return Err(classify_error(&String::from_utf8_lossy(
                &output.stderr,
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs `cmux args…` and parses its JSON stdout as `T`.
    fn run_json<T: DeserializeOwned, S: AsRef<str>>(
        &self,
        args: &[S],
    ) -> Result<T> {
        parse_json(&self.run(args)?)
    }

    /// Reads the whole tree of every window.
    fn tree_all(&self) -> Result<Tree> {
        self.run_json(&tree_all_args())
    }

    /// Reads the tree of one workspace and returns that workspace. `ws`
    /// may be a ref such as `workspace:4`.
    fn workspace_tree(&self, ws: &str) -> Result<WorkspaceJson> {
        let tree: Tree = self.run_json(&tree_workspace_args(ws))?;

        tree.windows
            .into_iter()
            .next()
            .and_then(|w| w.workspaces.into_iter().next())
            .ok_or_else(|| {
                Error::Unexpected(format!("cmux tree has no workspace {ws}"))
            })
    }

    /// Returns the workspace id that holds `surface`.
    fn workspace_of(&self, surface: &str) -> Result<String> {
        locate(&self.tree_all()?, surface).map(|(workspace, _)| workspace)
    }

    /// Reads the active sessions `args` selects.
    fn sessions(&self, args: &[String]) -> Result<Vec<SessionJson>> {
        let sessions: Sessions = self.run_json(args)?;

        Ok(sessions.sessions)
    }
}

impl Driver for Cmux {
    fn name(&self) -> &'static str {
        "cmux"
    }

    fn workspace_list(&self) -> Result<Vec<WorkspaceRecord>> {
        Ok(workspace_records(&self.tree_all()?))
    }

    fn workspace_create(
        &self,
        label: &str,
        cwd: &Path,
    ) -> Result<WorkspaceRecord> {
        let out = self.run(&new_workspace_args(label, cwd, None))?;
        let workspace =
            self.workspace_tree(&parse_ok_ref(&out, "workspace")?)?;

        Ok(WorkspaceRecord::from(&workspace))
    }

    fn workspace_close(&self, id: &str) -> Result<()> {
        self.run(&close_workspace_args(id))?;

        Ok(())
    }

    fn workspace_tag(
        &self,
        _id: &str,
        _set: &[(String, String)],
        _clear: &[String],
    ) -> Result<BTreeMap<String, String>> {
        Err(unsupported("cmux", "workspace tag"))
    }

    fn pane_list(&self, workspace: Option<&str>) -> Result<Vec<PaneRecord>> {
        let tree = self.tree_all()?;
        let sessions = self.sessions(&sessions_args(None))?;

        Ok(pane_records(&tree, &sessions, workspace))
    }

    fn pane_spawn(&self, request: &SpawnRequest<'_>) -> Result<Handle> {
        if request.command.is_empty() {
            return Err(Error::Usage("a command is required on cmux".into()));
        }

        if request.workspace.is_some() {
            return Err(Error::Usage(
                "cmux spawn always creates a workspace; --workspace is not \
                 accepted"
                    .into(),
            ));
        }

        let line = shell_join(request.command);
        let out = self.run(&new_workspace_args(
            request.name,
            request.cwd,
            Some(&line),
        ))?;
        let workspace =
            self.workspace_tree(&parse_ok_ref(&out, "workspace")?)?;
        let handle = workspace
            .panes
            .first()
            .and_then(|p| p.surfaces.iter().find(|s| s.is_terminal()))
            .map(|s| s.id.clone())
            .ok_or_else(|| {
                Error::Unexpected(
                    "new cmux workspace has no terminal surface".into(),
                )
            })?;
        let renamed = self
            .run(&rename_tab_args(&workspace.id, &handle, request.name))
            .map(drop);

        started(handle, renamed)
    }

    fn pane_split(&self, request: &SplitRequest<'_>) -> Result<Handle> {
        if request.command.is_empty() {
            return Err(Error::Usage("a command is required on cmux".into()));
        }

        let out =
            self.run(&new_split_args(request.direction, &request.target.0))?;
        let (surface, workspace) = parse_split_ok(&out)?;
        let identified = self.run(&identify_args(&workspace, &surface))?;
        let caller = parse_identify(&identified)?;
        let text = split_line(request.cwd, request.command);
        let finished = self
            .run(&paste_args(&caller.surface_id, &text))
            .and_then(|out| parse_paste(&out))
            .and_then(|()| {
                self.run(&rename_tab_args(
                    &caller.workspace_id,
                    &caller.surface_id,
                    request.name,
                ))
                .map(drop)
            });

        started(caller.surface_id, finished)
    }

    fn pane_read(
        &self,
        target: &Handle,
        lines: u32,
        ansi: bool,
    ) -> Result<String> {
        if ansi {
            return Err(unsupported("cmux", "pane read --ansi"));
        }

        read_text(&self.run(&read_screen_args(&target.0, lines))?)
    }

    fn pane_prompt(&self, target: &Handle, text: &str) -> Result<()> {
        parse_paste(&self.run(&paste_args(&target.0, text))?)
    }

    fn pane_rename(&self, target: &Handle, label: &str) -> Result<()> {
        let workspace = self.workspace_of(&target.0)?;

        self.run(&rename_tab_args(&workspace, &target.0, label))?;

        Ok(())
    }

    fn pane_interrupt(&self, target: &Handle) -> Result<()> {
        self.run(&owned(&["send-key", "--surface", &target.0, "ctrl+c"]))?;

        Ok(())
    }

    fn pane_kill(&self, target: &Handle) -> Result<()> {
        let workspace = self.workspace_of(&target.0)?;

        match self.run(&close_surface_args(&workspace, &target.0)) {
            Err(e) if e.to_string().contains(LAST_SURFACE) => {
                self.run(&close_workspace_args(&workspace))?;

                Ok(())
            }
            other => other.map(drop),
        }
    }

    fn pane_status(&self, target: &Handle) -> Result<StatusResult> {
        let sessions = self.sessions(&sessions_args(Some(&target.0)))?;

        resolve_status(target, &sessions, self.patterns.as_ref(), || {
            read_text(&self.run(&read_screen_args(&target.0, 15))?)
        })
    }

    fn raw(&self, args: &[String]) -> Result<String> {
        let output = self.output(args)?;

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

/// Maps a subprocess failure to an [`Error`]: a missing binary, a call
/// past the ceiling, and any other spawn failure are all
/// [`Error::HarnessUnavailable`].
fn spawn_error(e: &io::Error) -> Error {
    match e.kind() {
        ErrorKind::NotFound => {
            Error::HarnessUnavailable("cmux is not installed".into())
        }
        ErrorKind::TimedOut => {
            Error::HarnessUnavailable(format!("cmux timed out: {e}"))
        }
        _ => Error::HarnessUnavailable(format!("running cmux: {e}")),
    }
}

/// Turns cmux's stderr into an [`Error`]: an unreachable app is
/// [`Error::HarnessUnavailable`], `Error: not_found:` is
/// [`Error::NotFound`], and anything else is [`Error::Unexpected`].
fn classify_error(stderr: &str) -> Error {
    let stderr = stderr.trim();
    let unreachable = [
        "Socket not found",
        "Access denied",
        "Invalid password",
        "Connection refused",
    ];

    if unreachable.iter().any(|m| stderr.contains(m)) {
        return Error::HarnessUnavailable(stderr.to_string());
    }

    if stderr.starts_with("Error: not_found:") {
        return Error::NotFound(stderr.to_string());
    }

    Error::Unexpected(stderr.to_string())
}

/// Maps a cmux `agent_lifecycle` onto our status.
fn map_lifecycle(raw: &str) -> Status {
    match raw {
        "running" => Status::Working,
        "idle" => Status::Idle,
        "needsInput" => Status::Blocked,
        _ => Status::Unknown,
    }
}

/// Finishes a start whose pane already exists: a failed `result` is an
/// [`Error::StartFailed`] that carries the pane.
fn started(handle: String, result: Result<()>) -> Result<Handle> {
    let handle = Handle(handle);

    match result {
        Ok(()) => Ok(handle),
        Err(e) => Err(Error::StartFailed {
            handle,
            message: e.to_string(),
        }),
    }
}

/// The line typed into a fresh split: it changes to `cwd`, because
/// `new-split` takes no working directory, then runs `command`.
fn split_line(cwd: &Path, command: &[String]) -> String {
    let cd = shell_join(&["cd".into(), cwd.to_string_lossy().into_owned()]);

    format!("{cd} && {}", shell_join(command))
}

/// Picks the status for `target`. A counting session with a usable
/// lifecycle is native. Otherwise, with `patterns`, `read` supplies the
/// screen text to classify; without them, nothing is read.
fn resolve_status(
    target: &Handle,
    sessions: &[SessionJson],
    patterns: Option<&Patterns>,
    read: impl FnOnce() -> Result<String>,
) -> Result<StatusResult> {
    if let Some(status) = native_status(sessions, &target.0) {
        return Ok(StatusResult {
            handle: target.clone(),
            status,
            confidence: Confidence::Native,
            tail: None,
        });
    }

    let Some(patterns) = patterns else {
        return Ok(StatusResult {
            handle: target.clone(),
            status: Status::Unknown,
            confidence: Confidence::None,
            tail: None,
        });
    };
    let text = read()?;
    let (status, confidence) = patterns.classify(&text);

    Ok(StatusResult {
        handle: target.clone(),
        status,
        confidence,
        tail: Some(text),
    })
}

/// The native status of `surface`: `None` when no session counts or its
/// lifecycle maps to [`Status::Unknown`].
fn native_status(sessions: &[SessionJson], surface: &str) -> Option<Status> {
    let session = counting_session(sessions, surface)?;

    match session_status(session) {
        Status::Unknown => None,
        status => Some(status),
    }
}

/// The session that counts for `surface`: active for it, with a live
/// pid. The newest wins; on a tie the later one in the array wins.
fn counting_session<'a>(
    sessions: &'a [SessionJson],
    surface: &str,
) -> Option<&'a SessionJson> {
    sessions
        .iter()
        .filter(|s| {
            s.active_for_surface
                && s.stored_pid_exists
                && s.surface_id.as_deref() == Some(surface)
        })
        .max_by(|a, b| a.updated_at_unix.total_cmp(&b.updated_at_unix))
}

/// The status a session's lifecycle maps to.
fn session_status(session: &SessionJson) -> Status {
    map_lifecycle(session.agent_lifecycle.as_deref().unwrap_or_default())
}

/// One record per terminal surface, in tree order, limited to
/// `workspace` when given and joined with the counting sessions.
fn pane_records(
    tree: &Tree,
    sessions: &[SessionJson],
    workspace: Option<&str>,
) -> Vec<PaneRecord> {
    let mut records = Vec::new();

    for ws in tree.windows.iter().flat_map(|w| &w.workspaces) {
        if workspace.is_some_and(|id| id != ws.id) {
            continue;
        }

        for pane in &ws.panes {
            for surface in pane.surfaces.iter().filter(|s| s.is_terminal()) {
                let session = counting_session(sessions, &surface.id);

                records.push(pane_record(ws, pane, surface, session));
            }
        }
    }

    records
}

/// Builds the record of one surface.
fn pane_record(
    ws: &WorkspaceJson,
    pane: &PaneJson,
    surface: &SurfaceJson,
    session: Option<&SessionJson>,
) -> PaneRecord {
    let status = session.map_or(Status::Unknown, session_status);
    let status_confidence = match status {
        Status::Unknown => Confidence::None,
        _ => Confidence::Native,
    };

    PaneRecord {
        handle: Handle(surface.id.clone()),
        workspace_id: ws.id.clone(),
        workspace_label: Some(ws.title.clone()),
        tab_id: pane.id.clone(),
        tab_label: None,
        label: Some(surface.title.clone()),
        cwd: session.and_then(|s| s.cwd.clone()),
        agent: session.map(|s| s.agent.clone()),
        agent_session: session.map(|s| AgentSession {
            kind: SessionRefKind::Id,
            value: s.session_id.clone(),
        }),
        session_confidence: if session.is_some() {
            Confidence::Native
        } else {
            Confidence::None
        },
        status,
        status_confidence,
    }
}

/// One record per workspace of every window, in tree order.
fn workspace_records(tree: &Tree) -> Vec<WorkspaceRecord> {
    tree.windows
        .iter()
        .flat_map(|w| &w.workspaces)
        .map(WorkspaceRecord::from)
        .collect()
}

/// Finds `surface` in `tree` and returns its workspace id and pane id.
fn locate(tree: &Tree, surface: &str) -> Result<(String, String)> {
    for ws in tree.windows.iter().flat_map(|w| &w.workspaces) {
        for pane in &ws.panes {
            if pane.surfaces.iter().any(|s| s.id == surface) {
                return Ok((ws.id.clone(), pane.id.clone()));
            }
        }
    }

    Err(Error::NotFound(format!("surface {surface} not found")))
}

/// Whether `token` is `<noun>:<digits>`.
fn is_ref(token: &str, noun: &str) -> bool {
    token
        .strip_prefix(noun)
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(|n| {
            !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Parses `OK <noun>:<n>` and returns the `<noun>:<n>` token.
fn parse_ok_ref(stdout: &str, noun: &str) -> Result<String> {
    stdout
        .trim()
        .strip_prefix("OK ")
        .filter(|token| is_ref(token, noun))
        .map(str::to_string)
        .ok_or_else(|| {
            Error::Unexpected(format!(
                "unexpected cmux output: {:?}",
                stdout.trim()
            ))
        })
}

/// Parses `OK surface:<n> workspace:<m>` into both refs.
fn parse_split_ok(stdout: &str) -> Result<(String, String)> {
    let refs = stdout
        .trim()
        .strip_prefix("OK ")
        .map(|rest| rest.split_whitespace().collect::<Vec<_>>());

    match refs.as_deref() {
        Some([surface, workspace])
            if is_ref(surface, "surface")
                && is_ref(workspace, "workspace") =>
        {
            Ok(((*surface).to_string(), (*workspace).to_string()))
        }
        _ => Err(Error::Unexpected(format!(
            "unexpected cmux output: {:?}",
            stdout.trim()
        ))),
    }
}

/// Parses cmux JSON output as `T`.
fn parse_json<T: DeserializeOwned>(stdout: &str) -> Result<T> {
    serde_json::from_str(stdout)
        .map_err(|e| Error::Unexpected(format!("parsing cmux output: {e}")))
}

/// Parses `identify` output and returns its caller.
fn parse_identify(stdout: &str) -> Result<Caller> {
    let identify: Identify = parse_json(stdout)?;

    identify.caller.ok_or_else(|| {
        Error::Unexpected("cmux identify returned no caller".into())
    })
}

/// Checks that a `terminal.paste` reply says the text was submitted.
fn parse_paste(stdout: &str) -> Result<()> {
    let paste: Paste = parse_json(stdout)?;

    if !paste.submitted {
        return Err(Error::Unexpected(
            "cmux did not submit the pasted text".into(),
        ));
    }

    Ok(())
}

/// The screen text of a `read-screen --json` reply, trailing blank
/// lines trimmed.
fn read_text(stdout: &str) -> Result<String> {
    let screen: ReadScreen = parse_json(stdout)?;

    Ok(trim_trailing_blank_lines(&screen.text))
}

/// The compact JSON params of `terminal.paste`. Built with `json!`, so
/// quotes and backslashes in `text` are escaped.
fn paste_params(surface: &str, text: &str) -> String {
    serde_json::json!({
        "submit_key": "return",
        "surface_id": surface,
        "text": text,
    })
    .to_string()
}

/// Converts string slices to owned argument strings.
fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

/// Arguments for the whole-tree read.
fn tree_all_args() -> Vec<String> {
    owned(&["tree", "--all", "--json", "--id-format", "uuids"])
}

/// Arguments for the tree of one workspace.
fn tree_workspace_args(ws: &str) -> Vec<String> {
    owned(&["tree", "--workspace", ws, "--json", "--id-format", "uuids"])
}

/// Arguments for `new-workspace`; `command` is the shell line to run.
fn new_workspace_args(
    name: &str,
    cwd: &Path,
    command: Option<&str>,
) -> Vec<String> {
    let mut args = owned(&["new-workspace", "--name", name, "--cwd"]);

    args.push(cwd.to_string_lossy().into_owned());

    if let Some(line) = command {
        args.extend(owned(&["--command", line]));
    }

    args.extend(owned(&["--focus", "false"]));

    args
}

/// Arguments for `close-workspace`.
fn close_workspace_args(ws: &str) -> Vec<String> {
    owned(&["close-workspace", "--workspace", ws])
}

/// Arguments for `new-split` of `target`.
fn new_split_args(direction: Direction, target: &str) -> Vec<String> {
    let direction = match direction {
        Direction::Right => "right",
        Direction::Down => "down",
    };

    owned(&[
        "new-split",
        direction,
        "--surface",
        target,
        "--focus",
        "false",
    ])
}

/// Arguments for `identify` of a split's refs.
fn identify_args(workspace: &str, surface: &str) -> Vec<String> {
    owned(&[
        "identify",
        "--workspace",
        workspace,
        "--surface",
        surface,
        "--json",
        "--id-format",
        "uuids",
    ])
}

/// Arguments for `rename-tab`; `--` keeps a label that starts with a
/// dash from being read as a flag.
fn rename_tab_args(ws: &str, surface: &str, label: &str) -> Vec<String> {
    owned(&[
        "rename-tab",
        "--workspace",
        ws,
        "--surface",
        surface,
        "--",
        label,
    ])
}

/// Arguments for `close-surface`.
fn close_surface_args(ws: &str, surface: &str) -> Vec<String> {
    owned(&["close-surface", "--workspace", ws, "--surface", surface])
}

/// Arguments for `read-screen` of the last `lines` lines.
fn read_screen_args(surface: &str, lines: u32) -> Vec<String> {
    let mut args = owned(&["read-screen", "--surface", surface, "--lines"]);

    args.push(lines.to_string());
    args.push("--json".into());

    args
}

/// Arguments for `rpc terminal.paste` of `text` into `surface`.
fn paste_args(surface: &str, text: &str) -> Vec<String> {
    owned(&["rpc", "terminal.paste", &paste_params(surface, text)])
}

/// Arguments for `sessions`: those of one `surface`, else all.
fn sessions_args(surface: Option<&str>) -> Vec<String> {
    match surface {
        Some(id) => owned(&["sessions", "--json", "--surface", id]),
        None => owned(&["sessions", "--json", "--all"]),
    }
}

/// `tree --json`: only the windows are read.
#[derive(Debug, Deserialize)]
struct Tree {
    /// Every window.
    #[serde(default)]
    windows: Vec<WindowJson>,
}

/// One window of the tree.
#[derive(Debug, Deserialize)]
struct WindowJson {
    /// Its workspaces.
    #[serde(default)]
    workspaces: Vec<WorkspaceJson>,
}

/// One workspace of the tree.
#[derive(Debug, Deserialize)]
struct WorkspaceJson {
    /// Workspace UUID.
    id: String,
    /// Display title.
    #[serde(default)]
    title: String,
    /// Whether the user looks at it.
    #[serde(default)]
    active: bool,
    /// Its panes.
    #[serde(default)]
    panes: Vec<PaneJson>,
}

impl From<&WorkspaceJson> for WorkspaceRecord {
    /// Maps `title` to the label and `active` to `focused`.
    fn from(ws: &WorkspaceJson) -> Self {
        Self {
            workspace_id: ws.id.clone(),
            label: ws.title.clone(),
            focused: ws.active,
            tags: BTreeMap::new(),
        }
    }
}

/// One pane (split container) of the tree.
#[derive(Debug, Deserialize)]
struct PaneJson {
    /// Pane UUID.
    id: String,
    /// Its surfaces (tabs).
    #[serde(default)]
    surfaces: Vec<SurfaceJson>,
}

/// One surface (tab) of the tree.
#[derive(Debug, Deserialize)]
struct SurfaceJson {
    /// Surface UUID.
    id: String,
    /// Tab title.
    #[serde(default)]
    title: String,
    /// `terminal` or `browser`.
    #[serde(default, rename = "type")]
    kind: String,
}

impl SurfaceJson {
    /// Only terminal surfaces are panes for this driver.
    fn is_terminal(&self) -> bool {
        self.kind == "terminal"
    }
}

/// `identify --json`.
#[derive(Debug, Deserialize)]
struct Identify {
    /// The surface that asked; `null` outside cmux.
    #[serde(default)]
    caller: Option<Caller>,
}

/// The `caller` of [`Identify`].
#[derive(Debug, Deserialize)]
struct Caller {
    /// Surface UUID.
    surface_id: String,
    /// Workspace UUID.
    workspace_id: String,
}

/// `sessions --json`: only the sessions are read.
#[derive(Debug, Deserialize)]
struct Sessions {
    /// Every session the query matched.
    #[serde(default)]
    sessions: Vec<SessionJson>,
}

/// One agent session recorded by cmux's hooks.
#[derive(Debug, Deserialize)]
struct SessionJson {
    /// Agent kind, e.g. `claude`.
    agent: String,
    /// Conversation ID.
    session_id: String,
    /// The surface the session ran in.
    #[serde(default)]
    surface_id: Option<String>,
    /// The session's working directory.
    #[serde(default)]
    cwd: Option<String>,
    /// `running`, `idle`, `needsInput`, or something newer.
    #[serde(default)]
    agent_lifecycle: Option<String>,
    /// Whether the session is the live one for its surface.
    #[serde(default)]
    active_for_surface: bool,
    /// Whether the recorded process still exists.
    #[serde(default)]
    stored_pid_exists: bool,
    /// Last update, in Unix seconds.
    #[serde(default)]
    updated_at_unix: f64,
}

/// `read-screen --json`.
#[derive(Debug, Deserialize)]
struct ReadScreen {
    /// The screen text.
    text: String,
}

/// `rpc terminal.paste` reply.
#[derive(Debug, Deserialize)]
struct Paste {
    /// Whether the text and the submit key went in.
    #[serde(default)]
    submitted: bool,
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use rstest::rstest;
    use serde_json::{Value, json};

    use super::*;

    /// The Claude surface of the fixtures.
    const S4: &str = "9368A7E3-DCC1-4510-8DD6-DB8A819770F8";
    /// The shell surface of the fixtures.
    const SHELL: &str = "4198F9DC-3B4A-444D-A428-47C2A9E1A58B";

    /// The `tree_all.json` fixture, parsed.
    fn tree() -> Tree {
        parse_json(include_str!("../tests/fixtures/cmux/tree_all.json"))
            .unwrap()
    }

    /// The sessions of a `sessions*.json` fixture.
    fn sessions(text: &str) -> Vec<SessionJson> {
        parse_json::<Sessions>(text).unwrap().sessions
    }

    /// Sessions from a JSON array built inline.
    fn inline(items: &Value) -> Vec<SessionJson> {
        serde_json::from_value(items.clone()).unwrap()
    }

    /// One inline session on the Claude surface.
    fn session(
        id: &str,
        active: bool,
        pid: bool,
        lifecycle: &str,
        updated: f64,
    ) -> Value {
        json!({
            "agent": "claude",
            "session_id": id,
            "surface_id": S4,
            "cwd": "/private/tmp",
            "agent_lifecycle": lifecycle,
            "active_for_surface": active,
            "stored_pid_exists": pid,
            "updated_at_unix": updated,
        })
    }

    /// Owned argument strings from literals.
    fn v(items: &[&str]) -> Vec<String> {
        owned(items)
    }

    /// The error kind of a failed result, `None` when it succeeded.
    fn failure<T>(result: Result<T>) -> Option<&'static str> {
        result.err().map(|e| e.kind())
    }

    /// The record of the Claude surface for `sessions`.
    fn claude_record(sessions: &[SessionJson]) -> PaneRecord {
        pane_records(&tree(), sessions, None)
            .into_iter()
            .find(|r| r.handle.0 == S4)
            .unwrap()
    }

    mod classify_error {
        use super::*;

        #[rstest]
        #[case::not_found(
            include_str!("../tests/fixtures/cmux/err_not_found.txt"),
            "not_found"
        )]
        #[case::no_socket(
            include_str!("../tests/fixtures/cmux/err_no_socket.txt"),
            "harness_unavailable"
        )]
        #[case::access_denied(
            include_str!("../tests/fixtures/cmux/err_access_denied.txt"),
            "harness_unavailable"
        )]
        #[case::bad_password(
            include_str!("../tests/fixtures/cmux/err_bad_password.txt"),
            "harness_unavailable"
        )]
        #[case::refused("Error: Connection refused\n", "harness_unavailable")]
        #[case::invalid_params(
            include_str!("../tests/fixtures/cmux/err_invalid_params.txt"),
            "unexpected"
        )]
        #[case::unknown_flag(
            include_str!("../tests/fixtures/cmux/err_unknown_flag.txt"),
            "unexpected"
        )]
        fn should_classify_by_stderr(
            #[case] stderr: &str,
            #[case] kind: &str,
        ) {
            assert_eq!(classify_error(stderr).kind(), kind);
        }

        #[test]
        fn should_keep_trimmed_stderr_when_not_found() {
            let err =
                classify_error("Error: not_found: Workspace not found\n");

            assert_eq!(
                err.to_string(),
                "Error: not_found: Workspace not found"
            );
        }
    }

    mod spawn_error {
        use super::*;

        #[rstest]
        #[case::missing(ErrorKind::NotFound, "cmux is not installed")]
        #[case::timeout(ErrorKind::TimedOut, "cmux timed out: ")]
        #[case::other(ErrorKind::PermissionDenied, "running cmux: ")]
        fn should_be_unavailable_when_spawn_fails(
            #[case] kind: ErrorKind,
            #[case] prefix: &str,
        ) {
            let err = spawn_error(&io::Error::new(kind, "x"));

            assert_eq!(err.kind(), "harness_unavailable");
            assert!(err.to_string().starts_with(prefix), "got {err}");
        }
    }

    mod parse_ok_ref {
        use super::*;

        #[test]
        fn should_return_token_when_workspace_created() {
            let out = include_str!("../tests/fixtures/cmux/new_workspace.txt");

            assert_eq!(parse_ok_ref(out, "workspace").unwrap(), "workspace:4");
        }

        #[rstest]
        #[case::garbage("garbage")]
        #[case::wrong_noun("OK surface:4\n")]
        #[case::no_number("OK workspace:\n")]
        fn should_fail_when_shape_is_wrong(#[case] stdout: &str) {
            assert_eq!(
                failure(parse_ok_ref(stdout, "workspace")),
                Some("unexpected")
            );
        }
    }

    mod parse_split_ok {
        use super::*;

        #[test]
        fn should_return_both_refs_when_split() {
            let out = include_str!("../tests/fixtures/cmux/new_split.txt");

            assert_eq!(
                parse_split_ok(out).unwrap(),
                ("surface:8".to_string(), "workspace:4".to_string())
            );
        }

        #[rstest]
        #[case::one_ref("OK surface:8\n")]
        #[case::swapped("OK workspace:4 surface:8\n")]
        #[case::three_refs("OK surface:8 workspace:4 pane:1\n")]
        fn should_fail_when_shape_is_wrong(#[case] stdout: &str) {
            assert_eq!(failure(parse_split_ok(stdout)), Some("unexpected"));
        }
    }

    mod map_lifecycle {
        use super::*;

        #[rstest]
        #[case("running", Status::Working)]
        #[case("idle", Status::Idle)]
        #[case("needsInput", Status::Blocked)]
        #[case("unknown", Status::Unknown)]
        fn should_map_lifecycle(#[case] raw: &str, #[case] expected: Status) {
            assert_eq!(map_lifecycle(raw), expected);
        }
    }

    mod locate {
        use super::*;

        #[test]
        fn should_return_workspace_and_pane_when_surface_exists() {
            assert_eq!(
                locate(&tree(), S4).unwrap(),
                (
                    "3543ECC9-B2A1-4751-9961-1695A3EC1222".to_string(),
                    "C821ED94-AD86-4FBF-9788-EFF3D8ECB8DB".to_string()
                )
            );
        }

        #[test]
        fn should_fail_not_found_when_surface_is_absent() {
            assert_eq!(failure(locate(&tree(), "bogus")), Some("not_found"));
        }
    }

    mod pane_records {
        use super::*;

        /// Records of the main fixtures.
        fn records() -> Vec<PaneRecord> {
            let list =
                sessions(include_str!("../tests/fixtures/cmux/sessions.json"));

            pane_records(&tree(), &list, None)
        }

        #[test]
        fn should_list_every_terminal_surface() {
            assert_eq!(records().len(), 5);
        }

        #[test]
        fn should_join_session_when_agent_present() {
            let claude =
                records().into_iter().find(|r| r.handle.0 == S4).unwrap();

            assert_eq!(
                serde_json::to_value(&claude).unwrap(),
                json!({
                    "handle": S4,
                    "workspace_id": "3543ECC9-B2A1-4751-9961-1695A3EC1222",
                    "workspace_label": "md-test-probe-renamed",
                    "tab_id": "C821ED94-AD86-4FBF-9788-EFF3D8ECB8DB",
                    "tab_label": null,
                    "label": "\u{2733} Pong reply test",
                    "cwd": "/private/tmp",
                    "agent": "claude",
                    "agent_session": {
                        "kind": "id",
                        "value": "27a40df8-8472-4d5e-9e09-4859e42285ff"
                    },
                    "session_confidence": "native",
                    "status": "idle",
                    "status_confidence": "native"
                })
            );
        }

        #[test]
        fn should_leave_agent_fields_empty_when_plain_shell() {
            let shell =
                records().into_iter().find(|r| r.handle.0 == SHELL).unwrap();

            assert_eq!(
                (
                    shell.workspace_label.as_deref(),
                    shell.label.as_deref(),
                    shell.tab_label.as_deref(),
                    shell.cwd.as_deref(),
                    shell.agent.as_deref(),
                    shell.agent_session.is_none(),
                    shell.session_confidence,
                    shell.status,
                    shell.status_confidence,
                ),
                (
                    Some("group-1"),
                    Some("/home/u"),
                    None,
                    None,
                    None,
                    true,
                    Confidence::None,
                    Status::Unknown,
                    Confidence::None,
                )
            );
        }

        #[test]
        fn should_filter_when_workspace_given() {
            let found = pane_records(
                &tree(),
                &[],
                Some("3543ECC9-B2A1-4751-9961-1695A3EC1222"),
            );

            assert_eq!(found.len(), 2);
        }

        #[test]
        fn should_ignore_session_when_not_active_for_surface() {
            let list = sessions(include_str!(
                "../tests/fixtures/cmux/sessions_inactive.json"
            ));

            assert_eq!(claude_record(&list).agent, None);
        }

        #[test]
        fn should_ignore_session_when_pid_is_gone() {
            let list =
                inline(&json!([session("a", true, false, "idle", 1.0)]));

            assert_eq!(claude_record(&list).agent, None);
        }

        #[test]
        fn should_prefer_newer_session_when_two_count() {
            let list = inline(&json!([
                session("new", true, true, "running", 2.0),
                session("old", true, true, "idle", 1.0),
            ]));
            let record = claude_record(&list);

            assert_eq!(
                (record.agent_session.map(|s| s.value), record.status),
                (Some("new".to_string()), Status::Working)
            );
        }

        #[test]
        fn should_prefer_later_entry_when_timestamps_tie() {
            let list = inline(&json!([
                session("first", true, true, "idle", 1.0),
                session("second", true, true, "running", 1.0),
            ]));

            assert_eq!(
                claude_record(&list).agent_session.map(|s| s.value),
                Some("second".to_string())
            );
        }

        #[test]
        fn should_keep_agent_but_no_status_when_lifecycle_unknown() {
            let list =
                inline(&json!([session("a", true, true, "unknown", 1.0)]));
            let record = claude_record(&list);

            assert_eq!(
                (
                    record.agent.as_deref(),
                    record.session_confidence,
                    record.status,
                    record.status_confidence,
                ),
                (
                    Some("claude"),
                    Confidence::Native,
                    Status::Unknown,
                    Confidence::None
                )
            );
        }

        #[test]
        fn should_parse_null_cwd_as_none() {
            let mut item = session("a", true, true, "idle", 1.0);

            item["cwd"] = Value::Null;

            assert_eq!(claude_record(&inline(&json!([item]))).cwd, None);
        }

        #[test]
        fn should_skip_surface_when_browser() {
            let tree: Tree = parse_json(
                r#"{"windows":[{"workspaces":[{"id":"w","panes":[{"id":"p",
                "surfaces":[{"id":"b","type":"browser"},
                {"id":"t","type":"terminal"}]}]}]}]}"#,
            )
            .unwrap();
            let found = pane_records(&tree, &[], None);

            assert_eq!(
                found
                    .iter()
                    .map(|r| r.handle.0.as_str())
                    .collect::<Vec<_>>(),
                ["t"]
            );
        }

        #[test]
        fn should_list_both_windows_when_two_exist() {
            let tree: Tree = parse_json(
                r#"{"windows":[
                {"workspaces":[{"id":"w1","panes":[{"id":"p1",
                "surfaces":[{"id":"s1","type":"terminal"}]}]}]},
                {"workspaces":[{"id":"w2","panes":[{"id":"p2",
                "surfaces":[{"id":"s2","type":"terminal"}]}]}]}]}"#,
            )
            .unwrap();
            let found = pane_records(&tree, &[], None);

            assert_eq!(
                found
                    .iter()
                    .map(|r| r.workspace_id.as_str())
                    .collect::<Vec<_>>(),
                ["w1", "w2"]
            );
        }
    }

    mod resolve_status {
        use super::*;

        /// A patterns set whose idle regex matches a `❯` prompt.
        fn patterns() -> Patterns {
            Patterns::from_json(r#"{"idle":["❯ $"]}"#).unwrap()
        }

        /// The sessions of an agent with an unusable lifecycle.
        fn unknown() -> Vec<SessionJson> {
            inline(&json!([session("a", true, true, "unknown", 1.0)]))
        }

        #[test]
        fn should_be_native_when_session_has_lifecycle() {
            let list = sessions(include_str!(
                "../tests/fixtures/cmux/sessions_needs_input.json"
            ));
            let result = resolve_status(
                &Handle(S4.into()),
                &list,
                Some(&patterns()),
                || Err(Error::Unexpected("must not read".into())),
            )
            .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail),
                (Status::Blocked, Confidence::Native, None)
            );
        }

        #[test]
        fn should_classify_screen_when_lifecycle_unknown_and_patterns() {
            let result = resolve_status(
                &Handle(S4.into()),
                &unknown(),
                Some(&patterns()),
                || Ok("hello\n/tmp ❯ ".into()),
            )
            .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail.is_some()),
                (Status::Idle, Confidence::Heuristic, true)
            );
        }

        #[test]
        fn should_be_unknown_without_reading_when_no_patterns() {
            let read = Cell::new(false);
            let result =
                resolve_status(&Handle(S4.into()), &unknown(), None, || {
                    read.set(true);

                    Ok(String::new())
                })
                .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail, read.get()),
                (Status::Unknown, Confidence::None, None, false)
            );
        }
    }

    mod identify {
        use super::*;

        #[test]
        fn should_return_caller_ids() {
            let caller = parse_identify(include_str!(
                "../tests/fixtures/cmux/identify.json"
            ))
            .unwrap();

            assert_eq!(
                (caller.surface_id, caller.workspace_id),
                (
                    "2013CF4A-A408-4556-94D9-47C90D2B5509".to_string(),
                    "71B1C2DE-315C-4A89-8121-4D6175B6905F".to_string()
                )
            );
        }

        #[test]
        fn should_fail_unexpected_when_no_caller() {
            let result = parse_identify(include_str!(
                "../tests/fixtures/cmux/identify_no_caller.json"
            ));

            assert_eq!(
                result.err().map(|e| (e.kind(), e.to_string())),
                Some((
                    "unexpected",
                    "cmux identify returned no caller".to_string()
                ))
            );
        }
    }

    mod paste {
        use super::*;

        #[test]
        fn should_accept_when_submitted() {
            assert!(
                parse_paste(include_str!(
                    "../tests/fixtures/cmux/terminal_paste.json"
                ))
                .is_ok()
            );
        }

        #[test]
        fn should_fail_unexpected_when_not_submitted() {
            let result = parse_paste(include_str!(
                "../tests/fixtures/cmux/terminal_paste_unsubmitted.json"
            ));

            assert_eq!(failure(result), Some("unexpected"));
        }
    }

    mod workspace_records {
        use super::*;

        #[test]
        fn should_list_every_workspace_with_the_active_one_focused() {
            let records = workspace_records(&tree());

            assert_eq!(
                records.iter().map(|r| r.focused).collect::<Vec<_>>(),
                [false, true, false, false]
            );
            assert_eq!(records[1].label, "~/proj/a");
            assert!(records[1].tags.is_empty());
        }
    }

    mod paste_params {
        use super::*;

        #[test]
        fn should_escape_backslash_and_quote() {
            let text = r#"say \n "hi" it's"#;

            assert_eq!(
                serde_json::from_str::<Value>(&paste_params("S", text))
                    .unwrap(),
                json!({
                    "submit_key": "return",
                    "surface_id": "S",
                    "text": text,
                })
            );
        }
    }

    mod read_text {
        use super::*;

        #[test]
        fn should_trim_trailing_blank_lines() {
            assert_eq!(
                read_text(include_str!(
                    "../tests/fixtures/cmux/read_screen.json"
                ))
                .unwrap(),
                "\n/tmp ✗ echo hello-$((40+2))\nhello-42\n\n/tmp ❯ "
            );
        }
    }

    mod split_line {
        use super::*;

        #[test]
        fn should_change_directory_before_the_command() {
            assert_eq!(
                split_line(Path::new("/tmp"), &v(&["bash", "-c", "echo hi"])),
                "cd /tmp && bash -c 'echo hi'"
            );
        }
    }

    mod args {
        use super::*;

        #[rstest]
        #[case::plain(None, &[
            "new-workspace", "--name", "n", "--cwd", "/tmp", "--focus",
            "false"
        ])]
        #[case::command(Some("claude -p x"), &[
            "new-workspace", "--name", "n", "--cwd", "/tmp", "--command",
            "claude -p x", "--focus", "false"
        ])]
        fn should_build_new_workspace(
            #[case] command: Option<&str>,
            #[case] expected: &[&str],
        ) {
            assert_eq!(
                new_workspace_args("n", Path::new("/tmp"), command),
                v(expected)
            );
        }

        #[rstest]
        #[case::right(Direction::Right, "right")]
        #[case::down(Direction::Down, "down")]
        fn should_build_new_split(
            #[case] direction: Direction,
            #[case] word: &str,
        ) {
            assert_eq!(
                new_split_args(direction, "S"),
                v(&["new-split", word, "--surface", "S", "--focus", "false"])
            );
        }

        #[test]
        fn should_build_identify() {
            assert_eq!(
                identify_args("workspace:4", "surface:8"),
                v(&[
                    "identify",
                    "--workspace",
                    "workspace:4",
                    "--surface",
                    "surface:8",
                    "--json",
                    "--id-format",
                    "uuids"
                ])
            );
        }

        #[test]
        fn should_end_options_before_label_in_rename_tab() {
            assert_eq!(
                rename_tab_args("W", "S", "-x"),
                v(&[
                    "rename-tab",
                    "--workspace",
                    "W",
                    "--surface",
                    "S",
                    "--",
                    "-x"
                ])
            );
        }

        #[test]
        fn should_build_close_surface() {
            assert_eq!(
                close_surface_args("W", "S"),
                v(&["close-surface", "--workspace", "W", "--surface", "S"])
            );
        }

        #[test]
        fn should_build_read_screen() {
            assert_eq!(
                read_screen_args("S", 15),
                v(&[
                    "read-screen",
                    "--surface",
                    "S",
                    "--lines",
                    "15",
                    "--json"
                ])
            );
        }

        #[rstest]
        #[case::all(None, &["sessions", "--json", "--all"])]
        #[case::one(Some("S"), &["sessions", "--json", "--surface", "S"])]
        fn should_build_sessions(
            #[case] surface: Option<&str>,
            #[case] expected: &[&str],
        ) {
            assert_eq!(sessions_args(surface), v(expected));
        }
    }
}
