//! tmux driver (§7). Workspaces are sessions, tabs are windows, and
//! handles are tmux pane IDs (`%12`).

use std::{
    collections::BTreeMap,
    env, io,
    path::Path,
    process::{Command, Output},
    thread,
    time::Duration,
};

use crate::{
    agents::is_known_kind,
    driver::{Driver, Error, Result, SpawnRequest, SplitRequest},
    model::{
        Confidence, Direction, Handle, PaneRecord, Status, StatusResult,
        WorkspaceRecord,
    },
    patterns::Patterns,
    process,
    text::{shell_join, trim_trailing_blank_lines},
};

/// The pane format every pane-creating command prints.
const PANE_ID: &str = "#{pane_id}";

/// `list-panes` format: one tab-separated line per pane. The working
/// directory comes last because it is the one field that may hold tabs
/// and newlines (`parse_panes` rejoins them); labels and names cannot
/// (`check_text`).
const PANE_FORMAT: &str = "#{pane_id}\t#{session_id}\t#{session_name}\t\
#{window_id}\t#{window_name}\t#{@md-label}\t#{@md-agent}\t\
#{pane_current_command}\t#{pane_current_path}";

/// Fields in a [`PANE_FORMAT`] row.
const PANE_FIELDS: usize = 9;

/// `list-sessions` format: ID, name, attached-client count.
const SESSION_FORMAT: &str =
    "#{session_id}\t#{session_name}\t#{session_attached}";

/// Session option prefix for workspace tags.
const TAG_PREFIX: &str = "@md-tag-";

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

    /// Reads the tags of session `id`: key names from `show-options`,
    /// each value from `show-options -v`, because the plain listing
    /// quotes and escapes values.
    fn read_tags(&self, id: &str) -> Result<BTreeMap<String, String>> {
        let listing = self.run(&["show-options", "-t", id])?;
        let mut tags = BTreeMap::new();

        for key in parse_tag_keys(&listing) {
            let option = format!("{TAG_PREFIX}{key}");
            let mut value =
                self.run(&["show-options", "-v", "-t", id, &option])?;

            if value.ends_with('\n') {
                value.pop();
            }

            tags.insert(key, value);
        }

        Ok(tags)
    }

    /// Sets a pane-scoped user option.
    fn set_pane_option(
        &self,
        pane: &str,
        key: &str,
        value: &str,
    ) -> Result<()> {
        let value = escape_semicolon(value);

        self.run(&["set-option", "-p", "-t", pane, key, &value])
            .map(|_| ())
    }
}

