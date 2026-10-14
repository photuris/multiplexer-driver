//! tuios driver. Handles are window UUIDs; workspaces are the tuios
//! workspace numbers (1-9) of one session. Every argv here was verified
//! against a private tuios daemon (`tuios version dev`).

use std::{
    collections::BTreeMap,
    env,
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

/// Fragment of tuios's refusal to find a window.
const NO_WINDOW: &str = "no window found matching";

/// Fragment of tuios's message when no daemon answers the socket.
const NO_DAEMON: &str = "daemon is not running";

/// Fragment of tuios's message when the named session does not exist.
const NO_SESSION: &str = "matches no live session";

/// Drives one tuios daemon through its CLI.
#[derive(Debug)]
pub struct Tuios {
    /// Session name passed as `-s`; `None` lets tuios pick the most
    /// recently active session.
    session: Option<String>,
    /// Status patterns for panes that report no agent state.
    patterns: Option<Patterns>,
}

impl Tuios {
    /// Creates a driver for the tuios session `session`, else the one in
    /// `TUIOS_SESSION`, else tuios's own choice, with optional fallback
    /// status `patterns`. The daemon is chosen by `XDG_RUNTIME_DIR`,
    /// which the driver never sets.
    pub fn new(session: Option<String>, patterns: Option<Patterns>) -> Self {
        Self {
            session: pick_session(session, env::var("TUIOS_SESSION").ok()),
            patterns,
        }
    }

    /// Runs `tuios args…` exactly as given under the ceiling and returns
    /// the raw output.
    fn execute(args: &[String]) -> Result<Output> {
        let mut command = Command::new("tuios");

        command.args(args);

        output_within(&mut command, CEILING).map_err(|e| spawn_error(&e))
    }

    /// Runs `tuios args…` with `-s <session>` after the verb and returns
    /// stdout; a failing exit is classified from its message.
    fn run(&self, args: &[String]) -> Result<String> {
        let output =
            Self::execute(&with_session(args, self.session.as_deref()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            return Err(classify_error(&failure_text(&stdout, &stderr)));
        }

        Ok(stdout.into_owned())
    }

    /// Runs `tuios args…` and parses its JSON stdout as `T`.
    fn run_json<T: DeserializeOwned>(&self, args: &[String]) -> Result<T> {
        parse_json(&self.run(args)?)
    }

    /// Types `command` as one shell line into the window `handle` and
    /// presses Enter. An empty command types nothing: the shell stays.
    fn type_command(&self, handle: &str, command: &[String]) -> Result<()> {
        if command.is_empty() {
            return Ok(());
        }

        self.run(&send_text_args(handle, &shell_join(command)))?;
        self.run(&send_enter_args(handle))?;

        Ok(())
    }
}

impl Driver for Tuios {
    fn name(&self) -> &'static str {
        "tuios"
    }

    fn workspace_list(&self) -> Result<Vec<WorkspaceRecord>> {
        let workspaces: Workspaces = self.run_json(&list_workspaces_args())?;

        Ok(workspace_records(&workspaces.workspaces))
    }

    /// The nine workspaces are fixed slots, so none can be created.
    fn workspace_create(
        &self,
        _label: &str,
        _cwd: &Path,
    ) -> Result<WorkspaceRecord> {
        Err(unsupported("tuios", "workspace create"))
    }

    fn workspace_close(&self, id: &str) -> Result<()> {
        self.run(&close_workspace_args(id))?;

        Ok(())
    }

    /// Workspaces carry no tags in tuios.
    fn workspace_tag(
        &self,
        _id: &str,
        _set: &[(String, String)],
        _clear: &[String],
    ) -> Result<BTreeMap<String, String>> {
        Err(unsupported("tuios", "workspace tag"))
    }

    fn pane_list(&self, workspace: Option<&str>) -> Result<Vec<PaneRecord>> {
        let windows: Windows = self.run_json(&list_windows_args())?;
        let workspaces: Workspaces = self.run_json(&list_workspaces_args())?;
        let agents: Agents = self.run_json(&list_agents_args())?;

        Ok(pane_records(
            &windows.windows,
            &workspaces.workspaces,
            &agents.agents,
            workspace,
        ))
    }

    fn pane_spawn(&self, request: &SpawnRequest<'_>) -> Result<Handle> {
        if request.workspace.is_some() {
            return Err(Error::Usage(
                "tuios spawn opens the window in the current workspace; \
                 --workspace is not accepted"
                    .into(),
            ));
        }

        let out = self.run(&new_window_args(request.name, request.cwd))?;
        let handle = parse_print_id(&out)?;
        let typed = self.type_command(&handle, request.command);

        started(handle, typed)
    }

    /// The request's cwd is ignored: the split inherits the target's
    /// directory. tuios refuses a split without an attached client or
    /// with tiling off; its refusal is passed through as
    /// [`Error::Unsupported`], and tiling is never enabled here.
    fn pane_split(&self, request: &SplitRequest<'_>) -> Result<Handle> {
        let out = self
            .run(&split_window_args(
                request.direction,
                &request.target.0,
                request.name,
            ))
            .map_err(split_error)?;
        let created: Created = parse_json(&out)?;
        let typed = self.type_command(&created.window_id, request.command);

        started(created.window_id, typed)
    }

    fn pane_read(
        &self,
        target: &Handle,
        lines: u32,
        ansi: bool,
    ) -> Result<String> {
        read_text(&self.run(&capture_args(&target.0, lines, ansi))?)
    }

    fn pane_prompt(&self, target: &Handle, text: &str) -> Result<()> {
        self.run(&send_text_args(&target.0, text))?;
        self.run(&send_enter_args(&target.0))?;

        Ok(())
    }

    fn pane_rename(&self, target: &Handle, label: &str) -> Result<()> {
        self.run(&rename_args(&target.0, label))?;

        Ok(())
    }

    fn pane_interrupt(&self, target: &Handle) -> Result<()> {
        self.run(&interrupt_args(&target.0))?;

        Ok(())
    }

    fn pane_kill(&self, target: &Handle) -> Result<()> {
        self.run(&close_window_args(&target.0))?;

        Ok(())
    }

    fn pane_status(&self, target: &Handle) -> Result<StatusResult> {
        let state: AgentState = self.run_json(&agent_state_args(&target.0))?;

        resolve_status(target, &state.state, self.patterns.as_ref(), || {
            read_text(&self.run(&capture_args(&target.0, 15, false))?)
        })
    }

    /// Runs `tuios <args>` verbatim. The session is not injected,
    /// because the verbs differ in the flags they take.
    fn raw(&self, args: &[String]) -> Result<String> {
        let output = Self::execute(args)?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let text = failure_text(&stdout, &stderr);

            return Err(match classify_error(&text) {
                err @ Error::HarnessUnavailable(_) => err,
                _ => Error::Unexpected(text),
            });
        }

        Ok(stdout.into_owned())
    }
}

