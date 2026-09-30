//! Text helpers for terminal captures and shell command lines (§6).

use std::sync::LazyLock;

use regex::Regex;

/// Arguments made only of these characters need no quoting.
static SHELL_SAFE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_./=-]+$").ok());

/// Removes the trailing blank lines a terminal capture pads its output
/// with. Inner blank lines stay.
pub fn trim_trailing_blank_lines(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |i| i + 1);

    lines[..end].join("\n")
}

/// Renders `args` as one POSIX shell command line.
pub fn shell_join(args: &[String]) -> String {
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// Quotes one argument unless it is non-empty and shell-safe.
fn quote(arg: &str) -> String {
    let safe = SHELL_SAFE.as_ref().is_some_and(|re| re.is_match(arg));

    if safe {
        return arg.to_string();
    }

    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod trim_trailing_blank_lines {
        use super::*;

        #[rstest]
        #[case::padded("a\nb\n\n  \n\t\n", "a\nb")]
        #[case::inner_blank_kept("a\n\nb\n", "a\n\nb")]
        #[case::all_blank("\n\n", "")]
        #[case::empty("", "")]
        #[case::no_newline("a", "a")]
        fn should_drop_only_trailing_blank_lines(
            #[case] input: &str,
            #[case] expected: &str,
        ) {
            assert_eq!(trim_trailing_blank_lines(input), expected);
        }
    }

    mod shell_join {
        use super::*;

        #[rstest]
        #[case::safe(&["claude", "--model", "opus"], "claude --model opus")]
        #[case::space(&["echo", "a b"], "echo 'a b'")]
        #[case::quote(&["echo", "it's"], r"echo 'it'\''s'")]
        #[case::empty_arg(&["echo", ""], "echo ''")]
        #[case::path_and_eq(&["./x.sh", "K=v"], "./x.sh K=v")]
        #[case::dollar(&["echo", "$HOME"], "echo '$HOME'")]
        fn should_quote_only_unsafe_args(
            #[case] args: &[&str],
            #[case] expected: &str,
        ) {
            let owned: Vec<String> =
                args.iter().map(|s| (*s).to_string()).collect();

            assert_eq!(shell_join(&owned), expected);
        }
    }
}
