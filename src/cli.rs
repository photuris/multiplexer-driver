//! Command-line definitions. Help text comes from these doc comments
//! (§5.6).

use std::{path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand, builder::NonEmptyStringValueParser};

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
    /// Harness to drive. Required for workspace, pane, and raw.
    #[arg(long, global = true, env = "MULTIPLEXER_DRIVER_HARNESS")]
    pub harness: Option<Harness>,
    /// Harness server to use: a Herdr session name or a tmux -L socket
    /// name. Unset uses the ambient server.
    #[arg(long, global = true, env = "MULTIPLEXER_DRIVER_SESSION")]
    pub session: Option<String>,
    /// Indent single-object output. Lists stay JSON Lines.
    #[arg(long, global = true)]
    pub pretty: bool,
    /// Diagnostics level on stderr: debug, info, warn, or error.
    #[arg(long, global = true, env = "RUST_LOG", default_value = "warn")]
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
    ///
    /// Prints {"sent":bool,"method":…,"reason":…}. Uses Herdr inside
    /// Herdr, else notify-send, else osascript; none available is not an
    /// error. Ignores --harness and --session.
    ///
    /// Example: multiplexer-driver notify --message "build done"
    Notify(NotifyArgs),
    /// Run the harness CLI directly, with --session applied.
    ///
    /// Prints the harness stdout unchanged. A failing harness command is
    /// exit 1 with its stderr in the message.
    ///
    /// Example: multiplexer-driver --harness herdr raw -- tab list
    /// --workspace w1
    Raw {
        /// Arguments passed to the harness binary.
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
}

