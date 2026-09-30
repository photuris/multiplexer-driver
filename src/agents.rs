//! Agent knowledge shared by every driver: which agent kinds exist and
//! how to resume a conversation (§4.5).

use crate::driver::{Error, Result};

/// Agent kinds `herdr agent start --kind` accepts (Herdr 0.8.2). The
/// tmux driver uses the same list to recognize an agent command.
pub const KNOWN_KINDS: [&str; 22] = [
    "pi",
    "claude",
    "codex",
    "gemini",
    "cursor",
    "devin",
    "agy",
    "cline",
    "omp",
    "mastracode",
    "opencode",
    "copilot",
    "kimi",
    "kiro",
    "droid",
    "amp",
    "grok",
    "hermes",
    "kilo",
    "qodercli",
    "qwen",
    "maki",
];

/// Whether `command` names a known agent kind.
pub fn is_known_kind(command: &str) -> bool {
    KNOWN_KINDS.contains(&command)
}

/// Returns the argv that resumes `reference` for agent `kind`.
/// Verified against each tool's `--help` on 2026-09-30.
///
/// # Errors
///
/// [`Error::Unsupported`] for a kind with no verified resume form.
pub fn resume_argv(kind: &str, reference: &str) -> Result<Vec<String>> {
    let parts: [&str; 2] = match kind {
        "claude" => ["claude", "--resume"],
        "codex" => ["codex", "resume"],
        "pi" => ["pi", "--session"],
        _ => {
            return Err(Error::Unsupported(format!(
                "no verified resume command for agent kind {kind:?}"
            )));
        }
    };

    Ok(vec![parts[0].into(), parts[1].into(), reference.into()])
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod resume_argv {
        use super::*;

        #[rstest]
        #[case::claude("claude", "abc", &["claude", "--resume", "abc"])]
        #[case::codex("codex", "abc", &["codex", "resume", "abc"])]
        #[case::pi("pi", "/s/x.jsonl", &["pi", "--session", "/s/x.jsonl"])]
        fn should_build_argv_when_kind_known(
            #[case] kind: &str,
            #[case] reference: &str,
            #[case] expected: &[&str],
        ) {
            assert_eq!(resume_argv(kind, reference).unwrap(), expected);
        }

        #[test]
        fn should_fail_unsupported_when_kind_unknown() {
            let err = resume_argv("gemini", "abc").unwrap_err();

            assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
        }
    }

    mod is_known_kind {
        use super::*;

        #[test]
        fn should_know_all_22_herdr_kinds() {
            assert_eq!(KNOWN_KINDS.len(), 22);
            assert!(is_known_kind("maki") && !is_known_kind("bash"));
        }
    }
}
