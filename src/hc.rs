//! Herdr desktop core driver. Handles are `hc` terminal addresses
//! (`t7`). The app arranges terminals into spaces and tabs through the
//! `herdr.space` and `herdr.tab` labels; this driver only reads them.
//! Every argv here was verified against `hc` 0.1.17 on a private core.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    io::{self, ErrorKind},
    process::{Command, Output},
};

use serde::{Deserialize, de::DeserializeOwned};

use crate::{
    agents::is_known_kind,
    driver::{Driver, Error, Result, SpawnRequest, SplitRequest, unsupported},
    model::{Confidence, Handle, PaneRecord, Status, StatusResult},
    patterns::{Patterns, resolve_status},
    process::{CEILING, output_within},
    text::trim_trailing_blank_lines,
};

/// Label that carries the pane label (`hc` has no rename).
const LABEL_KEY: &str = "md.label";

/// Label the Herdr app reads as the space a terminal belongs to.
const SPACE_KEY: &str = "herdr.space";

/// Label the Herdr app reads as the tab a terminal belongs to.
const TAB_KEY: &str = "herdr.tab";

/// Drives one `hc` core through its CLI.
#[derive(Debug)]
pub struct Hc {
    /// `HC_SESSION` value; `None` lets the ambient environment pick the
    /// core.
    session: Option<String>,
    /// Patterns for terminals whose program reports no status.
    patterns: Option<Patterns>,
}

impl Hc {
    /// Creates a driver for the core named `session` (empty means the
    /// ambient core).
    pub fn new(session: Option<String>, patterns: Option<Patterns>) -> Self {
        Self {
            session: session.filter(|s| !s.is_empty()),
            patterns,
        }
    }

    /// The `hc` command with the session environment applied. With a
    /// session, every `HC_*` variable is removed from the child and
    /// `HC_SESSION` is set, so the caller's own terminal context never
    /// leaks in.
    fn command(&self) -> Command {
        let mut command = Command::new("hc");
        let Some(name) = &self.session else {
            return command;
        };
        let vars: Vec<(OsString, OsString)> = env::vars_os().collect();
        let text: Vec<(&str, &str)> = vars
            .iter()
            .filter_map(|(k, v)| Some((k.to_str()?, v.to_str()?)))
            .collect();

        command.env_clear().envs(child_env(Some(name), &text));

        // Variables that are not valid UTF-8 cannot go through the pure
        // rule; keep them unless they are `HC_*`.
        for (key, value) in &vars {
            let is_text = key.to_str().is_some() && value.to_str().is_some();

            if !is_text && !key.to_string_lossy().starts_with("HC_") {
                command.env(key, value);
            }
        }

        command
    }

    /// Runs `hc args…` under the ceiling and returns the raw output.
    fn output<S: AsRef<str>>(&self, args: &[S]) -> Result<Output> {
        let mut command = self.command();

        command.args(args.iter().map(AsRef::as_ref));

        output_within(&mut command, CEILING).map_err(|e| spawn_error(&e))
    }