/// The session to use: `explicit` when non-empty, else `ambient` (the
/// `TUIOS_SESSION` value) when non-empty, else none.
fn pick_session(
    explicit: Option<String>,
    ambient: Option<String>,
) -> Option<String> {
    explicit
        .filter(|s| !s.is_empty())
        .or_else(|| ambient.filter(|s| !s.is_empty()))
}

/// Maps a subprocess failure to an [`Error`]: a missing binary, a call
/// past the ceiling, and any other spawn failure are all
/// [`Error::HarnessUnavailable`].
fn spawn_error(e: &io::Error) -> Error {
    match e.kind() {
        ErrorKind::NotFound => {
            Error::HarnessUnavailable("tuios is not installed".into())
        }
        ErrorKind::TimedOut => {
            Error::HarnessUnavailable(format!("tuios timed out: {e}"))
        }
        _ => Error::HarnessUnavailable(format!("running tuios: {e}")),
    }
}

/// The error message of a failed tuios call. Plain verbs write it to
/// stderr; verbs run with `--json` write `{"error":…,"success":false}`
/// to stdout and leave stderr empty. Whichever has text wins, stderr
/// first.
fn failure_text(stdout: &str, stderr: &str) -> String {
    let stderr = stderr.trim();

    if !stderr.is_empty() {
        return stderr.to_string();
    }

    match serde_json::from_str::<ErrorJson>(stdout) {
        Ok(json) => json.error,
        Err(_) => stdout.trim().to_string(),
    }
}

/// Turns tuios's error text into an [`Error`]: a missing window is
/// [`Error::NotFound`], a daemon that is not running or a session that
/// does not exist is [`Error::HarnessUnavailable`], and anything else is
/// [`Error::Unexpected`].
fn classify_error(text: &str) -> Error {
    let text = text.trim();

    if text.contains(NO_WINDOW) {
        return Error::NotFound(text.to_string());
    }

    if text.contains(NO_DAEMON) || text.contains(NO_SESSION) {
        return Error::HarnessUnavailable(text.to_string());
    }

    Error::Unexpected(text.to_string())
}

/// Reports a `split-window` failure that is not a missing window or an
/// unreachable daemon as [`Error::Unsupported`] with tuios's own text:
/// tuios refuses a split without an attached client or with tiling off.
fn split_error(err: Error) -> Error {
    match err {
        Error::Unexpected(message) => Error::Unsupported(message),
        other => other,
    }
}

/// Finishes a start whose window already exists: a failed `result` is an
/// [`Error::StartFailed`] that carries the window.
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

/// Maps a tuios agent state onto our status; `none`, `unknown`, and
/// anything newer have no native status.
fn map_state(raw: &str) -> Status {
    match raw {
        "working" => Status::Working,
        "needs_input" | "errored" => Status::Blocked,
        "idle" | "done" => Status::Idle,
        _ => Status::Unknown,
    }
}

