//! Drive terminal multiplexer harnesses (tmux, Herdr) and the coding
//! agents inside them through one vocabulary of workspaces, tabs, and
//! panes. See the design spec in `docs/superpowers/specs/`.

pub mod agents;
pub mod cli;
pub mod driver;
pub mod herdr;
pub mod model;
pub mod notify;
pub mod patterns;
pub mod process;
pub mod text;
pub mod tmux;

use std::{env, io::Write, path::Path, path::PathBuf, time::Duration};

use serde::Serialize;
use serde_json::json;

use crate::{
    cli::{
        AgentCommand, Cli, Command, PaneCommand, SpawnArgs, SplitArgs,
        WorkspaceCommand,
    },
    driver::{Driver, Error, Result, SpawnRequest, SplitRequest},
    model::{Handle, StartResult, Status},
    patterns::Patterns,
};

/// How often `pane wait` polls the status.
const WAIT_INTERVAL: Duration = Duration::from_millis(500);

/// Harnesses this build can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Harness {
    /// tmux.
    Tmux,
    /// Herdr.
    Herdr,
}

/// Builds the driver for `harness`.
///
/// # Errors
///
/// None today; the `Result` leaves room for harness probes.
pub fn driver_for(
    harness: Harness,
    session: Option<&str>,
    patterns: Option<Patterns>,
) -> Result<Box<dyn Driver>> {
    let session = session.map(str::to_string);

    Ok(match harness {
        Harness::Tmux => Box::new(tmux::Tmux::new(session, patterns)),
        Harness::Herdr => Box::new(herdr::Herdr::new(session)),
    })
}

/// Renders the one-line JSON error record for `err` (§5.5).
pub fn error_record(err: &Error) -> String {
    json!({
        "error": { "type": err.kind(), "message": err.to_string() }
    })
    .to_string()
}

/// Runs one parsed command and prints its result to `out`.
///
/// # Errors
///
/// Any [`Error`] from validation, the driver, or writing to `out`.
pub fn run(cli: Cli, out: &mut dyn Write) -> Result<()> {
    let pretty = cli.pretty;

    match cli.command {
        Command::Notify(args) => {
            let result = notify::send(
                &args.title,
                &args.message,
                args.sound.as_deref(),
            )?;

            print_one(out, &result, pretty)
        }
        Command::Agent(AgentCommand::ResumeArgs { kind, session_ref }) => {
            let argv = agents::resume_argv(&kind, &session_ref)?;

            print_one(out, &json!({ "argv": argv }), pretty)
        }
        command => {
            let harness = cli
                .harness
                .ok_or_else(|| Error::Usage("--harness is required".into()))?;
            // The patterns file is a tmux concept; Herdr ignores it, even
            // when it is missing or invalid.
            let patterns = match (harness, patterns_path(&command)) {
                (Harness::Tmux, Some(path)) => Some(Patterns::load(path)?),
                _ => None,
            };
            let driver =
                driver_for(harness, cli.session.as_deref(), patterns)?;

            dispatch(&*driver, command, out, pretty)
        }
    }
}

/// The `--patterns` path of a command, if it takes one.
fn patterns_path(command: &Command) -> Option<&Path> {
    match command {
        Command::Pane(
            PaneCommand::Status { patterns, .. }
            | PaneCommand::Wait { patterns, .. },
        ) => patterns.as_deref(),
        _ => None,
    }
}

/// Sends a driver-backed command to the matching handler.
fn dispatch(
    driver: &dyn Driver,
    command: Command,
    out: &mut dyn Write,
    pretty: bool,
) -> Result<()> {
    match command {
        Command::Workspace(command) => workspace(driver, command, out, pretty),
        Command::Pane(command) => pane(driver, command, out, pretty),
        Command::Raw { args } => {
            let stdout = driver.raw(&args)?;

            written(out.write_all(stdout.as_bytes()))
        }
        Command::Notify(_) | Command::Agent(_) => Err(Error::Unexpected(
            "harness-free command reached the driver".into(),
        )),
    }
}