    /// Runs `hc args…` and returns stdout; a failing exit is classified
    /// from the last line of its stderr.
    fn run<S: AsRef<str>>(&self, args: &[S]) -> Result<String> {
        let output = self.output(args)?;

        if !output.status.success() {
            return Err(classify_error(
                &String::from_utf8_lossy(&output.stderr),
                output.status.code(),
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs `hc args…` and parses its JSON stdout as `T`.
    fn run_json<T: DeserializeOwned, S: AsRef<str>>(
        &self,
        args: &[S],
    ) -> Result<T> {
        let stdout = self.run(args)?;

        serde_json::from_str(&stdout)
            .map_err(|e| Error::Unexpected(format!("parsing hc output: {e}")))
    }

    /// Fetches one terminal with `hc get`.
    fn get(&self, target: &str) -> Result<Terminal> {
        self.run_json(&["get", "--json", target])
    }
}

impl Driver for Hc {
    fn name(&self) -> &'static str {
        "hc"
    }

    fn pane_list(&self, workspace: Option<&str>) -> Result<Vec<PaneRecord>> {
        let list: TerminalList = self.run_json(&["ls", "--json"])?;
        let mut records = Vec::new();

        for entry in list.terminals {
            let record =
                pane_record(&entry.address, &self.get(&entry.address)?);

            if workspace.is_none_or(|id| id == record.workspace_id) {
                records.push(record);
            }
        }

        Ok(records)
    }

    fn pane_spawn(&self, request: &SpawnRequest<'_>) -> Result<Handle> {
        if request.command.is_empty() {
            return Err(Error::Usage("a command is required on hc".into()));
        }

        let named = run_args(request, true);
        let stdout = match self.run(&named) {
            Err(e)
                if hc_name(request.name).is_some()
                    && retry_without_name(&e) =>
            {
                self.run(&run_args(request, false))?
            }
            other => other?,
        };
        let started: Started = serde_json::from_str(&stdout)
            .map_err(|e| Error::Unexpected(format!("parsing hc run: {e}")))?;

        Ok(Handle(started.address))
    }

    fn pane_split(&self, _request: &SplitRequest<'_>) -> Result<Handle> {
        Err(unsupported("hc", "pane split"))
    }

    fn pane_read(
        &self,
        target: &Handle,
        lines: u32,
        ansi: bool,
    ) -> Result<String> {
        if ansi {
            return Err(unsupported("hc", "pane read --ansi"));
        }

        let text = self.run(&read_args(&target.0, lines))?;

        Ok(last_lines(&trim_trailing_blank_lines(&text), lines))
    }

    fn pane_prompt(&self, target: &Handle, text: &str) -> Result<()> {
        self.run(&["send", "--json", &target.0, "--", text])?;

        Ok(())
    }

    fn pane_rename(&self, target: &Handle, label: &str) -> Result<()> {
        self.run(&label_args(&target.0, label))?;

        Ok(())
    }

    fn pane_interrupt(&self, target: &Handle) -> Result<()> {
        self.run(&["keys", "--json", &target.0, "ctrl+c"])?;

        Ok(())
    }

    fn pane_kill(&self, target: &Handle) -> Result<()> {
        match self.run(&["kill", "--json", &target.0]) {
            Err(e) if !is_finished(&e) => return Err(e),
            _ => {}
        }

        self.run(&["rm", "--json", &target.0])?;

        Ok(())
    }

    fn pane_status(&self, target: &Handle) -> Result<StatusResult> {
        let status = live_status(target, &self.get(&target.0)?)?;

        resolve_status(target, status, self.patterns.as_ref(), || {
            self.pane_read(target, 15, false)
        })
    }

    fn raw(&self, args: &[String]) -> Result<String> {
        let output = self.output(args)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            return Err(Error::Unexpected(stderr.trim().to_string()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The `hc run` reply.
#[derive(Debug, Deserialize)]
struct Started {
    /// Address of the new terminal.
    address: String,
}

/// The `hc ls` reply.
#[derive(Debug, Deserialize)]
struct TerminalList {
    /// Live terminals, in `hc` order.
    terminals: Vec<ListEntry>,
}

/// One row of `hc ls`.
#[derive(Debug, Deserialize)]
struct ListEntry {
    /// Terminal address.
    address: String,
}

/// The status record a program reports over OSC 7501.
#[derive(Debug, Deserialize)]
struct StatusRecord {
    /// `idle`, `working`, `done`, `blocked`, or `error`.
    state: String,
}

/// The fields of `hc get` this driver uses.
#[derive(Debug, Deserialize)]
struct Terminal {
    /// `live` or `finished`.
    state: String,
    /// What the program reported; `None` for a program that reports
    /// nothing.
    status: Option<StatusRecord>,
    /// Labels set on the terminal.
    #[serde(default)]
    labels: BTreeMap<String, String>,
    /// Working directory.
    cwd: String,
    /// Program and arguments.
    argv: Vec<String>,
}

/// The error record `hc` writes to stderr.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    /// The record body.
    error: ErrorBody,
}

/// The body of an error record; other fields are ignored.
#[derive(Debug, Deserialize)]
struct ErrorBody {
    /// Machine-readable kind (`not_found`, `name_taken`, …).
    kind: String,
    /// Human-readable message.
    message: String,
}

/// The environment the child sees. With a non-empty `session`, every
/// `HC_*` variable is dropped and `HC_SESSION` is set; otherwise
/// `inherited` passes through unchanged.
fn child_env(
    session: Option<&str>,
    inherited: &[(&str, &str)],
) -> Vec<(String, String)> {
    let owned = |(k, v): &(&str, &str)| (k.to_string(), v.to_string());
    let Some(name) = session.filter(|s| !s.is_empty()) else {
        return inherited.iter().map(owned).collect();
    };

    inherited
        .iter()
        .filter(|(k, _)| !k.starts_with("HC_"))
        .map(owned)
        .chain([("HC_SESSION".to_string(), name.to_string())])
        .collect()
}

/// Maps a subprocess failure to an [`Error`]: a missing binary, a call
/// past the ceiling, and any other spawn failure are all
/// [`Error::HarnessUnavailable`].
fn spawn_error(e: &io::Error) -> Error {
    match e.kind() {
        ErrorKind::NotFound => {
            Error::HarnessUnavailable("hc is not installed".into())
        }
        ErrorKind::TimedOut => {
            Error::HarnessUnavailable(format!("hc timed out: {e}"))
        }
        _ => Error::HarnessUnavailable(format!("running hc: {e}")),
    }
}

/// Classifies a failed call from the last non-empty line of `stderr`,
/// which holds `{"error":{"kind":…,"message":…}}`. `code` is the exit
/// code, if the process exited normally.
fn classify_error(stderr: &str, code: Option<i32>) -> Error {
    let Some(line) = stderr.lines().rfind(|l| !l.trim().is_empty()) else {
        return Error::Unexpected(match code {
            Some(code) => format!("hc exited {code} with no error record"),
            None => "hc was killed by a signal with no error record".into(),
        });
    };
    let Ok(record) = serde_json::from_str::<ErrorEnvelope>(line) else {
        return Error::Unexpected(line.trim().to_string());
    };
    let ErrorBody { kind, message } = record.error;

    match kind.as_str() {
        "not_found" => Error::NotFound(message),
        "name_taken" | "invalid_params" => {
            Error::Usage(format!("{kind}: {message}"))
        }
        _ => Error::Unexpected(format!("{kind}: {message}")),
    }
}

/// `label` when `hc run --name` accepts its shape (1-32 of `a-z 0-9 - _ .`,
/// starting with a letter or digit), else `None`. `hc` also rejects names
/// shaped like an id (`t7`); that shape is undocumented, so the caller
/// retries without a name on `invalid_params` instead of guessing it.
fn hc_name(label: &str) -> Option<&str> {
    let mut bytes = label.bytes();
    let first = bytes.next()?;
    let valid = (first.is_ascii_lowercase() || first.is_ascii_digit())
        && label.len() <= 32
        && bytes.all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'.' | b'_' | b'-')
        });

    valid.then_some(label)
}

/// Whether a failed `hc run --name` is worth retrying without the name.
fn retry_without_name(err: &Error) -> bool {
    matches!(err, Error::Usage(m) if m.starts_with("invalid_params:"))
}

/// Whether `err` says the terminal had already finished.
fn is_finished(err: &Error) -> bool {
    matches!(err, Error::Unexpected(m) if m.starts_with("finished:"))
}

/// Arguments for `hc run`. The name is passed only when `named` and
/// [`hc_name`] accepts it; the label travels in `md.label` either way.
fn run_args(request: &SpawnRequest<'_>, named: bool) -> Vec<String> {
    let mut args: Vec<String> =
        ["run", "--json", "--bg", "--outlive-parent", "--cwd"]
            .map(String::from)
            .into();

    args.push(request.cwd.to_string_lossy().into_owned());
    args.extend(["--label".into(), format!("{LABEL_KEY}={}", request.name)]);

    if let Some(name) = hc_name(request.name).filter(|_| named) {
        args.extend(["--name".into(), name.into()]);
    }

    if let Some(space) = request.workspace {
        args.extend(["--label".into(), format!("{SPACE_KEY}={space}")]);
    }

    args.push("--".into());
    args.extend(request.command.iter().cloned());

    args
}

/// Arguments for `hc read`.
fn read_args(target: &str, lines: u32) -> Vec<String> {
    ["read", target, "--lines"]
        .map(String::from)
        .into_iter()
        .chain([lines.to_string()])
        .collect()
}

/// Arguments for `hc label` that set the pane label.
fn label_args(target: &str, label: &str) -> Vec<String> {
    ["label", "--json", target]
        .map(String::from)
        .into_iter()
        .chain([format!("{LABEL_KEY}={label}")])
        .collect()
}

/// The last `n` lines of `text`, joined with `\n`. `n == 0`, or an `n`
/// that covers every line, returns `text` unchanged. Does not trim.
fn last_lines(text: &str, n: u32) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = usize::try_from(n).unwrap_or(usize::MAX);

    if n == 0 || n >= lines.len() {
        return text.to_string();
    }

    lines[lines.len() - n..].join("\n")
}

/// Maps a reported `hc` status state onto our status; anything else has
/// no native status.
fn map_state(raw: &str) -> Status {
    match raw {
        "working" => Status::Working,
        "idle" | "done" => Status::Idle,
        "blocked" | "error" => Status::Blocked,
        _ => Status::Unknown,
    }
}

/// The native status of a live terminal, or [`Error::NotFound`] when it
/// has finished (a finished terminal has no status to wait on).
fn live_status(target: &Handle, terminal: &Terminal) -> Result<Status> {
    if terminal.state == "finished" {
        return Err(Error::NotFound(format!("{target} has finished")));
    }

    Ok(map_state(
        terminal.status.as_ref().map_or("", |s| s.state.as_str()),
    ))
}

/// The pane record for the terminal at `address`.
fn pane_record(address: &str, terminal: &Terminal) -> PaneRecord {
    let label = |key: &str| terminal.labels.get(key).cloned();
    let program = terminal
        .argv
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a))
        .filter(|p| is_known_kind(p));
    let (status, status_confidence) = match &terminal.status {
        Some(s) => match map_state(&s.state) {
            Status::Unknown => (Status::Unknown, Confidence::None),
            status => (status, Confidence::Native),
        },
        None => (Status::Unknown, Confidence::None),
    };