/// Picks the status for `target`. A mappable agent `state` is native.
/// Otherwise, with `patterns`, `read` supplies the screen text to
/// classify; without them, nothing is read.
fn resolve_status(
    target: &Handle,
    state: &str,
    patterns: Option<&Patterns>,
    read: impl FnOnce() -> Result<String>,
) -> Result<StatusResult> {
    let status = map_state(state);

    if status != Status::Unknown {
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

/// `value` as `Some`, or `None` when it is empty.
fn non_empty(value: &str) -> Option<String> {
    Some(value).filter(|v| !v.is_empty()).map(str::to_string)
}

/// One record per workspace that holds windows, plus the current one.
fn workspace_records(workspaces: &[WorkspaceJson]) -> Vec<WorkspaceRecord> {
    workspaces
        .iter()
        .filter(|ws| ws.window_count > 0 || ws.current)
        .map(WorkspaceRecord::from)
        .collect()
}

/// One record per window, in tuios order, limited to `workspace` (a
/// workspace number as text) when given and joined with the agents.
fn pane_records(
    windows: &[WindowJson],
    workspaces: &[WorkspaceJson],
    agents: &[AgentJson],
    workspace: Option<&str>,
) -> Vec<PaneRecord> {
    windows
        .iter()
        .filter(|w| workspace.is_none_or(|id| id == w.workspace.to_string()))
        .map(|w| {
            let agent = agents.iter().find(|a| a.window_id == w.window_id);

            pane_record(w, workspaces, agent)
        })
        .collect()
}

/// Builds the record of one window. Its status comes from the window's
/// own `agent_state`; its agent kind and conversation from `agent`.
fn pane_record(
    window: &WindowJson,
    workspaces: &[WorkspaceJson],
    agent: Option<&AgentJson>,
) -> PaneRecord {
    let status = map_state(&window.agent_state);
    let status_confidence = match status {
        Status::Unknown => Confidence::None,
        _ => Confidence::Native,
    };
    let agent_session = agent.and_then(|a| {
        non_empty(&a.agent_session_id).map(|value| AgentSession {
            kind: SessionRefKind::Id,
            value,
        })
    });
    let workspace_label = workspaces
        .iter()
        .find(|ws| ws.workspace == window.workspace)
        .and_then(|ws| non_empty(&ws.name));

    PaneRecord {
        handle: Handle(window.window_id.clone()),
        workspace_id: window.workspace.to_string(),
        workspace_label,
        tab_id: window.window_id.clone(),
        tab_label: non_empty(&window.display_name),
        label: window.custom_name.as_deref().and_then(non_empty),
        cwd: window.cwd.as_deref().and_then(non_empty),
        agent: agent.and_then(|a| non_empty(&a.harness_id)),
        session_confidence: if agent_session.is_some() {
            Confidence::Native
        } else {
            Confidence::None
        },
        agent_session,
        status,
        status_confidence,
    }
}

/// Parses `new-window --print-id` output: the bare window UUID.
fn parse_print_id(stdout: &str) -> Result<String> {
    let id = stdout.trim();

    if id.is_empty() || id.contains(char::is_whitespace) {
        return Err(Error::Unexpected(format!(
            "unexpected tuios output: {id:?}"
        )));
    }

    Ok(id.to_string())
}

/// Parses tuios JSON output as `T`.
fn parse_json<T: DeserializeOwned>(stdout: &str) -> Result<T> {
    serde_json::from_str(stdout)
        .map_err(|e| Error::Unexpected(format!("parsing tuios output: {e}")))
}

/// The screen text of a `capture-pane --json` reply, trailing blank
/// lines trimmed.
fn read_text(stdout: &str) -> Result<String> {
    let capture: Capture = parse_json(stdout)?;

    Ok(trim_trailing_blank_lines(&capture.content))
}

/// Inserts `-s <session>` right after the verb, the one place every
/// tuios verb accepts it. Without a session, `args` is returned as is.
fn with_session(args: &[String], session: Option<&str>) -> Vec<String> {
    match (args.split_first(), session) {
        (Some((verb, rest)), Some(name)) => {
            let mut out =
                vec![verb.clone(), "-s".to_string(), name.to_string()];

            out.extend_from_slice(rest);

            out
        }
        _ => args.to_vec(),
    }
}

/// Converts string slices to owned argument strings.
fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

/// Arguments for the window list.
fn list_windows_args() -> Vec<String> {
    owned(&["list-windows", "--json"])
}

/// Arguments for the workspace list.
fn list_workspaces_args() -> Vec<String> {
    owned(&["list-workspaces", "--json"])
}

/// Arguments for the agent list.
fn list_agents_args() -> Vec<String> {
    owned(&["list-agents", "--json"])
}

/// Arguments for `new-window`: the window starts in `cwd`, keeps the
/// focus where it is, and prints its full id. `--` keeps a name that
/// starts with a dash from being read as a flag.
fn new_window_args(name: &str, cwd: &Path) -> Vec<String> {
    let mut args = owned(&["new-window", "--cwd"]);

    args.push(cwd.to_string_lossy().into_owned());
    args.extend(owned(&["--no-focus", "--print-id", "--", name]));

    args
}

/// Arguments for `split-window` of `target`: a vertical split puts the
/// new window to the right, a horizontal one below.
fn split_window_args(
    direction: Direction,
    target: &str,
    name: &str,
) -> Vec<String> {
    let axis = match direction {
        Direction::Right => "vertical",
        Direction::Down => "horizontal",
    };

    owned(&["split-window", axis, "-w", target, "--name", name, "--json"])
}

/// Arguments for `capture-pane` of the last `lines` lines including the
/// scrollback; without `--scrollback` tuios reads only the visible
/// screen.
fn capture_args(target: &str, lines: u32, ansi: bool) -> Vec<String> {
    let mut args =
        owned(&["capture-pane", "-w", target, "--json", "--scrollback"]);

    if ansi {
        args.push("--ansi".into());
    }

    args.push("--lines".into());
    args.push(lines.to_string());

    args
}

/// Arguments for `send-text`: `text` goes to the PTY verbatim, and `--`
/// keeps text that starts with a dash from being read as a flag.
fn send_text_args(target: &str, text: &str) -> Vec<String> {
    owned(&["send-text", "-w", target, "--", text])
}

/// Arguments that press Enter in `target`.
fn send_enter_args(target: &str) -> Vec<String> {
    owned(&["send-keys", "-w", target, "Enter"])
}

/// Arguments that press Ctrl-C in `target`.
fn interrupt_args(target: &str) -> Vec<String> {
    owned(&["send-keys", "-w", target, "ctrl+c"])
}

/// Arguments for `set-window`: the label becomes the custom name.
fn rename_args(target: &str, label: &str) -> Vec<String> {
    owned(&["set-window", "-w", target, "--name", label])
}

/// Arguments for `close-window`; the window is a positional id.
fn close_window_args(target: &str) -> Vec<String> {
    owned(&["close-window", target])
}

/// Arguments for `close-workspace`.
fn close_workspace_args(workspace: &str) -> Vec<String> {
    owned(&["close-workspace", workspace])
}

/// Arguments for `get-agent-state`.
fn agent_state_args(target: &str) -> Vec<String> {
    owned(&["get-agent-state", "-w", target, "--json"])
}

/// `{"error":…,"success":false}`, what a failed `--json` verb prints.
#[derive(Debug, Deserialize)]
struct ErrorJson {
    /// The error text.
    error: String,
}

/// `list-windows --json`: only the windows are read.
#[derive(Debug, Deserialize)]
struct Windows {
    /// Every window of the session, on every workspace.
    #[serde(default)]
    windows: Vec<WindowJson>,
}

/// One window of `list-windows`.
#[derive(Debug, Deserialize)]
struct WindowJson {
    /// Window UUID.
    window_id: String,
    /// Workspace number (1-9) the window is on.
    workspace: u32,
    /// The name shown for the window: the custom name, else the title.
    #[serde(default)]
    display_name: String,
    /// The name set through `new-window` or `set-window`, when any.
    #[serde(default)]
    custom_name: Option<String>,
    /// The window's working directory.
    #[serde(default)]
    cwd: Option<String>,
    /// `none`, `working`, `needs_input`, `idle`, `done`, `errored`, or
    /// something newer.
    #[serde(default)]
    agent_state: String,
}

/// `list-workspaces --json`: only the workspaces are read.
#[derive(Debug, Deserialize)]
struct Workspaces {
    /// All nine workspaces.
    #[serde(default)]
    workspaces: Vec<WorkspaceJson>,
}

/// One workspace slot.
#[derive(Debug, Deserialize)]
struct WorkspaceJson {
    /// Workspace number (1-9).
    workspace: u32,
    /// Its name; empty when unnamed.
    #[serde(default)]
    name: String,
    /// How many windows it holds.
    #[serde(default)]
    window_count: u32,
    /// Whether it is the one on screen.
    #[serde(default)]
    current: bool,
}

impl From<&WorkspaceJson> for WorkspaceRecord {
    /// Maps the number to the ID, the name (else the number) to the
    /// label, and `current` to `focused`.
    fn from(ws: &WorkspaceJson) -> Self {
        let id = ws.workspace.to_string();

        Self {
            label: non_empty(&ws.name).unwrap_or_else(|| id.clone()),
            workspace_id: id,
            focused: ws.current,
            tags: BTreeMap::new(),
        }
    }
}

/// `list-agents --json`: only the agents are read.
#[derive(Debug, Deserialize)]
struct Agents {
    /// The panes something identified as an agent.
    #[serde(default)]
    agents: Vec<AgentJson>,
}

/// One agent pane of `list-agents`.
#[derive(Debug, Deserialize)]
struct AgentJson {
    /// UUID of the window the agent runs in.
    window_id: String,
    /// The agent's harness, e.g. `claude-code`; empty when unknown.
    #[serde(default)]
    harness_id: String,
    /// The harness's own conversation ID; empty when unknown.
    #[serde(default)]
    agent_session_id: String,
}

/// `get-agent-state --json`: only the state is read.
#[derive(Debug, Deserialize)]
struct AgentState {
    /// `none`, `working`, `needs_input`, `idle`, `done`, `errored`, or
    /// something newer.
    state: String,
}

/// `split-window --json`: only the new window is read.
#[derive(Debug, Deserialize)]
struct Created {
    /// UUID of the new window.
    window_id: String,
}

/// `capture-pane --json`: only the text is read.
#[derive(Debug, Deserialize)]
struct Capture {
    /// The captured text.
    content: String,
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// The window the fixtures put an agent in.
    const AGENT_WINDOW: &str = "b6f04787-b9c6-4a4b-ae3e-953d81ef5938";

    /// The `list_windows.json` fixture, parsed.
    fn windows() -> Vec<WindowJson> {
        parse_json::<Windows>(include_str!(
            "../tests/fixtures/tuios/list_windows.json"
        ))
        .unwrap()
        .windows
    }

    /// The `list_windows_agent.json` fixture, parsed: the same windows
    /// with an agent state on the agent window.
    fn agent_windows() -> Vec<WindowJson> {
        parse_json::<Windows>(include_str!(
            "../tests/fixtures/tuios/list_windows_agent.json"
        ))
        .unwrap()
        .windows
    }

    /// The `list_workspaces.json` fixture, parsed.
    fn workspaces() -> Vec<WorkspaceJson> {
        parse_json::<Workspaces>(include_str!(
            "../tests/fixtures/tuios/list_workspaces.json"
        ))
        .unwrap()
        .workspaces
    }

    /// The `list_agents.json` fixture, parsed.
    fn agents() -> Vec<AgentJson> {
        parse_json::<Agents>(include_str!(
            "../tests/fixtures/tuios/list_agents.json"
        ))
        .unwrap()
        .agents
    }

    /// Owned argument strings from literals.
    fn v(items: &[&str]) -> Vec<String> {
        owned(items)
    }

    /// The error kind of a failed result, `None` when it succeeded.
    fn failure<T>(result: Result<T>) -> Option<&'static str> {
        result.err().map(|e| e.kind())
    }

    /// The `get-agent-state` fixture of `state`.
    fn state_fixture(state: &str) -> AgentState {
        let text = match state {
            "working" => include_str!(
                "../tests/fixtures/tuios/agent_state_working.json"
            ),
            "needs_input" => include_str!(
                "../tests/fixtures/tuios/agent_state_needs_input.json"
            ),
            "idle" => {
                include_str!("../tests/fixtures/tuios/agent_state_idle.json")
            }
            "done" => {
                include_str!("../tests/fixtures/tuios/agent_state_done.json")
            }
            "errored" => include_str!(
                "../tests/fixtures/tuios/agent_state_errored.json"
            ),
            _ => include_str!("../tests/fixtures/tuios/agent_state_none.json"),
        };

        parse_json(text).unwrap()
    }

    mod pick_session {
        use super::*;

        #[rstest]
        #[case::explicit(Some("a"), Some("b"), Some("a"))]
        #[case::ambient(None, Some("b"), Some("b"))]
        #[case::empty_explicit(Some(""), Some("b"), Some("b"))]
        #[case::empty_ambient(None, Some(""), None)]
        #[case::neither(None, None, None)]
        fn should_prefer_explicit_session_when_both_given(
            #[case] explicit: Option<&str>,
            #[case] ambient: Option<&str>,
            #[case] expected: Option<&str>,
        ) {
            assert_eq!(
                pick_session(
                    explicit.map(str::to_string),
                    ambient.map(str::to_string)
                ),
                expected.map(str::to_string)
            );
        }
    }

    mod classify_error {
        use super::*;

        #[rstest]
        #[case::window_plain(
            include_str!("../tests/fixtures/tuios/err_window_plain.txt"),
            "not_found"
        )]
        #[case::window_json(
            &failure_text(
                include_str!("../tests/fixtures/tuios/err_window_json.json"),
                ""
            ),
            "not_found"
        )]
        #[case::no_daemon(
            include_str!("../tests/fixtures/tuios/err_no_daemon_plain.txt"),
            "harness_unavailable"
        )]
        #[case::no_session_plain(
            include_str!("../tests/fixtures/tuios/err_no_session_plain.txt"),
            "harness_unavailable"
        )]
        #[case::no_session_json(
            &failure_text(
                include_str!("../tests/fixtures/tuios/err_no_session_json.json"),
                ""
            ),
            "harness_unavailable"
        )]
        #[case::refusal(
            &failure_text(
                include_str!("../tests/fixtures/tuios/err_split_headless.json"),
                ""
            ),
            "unexpected"
        )]
        fn should_classify_as_expected_when_text_given(
            #[case] text: &str,
            #[case] kind: &str,
        ) {
            assert_eq!(classify_error(text).kind(), kind);
        }

        #[test]
        fn should_keep_trimmed_text_when_window_missing() {
            let err = classify_error("no window found matching \"x\".\n");

            assert_eq!(err.to_string(), "no window found matching \"x\".");
        }
    }

    mod failure_text {
        use super::*;

        #[test]
        fn should_use_stderr_when_plain_verb_fails() {
            let stderr =
                include_str!("../tests/fixtures/tuios/err_window_plain.txt");

            assert_eq!(failure_text("", stderr), stderr.trim());
        }

        #[test]
        fn should_use_error_field_when_json_verb_fails() {
            let text = failure_text(
                include_str!(
                    "../tests/fixtures/tuios/err_split_headless.json"
                ),
                "",
            );

            assert!(
                text.starts_with("split-window failed: FocusWindow changes"),
                "got {text:?}"
            );
        }

        #[test]
        fn should_fall_back_to_stdout_when_it_is_not_json() {
            assert_eq!(failure_text("boom\n", ""), "boom");
        }

        #[test]
        fn should_prefer_stderr_when_both_have_text() {
            assert_eq!(
                failure_text(r#"{"error":"from stdout"}"#, "from stderr\n"),
                "from stderr"
            );
        }
    }

    mod spawn_error {
        use super::*;

        #[rstest]
        #[case::missing(ErrorKind::NotFound, "tuios is not installed")]
        #[case::timeout(ErrorKind::TimedOut, "tuios timed out: ")]
        #[case::other(ErrorKind::PermissionDenied, "running tuios: ")]
        fn should_be_unavailable_when_spawn_fails(
            #[case] kind: ErrorKind,
            #[case] prefix: &str,
        ) {
            let err = spawn_error(&io::Error::new(kind, "x"));

            assert_eq!(err.kind(), "harness_unavailable");
            assert!(err.to_string().starts_with(prefix), "got {err}");
        }
    }

    mod split_error {
        use super::*;

        #[test]
        fn should_pass_tuios_text_as_unsupported_when_split_refused() {
            let err = split_error(classify_error(&failure_text(
                include_str!(
                    "../tests/fixtures/tuios/err_split_no_tiling.json"
                ),
                "",
            )));

            assert_eq!(
                (
                    err.kind(),
                    err.to_string().contains("tiling is not enabled")
                ),
                ("unsupported", true)
            );
        }

        #[rstest]
        #[case::not_found(Error::NotFound("w".into()), "not_found")]
        #[case::unavailable(
            Error::HarnessUnavailable("d".into()),
            "harness_unavailable"
        )]
        fn should_keep_error_when_not_a_refusal(
            #[case] err: Error,
            #[case] kind: &str,
        ) {
            assert_eq!(split_error(err).kind(), kind);
        }
    }

    mod started {
        use super::*;

        #[test]
        fn should_carry_handle_when_typing_failed() {
            let err = started("h".into(), Err(Error::Unexpected("x".into())))
                .unwrap_err();

            assert!(
                matches!(
                    &err,
                    Error::StartFailed { handle, message }
                        if handle.0 == "h" && message == "x"
                ),
                "got {err:?}"
            );
        }

        #[test]
        fn should_return_handle_when_typing_worked() {
            assert_eq!(started("h".into(), Ok(())).unwrap().0, "h");
        }
    }

    mod map_state {
        use super::*;

        #[rstest]
        #[case("working", Status::Working)]
        #[case("needs_input", Status::Blocked)]
        #[case("errored", Status::Blocked)]
        #[case("idle", Status::Idle)]
        #[case("done", Status::Idle)]
        #[case("none", Status::Unknown)]
        #[case("unknown", Status::Unknown)]
        #[case("newer", Status::Unknown)]
        fn should_map_status_when_state_given(
            #[case] raw: &str,
            #[case] expected: Status,
        ) {
            assert_eq!(map_state(raw), expected);
        }

        #[rstest]
        #[case("working", Status::Working)]
        #[case("needs_input", Status::Blocked)]
        #[case("idle", Status::Idle)]
        #[case("done", Status::Idle)]
        #[case("errored", Status::Blocked)]
        #[case("none", Status::Unknown)]
        fn should_map_status_when_state_comes_from_daemon(
            #[case] state: &str,
            #[case] expected: Status,
        ) {
            assert_eq!(map_state(&state_fixture(state).state), expected);
        }
    }

    mod resolve_status {
        use super::*;

        /// A patterns set whose idle regex matches a `❯` prompt.
        fn patterns() -> Patterns {
            Patterns::from_json(r#"{"idle":["❯ $"]}"#).unwrap()
        }

        /// A handle for the agent window.
        fn target() -> Handle {
            Handle(AGENT_WINDOW.into())
        }

        #[test]
        fn should_be_native_when_agent_reported_state() {
            let result = resolve_status(
                &target(),
                "needs_input",
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
        fn should_classify_screen_when_state_none_and_patterns() {
            let result =
                resolve_status(&target(), "none", Some(&patterns()), || {
                    Ok("hello\n/tmp ❯ ".into())
                })
                .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail.is_some()),
                (Status::Idle, Confidence::Heuristic, true)
            );
        }

        #[test]
        fn should_be_unknown_without_reading_when_no_patterns() {
            let read = Cell::new(false);
            let result = resolve_status(&target(), "none", None, || {
                read.set(true);

                Ok(String::new())
            })
            .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail, read.get()),
                (Status::Unknown, Confidence::None, None, false)
            );
        }

        #[test]
        fn should_classify_screen_when_state_unknown_and_patterns() {
            let result = resolve_status(
                &target(),
                "unknown",
                Some(&patterns()),
                || Ok("/tmp ❯ ".into()),
            )
            .unwrap();

            assert_eq!(result.confidence, Confidence::Heuristic);
        }
    }

    mod workspace_records {
        use super::*;

        #[test]
        fn should_list_only_used_and_current_workspaces() {
            let records = workspace_records(&workspaces());

            assert_eq!(
                records
                    .iter()
                    .map(|r| (r.workspace_id.as_str(), r.focused))
                    .collect::<Vec<_>>(),
                [("1", true)]
            );
        }

        #[test]
        fn should_label_with_number_when_workspace_unnamed() {
            assert_eq!(workspace_records(&workspaces())[0].label, "1");
        }

        #[test]
        fn should_use_name_and_keep_tags_empty_when_named() {
            let named: Vec<WorkspaceJson> = serde_json::from_value(json!([
                {"workspace": 3, "name": "build", "window_count": 2,
                 "current": false}
            ]))
            .unwrap();
            let records = workspace_records(&named);

            assert_eq!(
                (records[0].label.as_str(), records[0].tags.is_empty()),
                ("build", true)
            );
        }

        #[test]
        fn should_include_current_workspace_when_empty() {
            let empty: Vec<WorkspaceJson> = serde_json::from_value(json!([
                {"workspace": 4, "name": "", "window_count": 0,
                 "current": true},
                {"workspace": 5, "name": "", "window_count": 0,
                 "current": false}
            ]))
            .unwrap();

            assert_eq!(workspace_records(&empty).len(), 1);
        }

        #[test]
        fn should_include_used_workspace_when_not_current() {
            let used: Vec<WorkspaceJson> = serde_json::from_value(json!([
                {"workspace": 2, "name": "", "window_count": 1,
                 "current": false}
            ]))
            .unwrap();

            assert_eq!(workspace_records(&used).len(), 1);
        }
    }

    mod pane_records {
        use super::*;

        /// Records of the agent fixtures.
        fn records() -> Vec<PaneRecord> {
            pane_records(&agent_windows(), &workspaces(), &agents(), None)
        }

        #[test]
        fn should_list_every_window_when_fixtures_parse() {
            assert_eq!(records().len(), 4);
        }

        #[test]
        fn should_join_agent_when_window_runs_one() {
            let record = records()
                .into_iter()
                .find(|r| r.handle.0 == AGENT_WINDOW)
                .unwrap();

            assert_eq!(
                serde_json::to_value(&record).unwrap(),
                json!({
                    "handle": AGENT_WINDOW,
                    "workspace_id": "1",
                    "workspace_label": null,
                    "tab_id": AGENT_WINDOW,
                    "tab_label": "renamed",
                    "label": "renamed",
                    "cwd": "/tmp",
                    "agent": "claude-code",
                    "agent_session": {"kind": "id", "value": "abc123"},
                    "session_confidence": "native",
                    "status": "blocked",
                    "status_confidence": "native"
                })
            );
        }

        #[test]
        fn should_leave_agent_fields_empty_when_plain_shell() {
            let shell = pane_records(&windows(), &workspaces(), &[], None)
                .into_iter()
                .find(|r| r.label.as_deref() == Some("w2"))
                .unwrap();

            assert_eq!(
                (
                    shell.agent.as_deref(),
                    shell.agent_session.is_none(),
                    shell.session_confidence,
                    shell.status,
                    shell.status_confidence,
                ),
                (
                    None,
                    true,
                    Confidence::None,
                    Status::Unknown,
                    Confidence::None
                )
            );
        }

        #[test]
        fn should_leave_label_empty_when_window_has_no_custom_name() {
            let first = &records()[0];

            assert_eq!(
                (first.label.as_deref(), first.tab_label.is_some()),
                (None, true)
            );
        }

        #[test]
        fn should_filter_when_workspace_given() {
            let found =
                pane_records(&windows(), &workspaces(), &[], Some("2"));

            assert_eq!(found.len(), 1);
        }

        #[test]
        fn should_keep_every_window_of_workspace_when_filtering() {
            let found =
                pane_records(&windows(), &workspaces(), &[], Some("1"));

            assert_eq!(found.len(), 3);
        }

        #[test]
        fn should_label_workspace_when_it_is_named() {
            let named: Vec<WorkspaceJson> = serde_json::from_value(json!([
                {"workspace": 1, "name": "main", "window_count": 4,
                 "current": true}
            ]))
            .unwrap();
            let found = pane_records(&windows(), &named, &[], None);

            assert_eq!(found[0].workspace_label.as_deref(), Some("main"));
        }

        #[test]
        fn should_keep_agent_but_no_status_when_state_is_none() {
            let record =
                pane_records(&windows(), &workspaces(), &agents(), None)
                    .into_iter()
                    .find(|r| r.handle.0 == AGENT_WINDOW)
                    .unwrap();

            assert_eq!(
                (
                    record.agent.as_deref(),
                    record.status,
                    record.status_confidence
                ),
                (Some("claude-code"), Status::Unknown, Confidence::None)
            );
        }

        #[test]
        fn should_give_no_agent_session_when_conversation_id_empty() {
            let bare: Vec<AgentJson> = serde_json::from_value(json!([
                {"window_id": AGENT_WINDOW, "harness_id": "",
                 "agent_session_id": ""}
            ]))
            .unwrap();
            let record = pane_records(&windows(), &workspaces(), &bare, None)
                .into_iter()
                .find(|r| r.handle.0 == AGENT_WINDOW)
                .unwrap();

            assert_eq!(
                (
                    record.agent,
                    record.agent_session,
                    record.session_confidence
                ),
                (None, None, Confidence::None)
            );
        }
    }

    mod parse_print_id {
        use super::*;

        #[test]
        fn should_return_uuid_when_window_created() {
            let out = include_str!(
                "../tests/fixtures/tuios/new_window_print_id.txt"
            );

            assert_eq!(
                parse_print_id(out).ok().as_deref(),
                Some("fb62bc0f-9e6f-49a1-a9b4-aa44f4b9879d")
            );
        }

        #[rstest]
        #[case::empty("")]
        #[case::short_form("d6b97fe4  build\n")]
        fn should_fail_when_shape_is_wrong(#[case] stdout: &str) {
            assert_eq!(failure(parse_print_id(stdout)), Some("unexpected"));
        }
    }

    mod parse_json {
        use super::*;

        #[test]
        fn should_return_new_window_id_when_split_succeeds() {
            let created = parse_json::<Created>(include_str!(
                "../tests/fixtures/tuios/split_window.json"
            ));

            assert_eq!(
                created.ok().map(|c| c.window_id).as_deref(),
                Some("47bef9e9-2910-466a-b39d-e506814dcdc5")
            );
        }

        #[test]
        fn should_fail_unexpected_when_window_id_missing() {
            assert_eq!(
                failure(parse_json::<Created>(r#"{"success":true}"#)),
                Some("unexpected")
            );
        }

        #[test]
        fn should_read_both_split_directions_when_fixtures_parse() {
            let down = parse_json::<Created>(include_str!(
                "../tests/fixtures/tuios/split_window_down.json"
            ));

            assert_eq!(
                down.ok().map(|c| c.window_id).as_deref(),
                Some("d3fa833e-99fe-49d1-af47-94e6d659121d")
            );
        }
    }

    mod read_text {
        use super::*;

        #[test]
        fn should_trim_text_when_capture_has_trailing_blank_lines() {
            assert_eq!(
                read_text(include_str!(
                    "../tests/fixtures/tuios/capture_pane.json"
                ))
                .ok()
                .as_deref(),
                Some(
                    "\n/tmp ⬢ v26.7.0 ❯ echo hello-$((40+2))\nhello-42\n\
                     echo x\n\n/tmp ⬢ v26.7.0 ❯ echo x\nx\n\n\
                     /tmp ⬢ v26.7.0 ❯"
                )
            );
        }

        #[test]
        fn should_keep_escape_codes_when_capture_is_ansi() {
            let text = read_text(include_str!(
                "../tests/fixtures/tuios/capture_pane_ansi.json"
            ));

            assert!(
                text.as_ref().is_ok_and(|t| t.contains('\u{1b}')),
                "got {text:?}"
            );
        }

        #[test]
        fn should_fail_unexpected_when_content_missing() {
            assert_eq!(
                failure(read_text(r#"{"success":true}"#)),
                Some("unexpected")
            );
        }
    }

    mod with_session {
        use super::*;

        #[test]
        fn should_insert_session_after_verb_when_given() {
            assert_eq!(
                with_session(&v(&["close-window", "W"]), Some("s")),
                v(&["close-window", "-s", "s", "W"])
            );
        }

        #[test]
        fn should_leave_args_alone_when_no_session() {
            assert_eq!(
                with_session(&v(&["list-windows", "--json"]), None),
                v(&["list-windows", "--json"])
            );
        }

        #[test]
        fn should_stay_empty_when_args_are_empty() {
            assert_eq!(with_session(&[], Some("s")), Vec::<String>::new());
        }
    }

    mod unsupported {
        use super::*;

        #[test]
        fn should_refuse_workspace_create_and_tag_when_slots_are_fixed() {
            let driver = Tuios::new(None, None);
            let created =
                driver.workspace_create("x", Path::new("/tmp")).map(drop);
            let tagged = driver.workspace_tag("1", &[], &[]).map(drop);

            assert_eq!(
                (failure(created), failure(tagged)),
                (Some("unsupported"), Some("unsupported"))
            );
        }

        #[test]
        fn should_refuse_spawn_when_workspace_given() {
            let driver = Tuios::new(None, None);
            let command = v(&["bash"]);
            let request = SpawnRequest {
                name: "n",
                workspace: Some("2"),
                cwd: Path::new("/tmp"),
                command: &command,
            };

            assert_eq!(failure(driver.pane_spawn(&request)), Some("usage"));
        }
    }

    mod args {
        use super::*;

        #[test]
        fn should_build_list_args_with_json() {
            assert_eq!(
                (
                    list_windows_args(),
                    list_workspaces_args(),
                    list_agents_args()
                ),
                (
                    v(&["list-windows", "--json"]),
                    v(&["list-workspaces", "--json"]),
                    v(&["list-agents", "--json"])
                )
            );
        }

        #[test]
        fn should_build_new_window_without_focus_when_spawning() {
            assert_eq!(
                new_window_args("-n", Path::new("/tmp")),
                v(&[
                    "new-window",
                    "--cwd",
                    "/tmp",
                    "--no-focus",
                    "--print-id",
                    "--",
                    "-n"
                ])
            );
        }

        #[rstest]
        #[case::right(Direction::Right, "vertical")]
        #[case::down(Direction::Down, "horizontal")]
        fn should_build_split_window_when_direction_given(
            #[case] direction: Direction,
            #[case] axis: &str,
        ) {
            assert_eq!(
                split_window_args(direction, "W", "n"),
                v(&["split-window", axis, "-w", "W", "--name", "n", "--json"])
            );
        }

        #[rstest]
        #[case::plain(false, &[
            "capture-pane", "-w", "W", "--json", "--scrollback", "--lines",
            "30"
        ])]
        #[case::ansi(true, &[
            "capture-pane", "-w", "W", "--json", "--scrollback", "--ansi",
            "--lines", "30"
        ])]
        fn should_build_capture_when_ansi_is_optional(
            #[case] ansi: bool,
            #[case] expected: &[&str],
        ) {
            assert_eq!(capture_args("W", 30, ansi), v(expected));
        }

        #[test]
        fn should_end_options_before_text_when_sending_text() {
            assert_eq!(
                send_text_args("W", "-n hi"),
                v(&["send-text", "-w", "W", "--", "-n hi"])
            );
        }

        #[test]
        fn should_press_enter_and_ctrl_c_through_send_keys() {
            assert_eq!(
                (send_enter_args("W"), interrupt_args("W")),
                (
                    v(&["send-keys", "-w", "W", "Enter"]),
                    v(&["send-keys", "-w", "W", "ctrl+c"])
                )
            );
        }

        #[test]
        fn should_set_custom_name_when_renaming() {
            assert_eq!(
                rename_args("W", "L"),
                v(&["set-window", "-w", "W", "--name", "L"])
            );
        }

        #[test]
        fn should_close_by_positional_id_when_killing() {
            assert_eq!(close_window_args("W"), v(&["close-window", "W"]));
        }

        #[test]
        fn should_close_workspace_by_number() {
            assert_eq!(
                close_workspace_args("3"),
                v(&["close-workspace", "3"])
            );
        }

        #[test]
        fn should_read_state_as_json_when_asking_status() {
            assert_eq!(
                agent_state_args("W"),
                v(&["get-agent-state", "-w", "W", "--json"])
            );
        }
    }
}