impl Driver for Tmux {
    fn name(&self) -> &'static str {
        "tmux"
    }

    fn workspace_list(&self) -> Result<Vec<WorkspaceRecord>> {
        let listing = match self.run(&["list-sessions", "-F", SESSION_FORMAT])
        {
            Err(Error::HarnessUnavailable(msg)) if is_no_server(&msg) => {
                return Ok(Vec::new());
            }
            other => other?,
        };
        let mut records = Vec::new();

        for line in listing.lines().filter(|l| !l.is_empty()) {
            let fields: Vec<&str> = line.split('\t').collect();

            let [id, label, attached] = fields[..] else {
                tracing::warn!("skipping malformed session line: {line:?}");
                continue;
            };

            records.push(WorkspaceRecord {
                workspace_id: id.to_string(),
                label: label.to_string(),
                focused: attached.parse::<u32>().is_ok_and(|n| n > 0),
                tags: self.read_tags(id)?,
            });
        }

        Ok(records)
    }

    fn workspace_create(
        &self,
        label: &str,
        cwd: &Path,
    ) -> Result<WorkspaceRecord> {
        check_text("workspace label", label)?;

        let name = escape_semicolon(label);
        let cwd = escape_semicolon(&cwd.to_string_lossy());
        let id = self.run(&[
            "new-session",
            "-d",
            "-s",
            &name,
            "-c",
            &cwd,
            "-P",
            "-F",
            "#{session_id}",
        ])?;

        Ok(WorkspaceRecord {
            workspace_id: id.trim().to_string(),
            label: label.to_string(),
            focused: false,
            tags: BTreeMap::new(),
        })
    }

    fn workspace_close(&self, id: &str) -> Result<()> {
        self.run(&["kill-session", "-t", id])?;

        Ok(())
    }

    fn workspace_tag(
        &self,
        id: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<BTreeMap<String, String>> {
        for (key, value) in set {
            let option = format!("{TAG_PREFIX}{key}");
            let value = escape_semicolon(value);

            self.run(&["set-option", "-t", id, &option, &value])?;
        }

        for key in clear {
            let option = format!("{TAG_PREFIX}{key}");

            self.run(&["set-option", "-u", "-t", id, &option])?;
        }

        self.read_tags(id)
    }

    fn pane_list(&self, workspace: Option<&str>) -> Result<Vec<PaneRecord>> {
        let mut args = vec!["list-panes", "-F", PANE_FORMAT];

        match workspace {
            Some(ws) => args.extend(["-s", "-t", ws]),
            None => args.push("-a"),
        }

        match self.run(&args) {
            Err(Error::HarnessUnavailable(msg)) if is_no_server(&msg) => {
                Ok(Vec::new())
            }
            other => Ok(parse_panes(&other?)),
        }
    }

    fn pane_spawn(&self, request: &SpawnRequest<'_>) -> Result<Handle> {
        check_text("name", request.name)?;

        let workspace = match request.workspace {
            Some(ws) => ws.to_string(),
            None => self.default_workspace()?,
        };
        let cwd = escape_semicolon(&request.cwd.to_string_lossy());
        let joined = shell_join(request.command);
        let name = escape_semicolon(request.name);
        let mut args = vec![
            "new-window",
            "-d",
            "-t",
            &workspace,
            "-n",
            &name,
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
        check_text("name", request.name)?;

        let cwd = escape_semicolon(&request.cwd.to_string_lossy());
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

        // `-S -<n>` adds n history lines to the whole visible pane, so
        // cut back to the last `lines`.
        let trimmed = trim_trailing_blank_lines(&self.run(&args)?);
        let all: Vec<&str> = trimmed.split('\n').collect();
        let keep = all.len().saturating_sub(lines as usize);

        Ok(all[keep..].join("\n"))
    }

    fn pane_prompt(&self, target: &Handle, text: &str) -> Result<()> {
        // `--` keeps text that starts with `-` from parsing as a flag.
        let text = escape_semicolon(text);

        self.run(&["send-keys", "-t", &target.0, "-l", "--", &text])?;
        self.run(&["send-keys", "-t", &target.0, "Enter"])?;

        Ok(())
    }

    fn pane_rename(&self, target: &Handle, label: &str) -> Result<()> {
        check_text("label", label)?;

        self.set_pane_option(&target.0, "@md-label", label)?;
        let title = escape_semicolon(label);

        self.run(&["select-pane", "-t", &target.0, "-T", &title])?;

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

            // A missing server stays `harness_unavailable` (§7.4). So
            // does a dead one whose socket file remains: tmux reports
            // that as "server exited unexpectedly" (`run` retries it,
            // `raw` does not). Any other failure is `unexpected`.
            let message = stderr.trim();

            return Err(match classify_error(message) {
                e @ Error::HarnessUnavailable(_) => e,
                _ if message.contains("server exited unexpectedly") => {
                    Error::HarnessUnavailable(message.to_string())
                }
                _ => Error::Unexpected(message.to_string()),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Protects a trailing `;` in a data argument. tmux reads an argv
/// element ending in `;` as a command separator, even after `--`, and
/// strips one backslash before it; the extra backslash keeps the text
/// literal.
fn escape_semicolon(text: &str) -> String {
    match text.strip_suffix(';') {
        Some(head) => format!("{head}\\;"),
        None => text.to_string(),
    }
}

/// True when tmux stderr means no server is running (§7.4): the
/// `no server running` text, or a connect failure caused by a missing
/// or refusing socket. Other connect failures (permission denied) are
/// real errors.
fn is_no_server(stderr: &str) -> bool {
    stderr.starts_with("no server running")
        || (stderr.starts_with("error connecting to")
            && (stderr.contains("No such file or directory")
                || stderr.contains("Connection refused")))
}

/// Turns an empty field into `None`.
fn non_empty(field: &str) -> Option<String> {
    (!field.is_empty()).then(|| field.to_string())
}

/// Strips leading ASCII digits from `text`; `None` when there are none.
fn skip_digits(text: &str) -> Option<&str> {
    let n = text.bytes().take_while(u8::is_ascii_digit).count();

    (n > 0).then(|| &text[n..])
}

/// True when `line` starts a pane row: `%<digits>\t$<digits>\t`.
fn is_row_start(line: &str) -> bool {
    line.strip_prefix('%')
        .and_then(skip_digits)
        .and_then(|r| r.strip_prefix("\t$"))
        .and_then(skip_digits)
        .is_some_and(|r| r.starts_with('\t'))
}

/// Rejects control characters in a label or name. tmux refuses them in
/// window names, and they would break the line-based listing (§7.2).
fn check_text(what: &str, text: &str) -> Result<()> {
    if text.chars().any(char::is_control) {
        return Err(Error::Usage(format!(
            "{what} {text:?} contains a control character"
        )));
    }

    Ok(())
}

/// Parses `list-panes` output in [`PANE_FORMAT`]. Status and session
/// are never known from listing alone (§7.2). The working directory is
/// the last field, so tabs in it survive the split, and a line that
/// does not start a row continues the previous cwd after a newline.
fn parse_panes(output: &str) -> Vec<PaneRecord> {
    let mut rows: Vec<String> = Vec::new();

    for line in output.lines() {
        match rows.last_mut() {
            Some(row) if !is_row_start(line) => {
                row.push('\n');
                row.push_str(line);
            }
            _ if line.is_empty() => {}
            _ => rows.push(line.to_string()),
        }
    }

    let mut records = Vec::new();

    for row in &rows {
        let fields: Vec<&str> = row.splitn(PANE_FIELDS, '\t').collect();

        let [
            pane,
            ws_id,
            ws_label,
            tab_id,
            tab_label,
            label,
            agent,
            command,
            cwd,
        ] = fields[..]
        else {
            tracing::warn!("skipping malformed pane line: {row:?}");
            continue;
        };
        let agent = non_empty(agent)
            .or_else(|| is_known_kind(command).then(|| command.to_string()));

        records.push(PaneRecord {
            handle: Handle(pane.to_string()),
            workspace_id: ws_id.to_string(),
            workspace_label: non_empty(ws_label),
            tab_id: tab_id.to_string(),
            tab_label: non_empty(tab_label),
            label: non_empty(label),
            cwd: non_empty(cwd),
            agent,
            agent_session: None,
            session_confidence: Confidence::None,
            status: Status::Unknown,
            status_confidence: Confidence::None,
        });
    }

    records
}

/// Extracts tag keys from `show-options` output: the first word of
/// each `@md-tag-<key>` line, prefix removed.
fn parse_tag_keys(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|name| name.strip_prefix(TAG_PREFIX))
        .map(str::to_string)
        .collect()
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

    mod escape_semicolon {
        use super::*;

        #[rstest]
        #[case::plain("echo hi", "echo hi")]
        #[case::trailing(";", "\\;")]
        #[case::word("semi;", "semi\\;")]
        #[case::backslash("a\\;", "a\\\\;")]
        #[case::inner("a;b", "a;b")]
        fn should_escape_only_a_trailing_semicolon(
            #[case] text: &str,
            #[case] escaped: &str,
        ) {
            assert_eq!(escape_semicolon(text), escaped);
        }
    }

    mod is_no_server {
        use super::*;

        #[rstest]
        #[case::running("no server running on /tmp/tmux-1000/x", true)]
        #[case::missing(
            "error connecting to /x (No such file or directory)",
            true
        )]
        #[case::refused("error connecting to /x (Connection refused)", true)]
        #[case::permission(
            "error connecting to /x (Permission denied)",
            false
        )]
        #[case::other("can't find pane: %1", false)]
        fn should_recognise_only_real_no_server_errors(
            #[case] stderr: &str,
            #[case] expected: bool,
        ) {
            assert_eq!(is_no_server(stderr), expected);
        }
    }

    mod parse_panes {
        use super::*;

        #[test]
        fn should_fill_record_when_line_complete() {
            let line = "%1\t$0\tws\t@1\ttabby\timpl-1\tclaude\tnode\t/tmp";

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
            let records = parse_panes("%0\t$0\tws\t@0\tw\t\t\tcodex\t/tmp");

            assert_eq!(records[0].agent.as_deref(), Some("codex"));
        }

        #[test]
        fn should_leave_agent_null_when_command_unknown() {
            let records = parse_panes("%0\t$0\tws\t@0\tw\t\t\tbash\t/tmp");

            assert_eq!(records[0].agent, None);
        }

        #[rstest]
        #[case::tab("/tmp/a\tb")]
        #[case::newline("/tmp/a\nb")]
        #[case::trailing_newline("/tmp/a\n")]
        fn should_keep_cwd_whole_when_it_holds_delimiters(#[case] cwd: &str) {
            let output = format!(
                "%0\t$0\tws\t@0\tw\t\t\tbash\t{cwd}\n\
                 %1\t$0\tws\t@0\tw\tl\t\tbash\t/tmp\n"
            );

            let records = parse_panes(&output);

            assert_eq!(
                records
                    .iter()
                    .map(|r| (r.handle.0.as_str(), r.cwd.as_deref()))
                    .collect::<Vec<_>>(),
                vec![("%0", Some(cwd)), ("%1", Some("/tmp"))]
            );
        }

        #[test]
        fn should_skip_line_when_fields_missing() {
            assert!(parse_panes("%0\t$0\tws\n").is_empty());
        }
    }

    mod check_text {
        use super::*;

        #[rstest]
        #[case::tab("a\tb", false)]
        #[case::newline("a\nb", false)]
        #[case::escape("a\u{1b}b", false)]
        #[case::plain("impl-1 é", true)]
        fn should_reject_only_control_characters(
            #[case] text: &str,
            #[case] ok: bool,
        ) {
            assert_eq!(check_text("label", text).is_ok(), ok);
        }
    }

    mod parse_tag_keys {
        use super::*;

        #[test]
        fn should_keep_only_md_tag_options() {
            let keys = parse_tag_keys(
                "@md-tag-project x\n@other y\nstatus on\n@md-tag-k \"a b\"\n",
            );

            assert_eq!(keys, vec!["project".to_string(), "k".to_string()]);
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
