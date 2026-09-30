//! Desktop notifications for the user, independent of any harness
//! (§9).

use std::{
    env, io,
    process::{Command, Output},
};

use serde::{Deserialize, Serialize};

use crate::{
    driver::{Error, Result},
    process,
};

/// How, or whether, a notification was shown.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct NotifyResult {
    /// Whether the user saw it.
    pub sent: bool,
    /// `herdr`, `notify-send`, `osascript`, or `none`.
    pub method: &'static str,
    /// Herdr's reason when it did not show.
    pub reason: Option<String>,
}

/// The part of `herdr notification show` output that matters.
#[derive(Deserialize)]
struct HerdrEnvelope {
    /// The command result.
    result: HerdrShown,
}

/// Herdr's verdict on whether the notification was shown.
#[derive(Deserialize)]
struct HerdrShown {
    /// False when the user disabled notifications.
    shown: bool,
    /// Why it was not shown.
    #[serde(default)]
    reason: Option<String>,
}

/// Shows a notification through the first method available: Herdr,
/// `notify-send`, `osascript`. Nothing available is not an error.
///
/// # Errors
///
/// [`Error::Unexpected`] when an available notifier fails.
pub fn send(
    title: &str,
    message: &str,
    sound: Option<&str>,
) -> Result<NotifyResult> {
    if let Some(result) = via_herdr(title, message, sound)? {
        return Ok(result);
    }

    if let Some(result) = via_notify_send(title, message)? {
        return Ok(result);
    }

    if let Some(result) = via_osascript(title, message)? {
        return Ok(result);
    }

    Ok(NotifyResult {
        sent: false,
        method: "none",
        reason: None,
    })
}

/// Uses Herdr when running inside it and the binary is on PATH.
fn via_herdr(
    title: &str,
    message: &str,
    sound: Option<&str>,
) -> Result<Option<NotifyResult>> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") {
        return Ok(None);
    }

    let mut args = vec!["notification", "show", title, "--body", message];

    if let Some(sound) = sound {
        args.extend(["--sound", sound]);
    }

    let Some(output) = run_tool("herdr", &args)? else {
        return Ok(None);
    };

    parse_herdr_shown(&String::from_utf8_lossy(&output.stdout)).map(Some)
}

/// Uses `notify-send` on Linux when it is on PATH.
fn via_notify_send(
    title: &str,
    message: &str,
) -> Result<Option<NotifyResult>> {
    if !cfg!(target_os = "linux") {
        return Ok(None);
    }

    Ok(
        run_tool("notify-send", &[title, message])?.map(|_| NotifyResult {
            sent: true,
            method: "notify-send",
            reason: None,
        }),
    )
}

/// Uses `osascript` on macOS when it is on PATH.
fn via_osascript(title: &str, message: &str) -> Result<Option<NotifyResult>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }

    let script = format!(
        "display notification {} with title {}",
        applescript_string(message),
        applescript_string(title),
    );

    Ok(
        run_tool("osascript", &["-e", &script])?.map(|_| NotifyResult {
            sent: true,
            method: "osascript",
            reason: None,
        }),
    )
}

/// Runs `program`; `None` when it is not installed, an error when it
/// fails, hangs, or exits nonzero.
fn run_tool(program: &str, args: &[&str]) -> Result<Option<Output>> {
    let output = match process::output_within(
        Command::new(program).args(args),
        process::CEILING,
    ) {
        Ok(output) => output,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Unexpected(format!("running {program}: {e}")));
        }
    };

    if !output.status.success() {
        return Err(Error::Unexpected(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(Some(output))
}

/// Parses `herdr notification show` stdout. `sent` is `shown`, not the
/// exit status.
fn parse_herdr_shown(stdout: &str) -> Result<NotifyResult> {
    let envelope: HerdrEnvelope =
        serde_json::from_str(stdout).map_err(|e| {
            Error::Unexpected(format!(
                "parsing herdr notification output: {e}"
            ))
        })?;

    Ok(NotifyResult {
        sent: envelope.result.shown,
        method: "herdr",
        reason: envelope.result.reason,
    })
}

/// Quotes `s` as an AppleScript string literal.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    mod applescript_string {
        use super::*;

        #[test]
        fn should_escape_quote_and_backslash() {
            assert_eq!(applescript_string(r#"a"b\c"#), r#""a\"b\\c""#);
        }
    }

    mod parse_herdr_shown {
        use super::*;

        #[test]
        fn should_report_sent_when_shown() {
            let result =
                parse_herdr_shown(r#"{"result":{"shown":true}}"#).unwrap();

            assert_eq!(
                result,
                NotifyResult {
                    sent: true,
                    method: "herdr",
                    reason: None
                }
            );
        }

        #[test]
        fn should_report_reason_when_not_shown() {
            let result = parse_herdr_shown(
                r#"{"result":{"shown":false,"reason":"disabled"}}"#,
            )
            .unwrap();

            assert_eq!(
                result,
                NotifyResult {
                    sent: false,
                    method: "herdr",
                    reason: Some("disabled".into())
                }
            );
        }

        #[test]
        fn should_fail_unexpected_when_output_not_json() {
            let err = parse_herdr_shown("nope").unwrap_err();

            assert!(matches!(err, Error::Unexpected(_)), "got {err:?}");
        }
    }
}