/// Runs a `workspace` verb.
fn workspace(
    driver: &dyn Driver,
    command: WorkspaceCommand,
    out: &mut dyn Write,
    pretty: bool,
) -> Result<()> {
    match command {
        WorkspaceCommand::List => print_lines(out, &driver.workspace_list()?),
        WorkspaceCommand::Create { label, cwd } => {
            let record =
                driver.workspace_create(&label, &cwd_or_current(cwd)?)?;

            print_one(out, &record, pretty)
        }
        WorkspaceCommand::Close { workspace } => {
            driver.workspace_close(&workspace)?;

            print_one(
                out,
                &json!({ "workspace_id": workspace, "closed": true }),
                pretty,
            )
        }
        WorkspaceCommand::Tag {
            workspace,
            pairs,
            clear,
        } => {
            let set = parse_tag_pairs(&pairs)?;

            if set.is_empty() && clear.is_empty() {
                return Err(Error::Usage(
                    "workspace tag needs a key=value pair or --clear".into(),
                ));
            }

            if let Some(key) = clear.iter().find(|k| !valid_tag_key(k)) {
                return Err(Error::Usage(format!("invalid tag key {key:?}")));
            }

            let tags = driver.workspace_tag(&workspace, &set, &clear)?;

            print_one(
                out,
                &json!({ "workspace_id": workspace, "tags": tags }),
                pretty,
            )
        }
    }
}

/// Runs a `pane` verb.
fn pane(
    driver: &dyn Driver,
    command: PaneCommand,
    out: &mut dyn Write,
    pretty: bool,
) -> Result<()> {
    match command {
        PaneCommand::List { workspace } => {
            print_lines(out, &driver.pane_list(workspace.as_deref())?)
        }
        PaneCommand::Spawn(args) => {
            print_start(out, spawn(driver, &args), pretty)
        }
        PaneCommand::Split(args) => {
            print_start(out, split(driver, &args), pretty)
        }
        PaneCommand::Read {
            handle,
            lines,
            ansi,
        } => {
            let handle = Handle(handle);
            let output = driver.pane_read(&handle, lines, ansi)?;

            print_one(
                out,
                &json!({ "handle": handle, "output": output }),
                pretty,
            )
        }
        PaneCommand::Prompt { handle, text } => {
            let handle = Handle(handle);

            driver.pane_prompt(&handle, &text)?;

            print_one(out, &json!({ "handle": handle, "sent": true }), pretty)
        }
        PaneCommand::Rename { handle, label } => {
            let handle = Handle(handle);

            driver.pane_rename(&handle, &label)?;

            print_one(
                out,
                &json!({ "handle": handle, "label": label }),
                pretty,
            )
        }
        PaneCommand::Interrupt { handle } => {
            let handle = Handle(handle);

            driver.pane_interrupt(&handle)?;

            print_one(
                out,
                &json!({ "handle": handle, "interrupted": true }),
                pretty,
            )
        }
        PaneCommand::Kill { handle } => {
            let handle = Handle(handle);

            driver.pane_kill(&handle)?;

            print_one(
                out,
                &json!({ "handle": handle, "killed": true }),
                pretty,
            )
        }
        PaneCommand::Status { handle, .. } => {
            let result = driver.pane_status(&Handle(handle))?;

            print_one(out, &result, pretty)
        }
        PaneCommand::Wait {
            handle,
            timeout,
            until,
            ..
        } => {
            let until = if until.is_empty() {
                vec![Status::Idle, Status::Blocked]
            } else {
                until
            };
            let result = driver::wait_for(
                driver,
                &Handle(handle),
                &until,
                timeout,
                WAIT_INTERVAL,
            )?;

            print_one(out, &result, pretty)
        }
    }
}

/// Runs `pane spawn`.
fn spawn(driver: &dyn Driver, args: &SpawnArgs) -> Result<Handle> {
    let cwd = cwd_or_current(args.cwd.clone())?;

    driver.pane_spawn(&SpawnRequest {
        name: &args.name,
        workspace: args.workspace.as_deref(),
        cwd: &cwd,
        command: &args.command,
    })
}

/// Runs `pane split`.
fn split(driver: &dyn Driver, args: &SplitArgs) -> Result<Handle> {
    let cwd = cwd_or_current(args.cwd.clone())?;
    let target = Handle(args.handle.clone());

    driver.pane_split(&SplitRequest {
        target: &target,
        direction: args.direction,
        name: &args.name,
        cwd: &cwd,
        command: &args.command,
    })
}