    PaneRecord {
        handle: Handle(address.to_string()),
        workspace_id: label(SPACE_KEY).unwrap_or_default(),
        workspace_label: None,
        tab_id: label(TAB_KEY).unwrap_or_default(),
        tab_label: None,
        label: label(LABEL_KEY),
        cwd: Some(terminal.cwd.clone()),
        agent: program.map(str::to_string),
        agent_session: None,
        session_confidence: Confidence::None,
        status,
        status_confidence,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rstest::rstest;

    use super::*;

    /// A `SpawnRequest` for `name` in `/work` running `command`.
    fn request<'a>(
        name: &'a str,
        workspace: Option<&'a str>,
        command: &'a [String],
    ) -> SpawnRequest<'a> {
        SpawnRequest {
            name,
            workspace,
            cwd: Path::new("/work"),
            command,
        }
    }

    /// Owned strings from literals.
    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    mod hc_name {
        use super::*;

        #[rstest]
        #[case::empty("", None)]
        #[case::typical("impl-003", Some("impl-003"))]
        #[case::uppercase_and_space("Impl 003", None)]
        #[case::id_shaped("t7", Some("t7"))]
        #[case::longest(&"a".repeat(32), Some(&*"a".repeat(32)))]
        #[case::too_long(&"a".repeat(33), None)]
        #[case::single("a", Some("a"))]
        #[case::leading_dot(".x", None)]
        #[case::digit_first("0x", Some("0x"))]
        #[case::leading_dash("-x", None)]
        fn should_accept_only_valid_names(
            #[case] label: &str,
            #[case] expected: Option<&str>,
        ) {
            assert_eq!(hc_name(label), expected);
        }
    }

    mod last_lines {
        use super::*;

        #[rstest]
        #[case::two("a\nb\nc", 2, "b\nc")]
        #[case::one("a\nb\nc", 1, "c")]
        #[case::all("a\nb\nc", 3, "a\nb\nc")]
        #[case::more_than_all("a\nb\nc", 4, "a\nb\nc")]
        #[case::zero("a\nb", 0, "a\nb")]
        #[case::empty("", 3, "")]
        #[case::untrimmed("a\n\n", 1, "")]
        fn should_keep_the_last_n_lines(
            #[case] text: &str,
            #[case] n: u32,
            #[case] expected: &str,
        ) {
            assert_eq!(last_lines(text, n), expected);
        }
    }

    mod map_state {
        use super::*;

        #[rstest]
        #[case::working("working", Status::Working)]
        #[case::idle("idle", Status::Idle)]
        #[case::done("done", Status::Idle)]
        #[case::blocked("blocked", Status::Blocked)]
        #[case::error("error", Status::Blocked)]
        #[case::empty("", Status::Unknown)]
        #[case::weird("weird", Status::Unknown)]
        fn should_map_reported_state(
            #[case] raw: &str,
            #[case] expected: Status,
        ) {
            assert_eq!(map_state(raw), expected);
        }
    }

    mod classify_error {
        use super::*;

        #[test]
        fn should_be_not_found_when_kind_not_found() {
            let err = classify_error(
                include_str!("../tests/fixtures/hc/error_not_found.json"),
                Some(3),
            );

            assert!(
                matches!(&err, Error::NotFound(m) if m == "no terminal \"nosuch\""),
                "got {err:?}"
            );
        }

        #[test]
        fn should_be_usage_when_kind_name_taken() {
            let err = classify_error(
                include_str!("../tests/fixtures/hc/error_name_taken.json"),
                Some(1),
            );

            assert!(
                matches!(&err, Error::Usage(m) if m == "name_taken: a live terminal is already named \"dup\""),
                "got {err:?}"
            );
        }

        #[test]
        fn should_be_unexpected_when_kind_spawn_failed() {
            let err = classify_error(
                include_str!("../tests/fixtures/hc/error_spawn_failed.json"),
                Some(1),
            );

            assert!(
                matches!(&err, Error::Unexpected(m) if m == "spawn_failed: cannot start \"nope\": No such file or directory (os error 2)"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_be_usage_when_kind_invalid_params() {
            let err = classify_error(
                r#"{"error":{"kind":"invalid_params","message":"bad name"}}"#,
                Some(6),
            );

            assert!(
                matches!(&err, Error::Usage(m) if m == "invalid_params: bad name"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_be_unexpected_with_kind_when_kind_unknown() {
            let err = classify_error(
                r#"{"error":{"kind":"blocked","message":"program is blocked"}}"#,
                Some(9),
            );

            assert!(
                matches!(&err, Error::Unexpected(m) if m == "blocked: program is blocked"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_keep_the_line_when_stderr_is_not_json() {
            let err = classify_error("boom\n", Some(2));

            assert!(
                matches!(&err, Error::Unexpected(m) if m == "boom"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_name_the_exit_code_when_stderr_empty() {
            let err = classify_error("", Some(1));

            assert!(
                matches!(&err, Error::Unexpected(m) if m == "hc exited 1 with no error record"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_name_the_signal_when_no_exit_code() {
            let err = classify_error("\n", None);

            assert!(
                matches!(&err, Error::Unexpected(m) if m == "hc was killed by a signal with no error record"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_use_last_non_empty_line_when_stderr_has_several() {
            let stderr = format!(
                "WARN something\n{}\n\n",
                include_str!("../tests/fixtures/hc/error_not_found.json")
                    .trim()
            );
            let err = classify_error(&stderr, Some(3));

            assert!(
                matches!(&err, Error::NotFound(m) if m == "no terminal \"nosuch\""),
                "got {err:?}"
            );
        }
    }

    mod retry_without_name {
        use super::*;

        #[rstest]
        #[case::invalid_params(Error::Usage("invalid_params: x".into()), true)]
        #[case::name_taken(Error::Usage("name_taken: x".into()), false)]
        #[case::unexpected(Error::Unexpected("invalid_params: x".into()), false)]
        #[case::not_found(Error::NotFound("invalid_params: x".into()), false)]
        #[case::unavailable(
            Error::HarnessUnavailable("invalid_params: x".into()),
            false
        )]
        fn should_retry_only_on_invalid_params(
            #[case] err: Error,
            #[case] expected: bool,
        ) {
            assert_eq!(retry_without_name(&err), expected);
        }
    }

    mod is_finished {
        use super::*;

        #[rstest]
        #[case::finished(Error::Unexpected("finished: gone".into()), true)]
        #[case::blocked(Error::Unexpected("blocked: x".into()), false)]
        #[case::usage(Error::Usage("finished: x".into()), false)]
        #[case::not_found(Error::NotFound("finished".into()), false)]
        fn should_match_only_finished_kind(
            #[case] err: Error,
            #[case] expected: bool,
        ) {
            assert_eq!(is_finished(&err), expected);
        }
    }

    mod spawn_error {
        use super::*;

        #[test]
        fn should_say_not_installed_when_binary_missing() {
            let err = spawn_error(&io::Error::from(ErrorKind::NotFound));

            assert!(
                matches!(&err, Error::HarnessUnavailable(m) if m == "hc is not installed"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_say_timed_out_when_ceiling_passed() {
            let err = spawn_error(&io::Error::new(
                ErrorKind::TimedOut,
                "no exit within 30s",
            ));

            assert!(
                matches!(&err, Error::HarnessUnavailable(m) if m == "hc timed out: no exit within 30s"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_carry_the_io_error_when_spawn_fails_otherwise() {
            let err = spawn_error(&io::Error::new(
                ErrorKind::PermissionDenied,
                "denied",
            ));

            assert!(
                matches!(&err, Error::HarnessUnavailable(m) if m == "running hc: denied"),
                "got {err:?}"
            );
        }
    }

    mod run_args {
        use super::*;

        #[test]
        fn should_build_the_exact_argv_when_workspace_given() {
            let command = strings(&["bash", "-l"]);
            let args =
                run_args(&request("impl-003", Some("s1"), &command), true);

            assert_eq!(
                args,
                strings(&[
                    "run",
                    "--json",
                    "--bg",
                    "--outlive-parent",
                    "--cwd",
                    "/work",
                    "--label",
                    "md.label=impl-003",
                    "--name",
                    "impl-003",
                    "--label",
                    "herdr.space=s1",
                    "--",
                    "bash",
                    "-l",
                ])
            );
        }

        #[test]
        fn should_omit_space_label_when_no_workspace() {
            let command = strings(&["bash"]);
            let args = run_args(&request("w", None, &command), true);

            assert_eq!(
                args,
                strings(&[
                    "run",
                    "--json",
                    "--bg",
                    "--outlive-parent",
                    "--cwd",
                    "/work",
                    "--label",
                    "md.label=w",
                    "--name",
                    "w",
                    "--",
                    "bash",
                ])
            );
        }

        #[test]
        fn should_omit_name_when_label_is_not_a_valid_name() {
            let command = strings(&["bash"]);
            let args = run_args(&request("Impl 003", None, &command), true);

            assert!(!args.contains(&"--name".to_string()), "got {args:?}");
            assert!(
                args.contains(&"md.label=Impl 003".to_string()),
                "got {args:?}"
            );
        }

        #[test]
        fn should_omit_name_when_retrying_without_it() {
            let command = strings(&["bash"]);
            let args = run_args(&request("t7", None, &command), false);

            assert!(!args.contains(&"--name".to_string()), "got {args:?}");
        }

        #[test]
        fn should_put_a_one_argument_command_verbatim_after_separator() {
            let command = strings(&["bash"]);
            let args = run_args(&request("w", None, &command), true);
            let at = args.iter().position(|a| a == "--").unwrap();

            assert_eq!(args[at..], strings(&["--", "bash"])[..]);
        }

        #[test]
        fn should_keep_empty_argument_when_command_has_one() {
            let command = strings(&["printf", ""]);
            let args = run_args(&request("w", None, &command), true);
            let at = args.iter().position(|a| a == "--").unwrap();

            assert_eq!(args[at..], strings(&["--", "printf", ""])[..]);
        }

        #[test]
        fn should_keep_dashed_arguments_after_separator() {
            let command = strings(&["env", "--", "-x"]);
            let args = run_args(&request("w", None, &command), true);
            let at = args.iter().position(|a| a == "--").unwrap();

            assert_eq!(args[at..], strings(&["--", "env", "--", "-x"])[..]);
        }
    }

    mod read_args {
        use super::*;

        #[test]
        fn should_pass_lines_after_target() {
            assert_eq!(
                read_args("t7", 15),
                strings(&["read", "t7", "--lines", "15"])
            );
        }
    }

    mod label_args {
        use super::*;

        #[test]
        fn should_set_the_md_label() {
            assert_eq!(
                label_args("t7", "impl 4"),
                strings(&["label", "--json", "t7", "md.label=impl 4"])
            );
        }
    }

    mod child_env {
        use super::*;

        /// An environment that holds the `HC_*` context variables.
        const INHERITED: [(&str, &str); 5] = [
            ("PATH", "/bin"),
            ("HC_TERMINAL_ID", "t1"),
            ("HC_SCOPE", "s"),
            ("HC_TERMINAL_SOCKET", "/x.sock"),
            ("HOME", "/h"),
        ];

        #[test]
        fn should_drop_hc_variables_and_set_session_when_session_given() {
            assert_eq!(
                child_env(Some("mine"), &INHERITED),
                [
                    ("PATH".to_string(), "/bin".to_string()),
                    ("HOME".to_string(), "/h".to_string()),
                    ("HC_SESSION".to_string(), "mine".to_string()),
                ]
            );
        }

        #[test]
        fn should_replace_inherited_session_when_session_given() {
            let inherited = [("HC_SESSION", "other"), ("PATH", "/bin")];

            assert_eq!(
                child_env(Some("mine"), &inherited),
                [
                    ("PATH".to_string(), "/bin".to_string()),
                    ("HC_SESSION".to_string(), "mine".to_string()),
                ]
            );
        }

        #[rstest]
        #[case::none(None)]
        #[case::empty(Some(""))]
        fn should_inherit_everything_when_no_session(
            #[case] session: Option<&str>,
        ) {
            let expected: Vec<(String, String)> = INHERITED
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

            assert_eq!(child_env(session, &INHERITED), expected);
        }

        #[test]
        fn should_treat_empty_session_as_none_when_building_driver() {
            assert_eq!(Hc::new(Some(String::new()), None).session, None);
            assert_eq!(
                Hc::new(Some("default".into()), None).session.as_deref(),
                Some("default")
            );
        }
    }

    /// `hc get` for a live claude terminal in space `s1`.
    const LIVE_CLAUDE: &str = r#"{"address":"t7","argv":["/home/x/claude","--model","y"],"cwd":"/work","labels":{"herdr.space":"s1","herdr.tab":"b2","md.label":"impl-003"},"name":"impl-003","state":"live","status":{"state":"working","kind":"turn","app":"claude"},"turn":3}"#;

    /// `hc get` for a finished terminal.
    const FINISHED: &str = r#"{"address":"t8","argv":["bash"],"cwd":"/work","labels":{},"state":"finished","status":null}"#;

    mod pane_record {
        use super::*;

        #[test]
        fn should_map_labels_agent_and_native_status_when_live_claude() {
            let terminal: Terminal =
                serde_json::from_str(LIVE_CLAUDE).unwrap();
            let record = pane_record("t7", &terminal);

            assert_eq!(
                record,
                PaneRecord {
                    handle: Handle("t7".into()),
                    workspace_id: "s1".into(),
                    workspace_label: None,
                    tab_id: "b2".into(),
                    tab_label: None,
                    label: Some("impl-003".into()),
                    cwd: Some("/work".into()),
                    agent: Some("claude".into()),
                    agent_session: None,
                    session_confidence: Confidence::None,
                    status: Status::Working,
                    status_confidence: Confidence::Native,
                }
            );
        }

        #[test]
        fn should_leave_unknown_and_no_agent_when_plain_shell() {
            let terminal: Terminal = serde_json::from_str(
                r#"{"argv":["bash"],"cwd":"/","state":"live","status":null}"#,
            )
            .unwrap();
            let record = pane_record("t1", &terminal);

            assert_eq!(
                (
                    record.agent,
                    record.label,
                    record.workspace_id,
                    record.tab_id,
                    record.status,
                    record.status_confidence
                ),
                (
                    None,
                    None,
                    String::new(),
                    String::new(),
                    Status::Unknown,
                    Confidence::None
                )
            );
        }

        #[test]
        fn should_not_claim_native_when_reported_state_is_unmapped() {
            let terminal: Terminal = serde_json::from_str(
                r#"{"argv":["bash"],"cwd":"/","state":"live","status":{"state":"weird"}}"#,
            )
            .unwrap();
            let record = pane_record("t1", &terminal);

            assert_eq!(
                (record.status, record.status_confidence),
                (Status::Unknown, Confidence::None)
            );
        }
    }

    mod live_status {
        use super::*;

        #[test]
        fn should_be_not_found_when_terminal_finished() {
            let terminal: Terminal = serde_json::from_str(FINISHED).unwrap();
            let err =
                live_status(&Handle("t8".into()), &terminal).unwrap_err();

            assert!(
                matches!(&err, Error::NotFound(m) if m == "t8 has finished"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_map_reported_state_when_live() {
            let terminal: Terminal =
                serde_json::from_str(LIVE_CLAUDE).unwrap();

            assert_eq!(
                live_status(&Handle("t7".into()), &terminal).unwrap(),
                Status::Working
            );
        }

        #[test]
        fn should_be_unknown_when_live_and_nothing_reported() {
            let terminal: Terminal = serde_json::from_str(
                r#"{"argv":["bash"],"cwd":"/","state":"live","status":null}"#,
            )
            .unwrap();

            assert_eq!(
                live_status(&Handle("t1".into()), &terminal).unwrap(),
                Status::Unknown
            );
        }
    }

    mod driver {
        use super::*;

        #[test]
        fn should_refuse_empty_command_without_calling_hc() {
            let hc = Hc::new(Some("md-test-nonexistent".into()), None);
            let err = hc.pane_spawn(&request("w", None, &[])).unwrap_err();

            assert!(
                matches!(&err, Error::Usage(m) if m == "a command is required on hc"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_be_unsupported_when_splitting() {
            let hc = Hc::new(None, None);
            let target = Handle("t1".into());
            let command = strings(&["bash"]);
            let err = hc
                .pane_split(&SplitRequest {
                    target: &target,
                    direction: crate::model::Direction::Right,
                    name: "x",
                    cwd: Path::new("/"),
                    command: &command,
                })
                .unwrap_err();

            assert!(
                matches!(&err, Error::Unsupported(m) if m == "pane split is not supported on hc"),
                "got {err:?}"
            );
        }

        #[test]
        fn should_be_unsupported_when_reading_ansi() {
            let hc = Hc::new(None, None);
            let err = hc.pane_read(&Handle("t1".into()), 5, true).unwrap_err();

            assert!(
                matches!(&err, Error::Unsupported(m) if m == "pane read --ansi is not supported on hc"),
                "got {err:?}"
            );
        }
    }
}
