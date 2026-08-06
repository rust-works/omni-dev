//! Shared POSIX-style tokenizer for a user-supplied browser-launch command
//! string (e.g. `SNOWFLAKE_BROWSER_COMMAND`, Gmail's per-account
//! `browser_command`).
//!
//! Promoted out of `crate::snowflake` on its second consumer
//! (`crate::gmail::auth`, issue #1505) — unlike the small, stable
//! `BrowserLaunch` enum shape each of those modules keeps duplicated (see the
//! doc comment on `crate::gmail::auth::BrowserLaunch`: "extract only on a
//! third consumer"), this tokenizer is non-trivial escaping logic worth
//! sharing as soon as a second caller needs it.

use anyhow::{bail, Result};

/// Tokenizes `raw` into program + args with POSIX-style quoting so a program
/// path or an argument value may contain spaces (`Google Chrome.app`,
/// `--profile-directory="Profile 1"`):
///
/// - unquoted whitespace separates tokens;
/// - single quotes take their contents literally;
/// - double quotes group while a backslash escapes only `"` or `\` (so
///   Windows-style paths keep their other backslashes);
/// - an unquoted backslash escapes the next character.
///
/// The `{url}` placeholder is left intact here; the caller's browser-launch
/// code substitutes it (or appends the URL) at launch time.
///
/// `label` names the source of `raw` in error messages (e.g.
/// `"SNOWFLAKE_BROWSER_COMMAND"`, `"browser_command"`).
///
/// # Errors
///
/// If a quote is left unterminated, or the value tokenizes to zero words.
pub(crate) fn split_browser_command(label: &str, raw: &str) -> Result<Vec<String>> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    // Distinguishes an empty quoted arg (`""`) from "no arg accumulated yet".
    let mut has_word = false;
    let mut chars = raw.chars();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            '\'' => {
                has_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => current.push(ch),
                        None => bail!("{label} has an unterminated single quote: {raw}"),
                    }
                }
            }
            '"' => {
                has_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\')) => current.push(ch),
                            Some(ch) => {
                                current.push('\\');
                                current.push(ch);
                            }
                            None => bail!("{label} has an unterminated double quote: {raw}"),
                        },
                        Some(ch) => current.push(ch),
                        None => bail!("{label} has an unterminated double quote: {raw}"),
                    }
                }
            }
            '\\' => {
                has_word = true;
                match chars.next() {
                    Some(ch) => current.push(ch),
                    None => current.push('\\'),
                }
            }
            ch => {
                has_word = true;
                current.push(ch);
            }
        }
    }
    if has_word {
        words.push(current);
    }
    if words.is_empty() {
        bail!("{label} is set but contains no command");
    }
    Ok(words)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn split_browser_command_splits_on_unquoted_whitespace() {
        assert_eq!(
            split_browser_command("LABEL", "chrome --new-window {url}").unwrap(),
            vec!["chrome", "--new-window", "{url}"]
        );
    }

    #[test]
    fn split_browser_command_keeps_quoted_spaces_together() {
        // The motivating case: a program path and an argument value both with
        // spaces, plus the `{url}` placeholder left intact for later substitution.
        assert_eq!(
            split_browser_command(
                "LABEL",
                "'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
                 --profile-directory=\"Profile 1\" --new-window {url}"
            )
            .unwrap(),
            vec![
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "--profile-directory=Profile 1",
                "--new-window",
                "{url}",
            ]
        );
    }

    #[test]
    fn split_browser_command_handles_backslash_escapes() {
        // An unquoted backslash escapes the next char; inside double quotes only
        // `\"` and `\\` are escapes, so other backslashes (Windows paths) survive.
        assert_eq!(
            split_browser_command("LABEL", r#"chrome a\ b "c\"d" "e\\f" "g\h""#).unwrap(),
            vec!["chrome", "a b", "c\"d", "e\\f", "g\\h"]
        );
    }

    #[test]
    fn split_browser_command_rejects_unterminated_quotes() {
        assert!(split_browser_command("LABEL", "chrome \"--flag").is_err());
        assert!(split_browser_command("LABEL", "chrome '--flag").is_err());
    }

    #[test]
    fn split_browser_command_rejects_an_empty_command() {
        assert!(split_browser_command("LABEL", "   ").is_err());
        assert!(split_browser_command("LABEL", "").is_err());
    }

    #[test]
    fn split_browser_command_error_includes_the_label() {
        let err = split_browser_command("MY_ENV_VAR", "chrome \"--flag").unwrap_err();
        assert!(err.to_string().contains("MY_ENV_VAR"));
    }
}