/// `workspace` verbs.
#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// List workspaces.
    ///
    /// Prints JSON Lines, one WorkspaceRecord per line.
    ///
    /// Example: multiplexer-driver --harness tmux workspace list
    List,
    /// Create a workspace.
    ///
    /// Prints its WorkspaceRecord.
    ///
    /// Example: multiplexer-driver --harness tmux workspace create
    /// --label project-x
    Create {
        /// Workspace label (tmux session name).
        #[arg(long, value_parser = NonEmptyStringValueParser::new())]
        label: String,
        /// Root directory; defaults to the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Close a workspace and everything in it.
    ///
    /// Prints {"workspace_id":…,"closed":true}.
    ///
    /// Example: multiplexer-driver --harness tmux workspace close '$3'
    Close {
        /// Workspace ID.
        workspace: String,
    },
    /// Set and clear workspace tags.
    ///
    /// Prints {"workspace_id":…,"tags":{…}} with the tags as they are
    /// now. Keys match [A-Za-z0-9_-]+. At least one pair or --clear is
    /// required.
    ///
    /// Example: multiplexer-driver --harness herdr workspace tag w1
    /// project=x --clear old
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

/// `pane` verbs.
#[derive(Debug, Subcommand)]
pub enum PaneCommand {
    /// List every pane, shells included.
    ///
    /// Prints JSON Lines, one PaneRecord per line.
    ///
    /// Example: multiplexer-driver --harness tmux pane list --workspace
    /// '$0'
    List {
        /// Only panes in this workspace.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Open a new tab running a command.
    ///
    /// Prints {"handle":…,"started":true}. If the pane was created but
    /// the command did not start, prints started:false and exits 6.
    ///
    /// Example: multiplexer-driver --harness tmux pane spawn --name
    /// impl-1 --workspace '$0' -- bash -c 'make test'
    Spawn(SpawnArgs),
    /// Split a pane and run a command in the new one.
    ///
    /// Prints {"handle":…,"started":true}. If the pane was created but
    /// the command did not start, prints started:false and exits 6.
    ///
    /// Example: multiplexer-driver --harness tmux pane split %1 --name
    /// logs --direction down -- tail -f app.log
    Split(SplitArgs),
    /// Read the last lines of a pane.
    ///
    /// Prints {"handle":…,"output":…} with trailing blank lines trimmed.
    /// With --ansi, escape codes are kept. Note: the reset code that
    /// closes a dim run can land at the start of the next line.
    ///
    /// Example: multiplexer-driver --harness tmux pane read %1 --lines
    /// 50
    Read {
        /// Pane handle.
        handle: String,
        /// Number of lines to read.
        #[arg(long, default_value_t = 30)]
        lines: u32,
        /// Keep ANSI escape codes.
        #[arg(long)]
        ansi: bool,
    },
    /// Type text into a pane and press Enter.
    ///
    /// Prints {"handle":…,"sent":true}. Does not wait for the agent.
    ///
    /// Example: multiplexer-driver --harness tmux pane prompt %1 --text
    /// 'run the tests'
    Prompt {
        /// Pane handle.
        handle: String,
        /// Text to send.
        #[arg(long, value_parser = NonEmptyStringValueParser::new())]
        text: String,
    },
    /// Set a pane's label. The handle does not change.
    ///
    /// Prints {"handle":…,"label":…}.
    ///
    /// Example: multiplexer-driver --harness tmux pane rename %1 --label
    /// impl-2
    Rename {
        /// Pane handle.
        handle: String,
        /// New label.
        #[arg(long, value_parser = NonEmptyStringValueParser::new())]
        label: String,
    },
    /// Send Ctrl-C to a pane.
    ///
    /// Prints {"handle":…,"interrupted":true}.
    ///
    /// Example: multiplexer-driver --harness tmux pane interrupt %1
    Interrupt {
        /// Pane handle.
        handle: String,
    },
    /// Close one pane, never its tab or workspace.
    ///
    /// Prints {"handle":…,"killed":true}.
    ///
    /// Example: multiplexer-driver --harness tmux pane kill %1
    Kill {
        /// Pane handle.
        handle: String,
    },
    /// Report the agent status of a pane.
    ///
    /// Prints {"handle":…,"status":…,"confidence":…,"tail":…}. tmux needs
    /// --patterns for a signal; Herdr reports natively and ignores it.
    ///
    /// Example: multiplexer-driver --harness tmux pane status %1
    /// --patterns patterns.json
    Status {
        /// Pane handle.
        handle: String,
        /// JSON file of idle and blocked regexes (tmux only).
        #[arg(long)]
        patterns: Option<PathBuf>,
    },
    /// Wait until a pane reaches a status.
    ///
    /// Polls every 500 ms and prints the StatusResult that matched. Exits
    /// 7 on timeout. tmux requires --patterns.
    ///
    /// Example: multiplexer-driver --harness tmux pane wait %1 --timeout
    /// 5m --until idle --patterns patterns.json
    Wait {
        /// Pane handle.
        handle: String,
        /// Longest time to wait: an integer with ms, s, or m.
        #[arg(long, value_parser = parse_duration)]
        timeout: Duration,
        /// Status to wait for (repeatable). Default: idle and blocked.
        #[arg(long)]
        until: Vec<Status>,
        /// JSON file of idle and blocked regexes (tmux only).
        #[arg(long)]
        patterns: Option<PathBuf>,
    },
}

/// Arguments of `pane spawn`.
#[derive(Debug, Args)]
pub struct SpawnArgs {
    /// Label for the new tab and pane.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    pub name: String,
    /// Workspace to open the tab in; defaults to the caller's own.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Working directory; defaults to the current directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Command and arguments, after `--`.
    #[arg(last = true)]
    pub command: Vec<String>,
}

/// Arguments of `pane split`.
#[derive(Debug, Args)]
pub struct SplitArgs {
    /// Pane to split.
    pub handle: String,
    /// Label for the new pane.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    pub name: String,
    /// Side of the new pane.
    #[arg(long, value_enum, default_value_t = Direction::Right)]
    pub direction: Direction,
    /// Working directory; defaults to the current directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Command and arguments, after `--`.
    #[arg(last = true)]
    pub command: Vec<String>,
}

/// `agent` verbs.
#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Print the argv that resumes an agent conversation.
    ///
    /// Prints {"argv":[…]}. Known kinds: claude, codex, pi; any other
    /// kind exits 5. Needs no harness.
    ///
    /// Example: multiplexer-driver agent resume-args --kind claude
    /// --session-ref a6b4
    ResumeArgs {
        /// Agent kind.
        #[arg(long)]
        kind: String,
        /// Conversation ID or session file path.
        #[arg(long)]
        session_ref: String,
    },
}

/// Arguments of `notify`.
#[derive(Debug, Args)]
pub struct NotifyArgs {
    /// Notification body.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    pub message: String,
    /// Notification title.
    #[arg(long, default_value = "Overseer")]
    pub title: String,
    /// Sound: none, done, or request (Herdr only).
    #[arg(long, value_parser = ["none", "done", "request"])]
    pub sound: Option<String>,
}

/// Parses `500ms`, `60s`, or `5m` into a [`Duration`].
///
/// # Errors
///
/// A message for clap when the text has no valid unit or number, or the
/// value overflows.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("{text:?}: missing unit (ms, s, m)"))?;
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{text:?}: missing or too large number"))?;

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