/// `cwd` when given, else the caller's current directory.
fn cwd_or_current(cwd: Option<PathBuf>) -> Result<PathBuf> {
    match cwd {
        Some(cwd) => Ok(cwd),
        None => env::current_dir().map_err(|e| {
            Error::Unexpected(format!("reading current directory: {e}"))
        }),
    }
}

/// Whether `key` is a valid tag key: `[A-Za-z0-9_-]+`.
fn valid_tag_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Splits `key=value` arguments, validating each key.
fn parse_tag_pairs(pairs: &[String]) -> Result<Vec<(String, String)>> {
    pairs
        .iter()
        .map(|pair| {
            let (key, value) = pair.split_once('=').ok_or_else(|| {
                Error::Usage(format!("tag {pair:?} is not key=value"))
            })?;

            if !valid_tag_key(key) {
                return Err(Error::Usage(format!("invalid tag key {key:?}")));
            }

            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Prints the result of spawn or split. A created pane whose command did
/// not start prints `started:false` and still returns the error, so the
/// caller gets the handle and the exit code. Other errors print nothing.
///
/// # Errors
///
/// The error in `outcome`, or [`Error::Unexpected`] when writing fails.
pub fn print_start(
    out: &mut dyn Write,
    outcome: Result<Handle>,
    pretty: bool,
) -> Result<()> {
    match outcome {
        Ok(handle) => print_one(
            out,
            &StartResult {
                handle,
                started: true,
            },
            pretty,
        ),
        Err(err) => {
            if let Error::StartFailed { handle, .. } = &err {
                print_one(
                    out,
                    &StartResult {
                        handle: handle.clone(),
                        started: false,
                    },
                    pretty,
                )?;
            }

            Err(err)
        }
    }
}

/// Writes one JSON document and a newline.
fn print_one(
    out: &mut dyn Write,
    value: &impl Serialize,
    pretty: bool,
) -> Result<()> {
    let text = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .map_err(|e| Error::Unexpected(format!("encoding output: {e}")))?;

    written(writeln!(out, "{text}"))
}

/// Writes one compact JSON document per line, ignoring `--pretty`.
fn print_lines(out: &mut dyn Write, items: &[impl Serialize]) -> Result<()> {
    for item in items {
        print_one(out, item, false)?;
    }

    Ok(())
}

/// Maps a failed write to stdout to [`Error::Unexpected`]. A closed
/// reader (`| head -1`) is success: the caller has all it wants.
fn written(result: std::io::Result<()>) -> Result<()> {
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(Error::Unexpected(format!("writing output: {e}"))),
        Ok(()) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod print_start {
        use super::*;

        #[test]
        fn should_print_started_true_when_spawned() {
            let mut out = Vec::new();

            print_start(&mut out, Ok(Handle("%5".into())), false).unwrap();

            assert_eq!(out, b"{\"handle\":\"%5\",\"started\":true}\n");
        }

        #[test]
        fn should_print_started_false_and_fail_when_start_failed() {
            let mut out = Vec::new();
            let err = Error::StartFailed {
                handle: Handle("%5".into()),
                message: "boom".into(),
            };

            let result = print_start(&mut out, Err(err), false);

            assert_eq!(
                (out, matches!(result, Err(Error::StartFailed { .. }))),
                (b"{\"handle\":\"%5\",\"started\":false}\n".to_vec(), true)
            );
        }

        #[test]
        fn should_print_nothing_and_fail_when_no_pane_created() {
            let mut out = Vec::new();

            let result = print_start(
                &mut out,
                Err(Error::NotFound("no such workspace".into())),
                false,
            );

            assert_eq!(
                (out, matches!(result, Err(Error::NotFound(_)))),
                (Vec::new(), true)
            );
        }
    }

    mod valid_tag_key {
        use super::*;

        #[test]
        fn should_reject_empty_and_punctuated_keys() {
            assert!(valid_tag_key("a_b-1"));
            assert!(!valid_tag_key("") && !valid_tag_key("a b"));
        }
    }
}
