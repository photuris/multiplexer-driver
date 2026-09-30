//! Text helpers for terminal captures and shell command lines (§6).

use std::sync::LazyLock;

use regex::Regex;

use crate::driver::{Error, Result};

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

/// Renders `args` as one PowerShell command line that starts with the
/// call operator (§13.2). Safe arguments stay bare; others are
/// single-quoted, with every single-quote character (ASCII and the four
/// Unicode ones PowerShell also accepts) doubled.
///
/// # Errors
///
/// [`Error::Usage`] when `args` is empty, or when an argument is empty,
/// contains `"`, or ends with `\`: Windows PowerShell 5.1 passes those
/// to native programs wrongly.
pub fn powershell_join(args: &[String]) -> Result<String> {
    if args.is_empty() {
        return Err(Error::Usage("a command is required".into()));
    }

    for arg in args {
        if arg.is_empty() || arg.contains('"') || arg.ends_with('\\') {
            return Err(Error::Usage(format!(
                "argument {arg:?} cannot be passed through PowerShell: \
                 it is empty, contains a double quote, or ends with a \
                 backslash"
            )));
        }
    }

    let quoted: Vec<String> = args.iter().map(|a| ps_quote(a)).collect();

    Ok(format!("& {}", quoted.join(" ")))
}

/// Quotes one argument for PowerShell unless it is shell-safe.
fn ps_quote(arg: &str) -> String {
    let safe = SHELL_SAFE.as_ref().is_some_and(|re| re.is_match(arg));

    if safe {
        return arg.to_string();
    }

    let mut out = String::from("'");

    for c in arg.chars() {
        out.push(c);

        if matches!(
            c,
            '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}'
        ) {
            out.push(c);
        }
    }

    out.push('\'');

    out
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod powershell_join {
        use super::*;

        /// Owned argument strings from literals.
        fn owned(args: &[&str]) -> Vec<String> {
            args.iter().map(|s| (*s).to_string()).collect()
        }

        #[rstest]
        #[case::safe(&["claude", "--model", "opus"], "& claude --model opus")]
        #[case::space(&["echo", "a b"], "& echo 'a b'")]
        #[case::quote(&["echo", "it's"], "& echo 'it''s'")]
        #[case::first_quoted(&["my tool", "x"], "& 'my tool' x")]
        #[case::number(&["123"], "& 123")]
        #[case::keyword(&["if", "x"], "& if x")]
        #[case::smart_quotes(
            &["echo", "\u{2018}x\u{2019}"],
            "& echo '\u{2018}\u{2018}x\u{2019}\u{2019}'"
        )]
        #[case::low_smart_quotes(
            &["echo", "\u{201A}x\u{201B}"],
            "& echo '\u{201A}\u{201A}x\u{201B}\u{201B}'"
        )]
        #[case::dollar(&["echo", "$HOME"], "& echo '$HOME'")]
        #[case::ampersand(&["echo", "x&y"], "& echo 'x&y'")]
        fn should_quote_only_unsafe_args(
            #[case] args: &[&str],
            #[case] expected: &str,
        ) {
            assert_eq!(powershell_join(&owned(args)).unwrap(), expected);
        }

        #[rstest]
        #[case::empty_arg(&["echo", ""])]
        #[case::double_quote(&["echo", "a\"b"])]
        #[case::trailing_backslash(&["echo", "a\\"])]
        #[case::no_args(&[])]
        fn should_fail_usage_when_arg_unpassable(#[case] args: &[&str]) {
            let err = powershell_join(&owned(args)).unwrap_err();

            assert_eq!(err.kind(), "usage");
        }
    }

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
