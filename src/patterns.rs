//! The status patterns file: regexes that classify a pane's recent
//! output on harnesses without native status (§7.3).

use std::{fs, path::Path};

use regex::Regex;
use serde::Deserialize;

use crate::{
    driver::{Error, Result},
    model::{Confidence, Status},
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
}
