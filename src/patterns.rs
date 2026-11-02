//! The status patterns file: regexes that classify a pane's recent
//! output on harnesses without native status (§7.3).

use std::{fs, path::Path};

use regex::Regex;
use serde::Deserialize;

use crate::{
    driver::{Error, Result},
    model::{Confidence, Handle, Status, StatusResult},
};

/// Compiled idle and blocked patterns. The default has none.
#[derive(Debug, Default)]
pub struct Patterns {
    /// Matches output that means "waiting for input".
    idle: Vec<Regex>,
    /// Matches output that means "stuck on a dialog".
    blocked: Vec<Regex>,
}

/// The file format before compilation.
#[derive(Deserialize)]
struct PatternsFile {
    /// Idle regexes.
    #[serde(default)]
    idle: Vec<String>,
    /// Blocked regexes.
    #[serde(default)]
    blocked: Vec<String>,
}

impl Patterns {
    /// Reads and compiles the patterns file at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] when the file is unreadable, not JSON, or holds
    /// an invalid regex.
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| {
            Error::Usage(format!("reading --patterns {}: {e}", path.display()))
        })?;

        Self::from_json(&text)
    }

    /// Compiles patterns from JSON text.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] on bad JSON or an invalid regex.
    pub fn from_json(text: &str) -> Result<Self> {
        let file: PatternsFile = serde_json::from_str(text)
            .map_err(|e| Error::Usage(format!("parsing --patterns: {e}")))?;

        Ok(Self {
            idle: compile(&file.idle)?,
            blocked: compile(&file.blocked)?,
        })
    }

    /// Classifies `tail`: blocked first, then idle, else working.
    pub fn classify(&self, tail: &str) -> (Status, Confidence) {
        if self.idle.is_empty() && self.blocked.is_empty() {
            return (Status::Unknown, Confidence::None);
        }

        if self.blocked.iter().any(|re| re.is_match(tail)) {
            return (Status::Blocked, Confidence::Heuristic);
        }

        if self.idle.iter().any(|re| re.is_match(tail)) {
            return (Status::Idle, Confidence::Heuristic);
        }

        (Status::Working, Confidence::Heuristic)
    }
}

/// Picks the status for `target`. A `status` other than
/// [`Status::Unknown`] is native and `read` is never called. Otherwise,
/// with `patterns`, `read` supplies the screen text to classify;
/// without them, nothing is read.
///
/// # Errors
///
/// Any error from `read`.
pub fn resolve_status(
    target: &Handle,
    status: Status,
    patterns: Option<&Patterns>,
    read: impl FnOnce() -> Result<String>,
) -> Result<StatusResult> {
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

/// Compiles each source regex.
fn compile(sources: &[String]) -> Result<Vec<Regex>> {
    sources
        .iter()
        .map(|s| {
            Regex::new(s).map_err(|e| {
                Error::Usage(format!("invalid pattern {s:?}: {e}"))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    mod classify {
        use super::*;

        #[test]
        fn should_report_none_when_no_patterns() {
            let patterns = Patterns::default();

            assert_eq!(
                patterns.classify("anything"),
                (Status::Unknown, Confidence::None)
            );
        }

        #[test]
        fn should_prefer_blocked_when_both_match() {
            let patterns = Patterns::from_json(
                r#"{"idle":["\\$\\s*$"],"blocked":["Trust this folder\\?"]}"#,
            )
            .unwrap();

            assert_eq!(
                patterns.classify("Trust this folder?\n$ "),
                (Status::Blocked, Confidence::Heuristic)
            );
        }

        #[test]
        fn should_report_idle_when_idle_matches() {
            let patterns =
                Patterns::from_json(r#"{"idle":["\\$\\s*$"]}"#).unwrap();

            assert_eq!(
                patterns.classify("done\n$ "),
                (Status::Idle, Confidence::Heuristic)
            );
        }

        #[test]
        fn should_report_working_when_nothing_matches() {
            let patterns =
                Patterns::from_json(r#"{"idle":["^never$"]}"#).unwrap();

            assert_eq!(
                patterns.classify("compiling"),
                (Status::Working, Confidence::Heuristic)
            );
        }
    }

    mod from_json {
        use super::*;

        #[test]
        fn should_fail_usage_when_regex_invalid() {
            let err = Patterns::from_json(r#"{"idle":["("]}"#).unwrap_err();

            assert!(matches!(err, Error::Usage(_)), "got {err:?}");
        }
    }

    mod resolve_status {
        use std::cell::Cell;

        use super::*;

        /// A patterns set whose idle regex matches a `❯` prompt.
        fn patterns() -> Patterns {
            Patterns::from_json(r#"{"idle":["❯ $"]}"#).unwrap()
        }

        /// A handle for the pane under test.
        fn target() -> Handle {
            Handle("t1".into())
        }

        #[test]
        fn should_be_native_without_reading_when_status_known() {
            let result = resolve_status(
                &target(),
                Status::Blocked,
                Some(&patterns()),
                || panic!("must not read"),
            )
            .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail),
                (Status::Blocked, Confidence::Native, None)
            );
        }

        #[test]
        fn should_be_native_when_status_known_and_no_patterns() {
            let result =
                resolve_status(&target(), Status::Working, None, || {
                    panic!("must not read")
                })
                .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail),
                (Status::Working, Confidence::Native, None)
            );
        }

        #[test]
        fn should_be_unknown_without_reading_when_no_patterns() {
            let read = Cell::new(false);
            let result =
                resolve_status(&target(), Status::Unknown, None, || {
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
        fn should_classify_screen_when_unknown_and_patterns() {
            let result = resolve_status(
                &target(),
                Status::Unknown,
                Some(&patterns()),
                || Ok("/tmp ❯ ".into()),
            )
            .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail),
                (
                    Status::Idle,
                    Confidence::Heuristic,
                    Some("/tmp ❯ ".to_string())
                )
            );
        }

        #[test]
        fn should_stay_unknown_with_tail_when_patterns_empty() {
            let empty = Patterns::default();
            let result = resolve_status(
                &target(),
                Status::Unknown,
                Some(&empty),
                || Ok("anything".into()),
            )
            .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail),
                (
                    Status::Unknown,
                    Confidence::None,
                    Some("anything".to_string())
                )
            );
        }

        #[test]
        fn should_be_blocked_when_idle_and_blocked_both_match() {
            let both = Patterns::from_json(
                r#"{"idle":["\\$\\s*$"],"blocked":["Trust this folder\\?"]}"#,
            )
            .unwrap();
            let text = "Trust this folder?\n$ ";
            let result = resolve_status(
                &target(),
                Status::Unknown,
                Some(&both),
                || Ok(text.into()),
            )
            .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail),
                (
                    Status::Blocked,
                    Confidence::Heuristic,
                    Some(text.to_string())
                )
            );
        }

        #[test]
        fn should_be_working_when_patterns_match_nothing() {
            let result = resolve_status(
                &target(),
                Status::Unknown,
                Some(&patterns()),
                || Ok("compiling".into()),
            )
            .unwrap();

            assert_eq!(
                (result.status, result.confidence, result.tail),
                (
                    Status::Working,
                    Confidence::Heuristic,
                    Some("compiling".to_string())
                )
            );
        }

        #[test]
        fn should_pass_read_error_through_when_patterns_set() {
            let err = resolve_status(
                &target(),
                Status::Unknown,
                Some(&patterns()),
                || Err(Error::Unexpected("read failed".into())),
            )
            .unwrap_err();

            assert!(matches!(err, Error::Unexpected(m) if m == "read failed"));
        }
    }
}
