//! `omni-dev transcript fetch` — auto-detect the source from the locator and
//! download a transcript, rendering it in the requested format.
//!
//! Unlike the per-source `transcript <source> fetch` subcommands, this probes
//! every registered source's `matches` and dispatches to the one that
//! recognises the URL (#1187). The flags mirror the per-source `fetch`.

use std::fs;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::transcript::format::CliFormat;
use crate::transcript::detect::detect;
use crate::transcript::format::Format;
use crate::transcript::source::{FetchOpts, Transcript};

/// Fetches a transcript, auto-detecting the source from the locator.
#[derive(Parser)]
pub struct FetchCommand {
    /// Media URL or source-specific locator (e.g. a bare YouTube video ID).
    /// The source is auto-detected from its shape.
    pub url: String,

    /// Preferred caption language (e.g. `en`, `en-US`). Prefix fallback is
    /// applied — `en` matches `en-US`.
    #[arg(long, default_value = "en")]
    pub lang: String,

    /// Output format.
    #[arg(long, value_enum, default_value_t = CliFormat::Srt)]
    pub format: CliFormat,

    /// Allow falling through to auto-generated (ASR) captions when no manual
    /// track matches.
    #[arg(long)]
    pub auto: bool,

    /// Synthesise a translated track in this target language when no native
    /// track matches.
    #[arg(long, value_name = "LANG")]
    pub translate: Option<String>,

    /// Output file (writes to stdout if omitted).
    #[arg(short, long)]
    pub output: Option<String>,
}

impl FetchCommand {
    /// Detects the source from the locator, then fetches, renders, and writes.
    pub async fn execute(self) -> Result<()> {
        let source = detect(&self.url)?;
        let transcript = source.fetch(&self.url, &self.fetch_opts()).await?;
        render_and_write(&transcript, self.format, self.output.as_deref())
    }

    /// Map the parsed CLI flags onto the library's [`FetchOpts`].
    fn fetch_opts(&self) -> FetchOpts {
        FetchOpts {
            language: self.lang.clone(),
            allow_auto: self.auto,
            translate_to: self.translate.clone(),
        }
    }
}

/// Render `transcript` in `format` and write it to stdout or `output`.
///
/// Split from [`FetchCommand::execute`] so the render + write wiring is
/// unit-testable without the network-bound fetch that precedes it.
fn render_and_write(
    transcript: &Transcript,
    format: CliFormat,
    output: Option<&str>,
) -> Result<()> {
    let rendered = Format::from(format).render(transcript)?;
    write_output(&rendered, output)
}

fn write_output(text: &str, file: Option<&str>) -> Result<()> {
    if let Some(path) = file {
        fs::write(path, text).with_context(|| format!("Failed to write to {path}"))
    } else {
        print!("{text}");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    /// Build a parser for `FetchCommand` rooted at `fetch` so we can drive
    /// it with realistic argv vectors.
    fn parse(args: &[&str]) -> FetchCommand {
        let cmd = FetchCommand::command().no_binary_name(true);
        let matches = cmd.try_get_matches_from(args).unwrap();
        FetchCommand::from_arg_matches(&matches).unwrap()
    }

    #[test]
    fn fetch_command_defaults() {
        let cmd = parse(&["https://youtu.be/dQw4w9WgXcQ"]);
        assert_eq!(cmd.url, "https://youtu.be/dQw4w9WgXcQ");
        assert_eq!(cmd.lang, "en");
        assert_eq!(cmd.format, CliFormat::Srt);
        assert!(!cmd.auto);
        assert_eq!(cmd.translate, None);
        assert_eq!(cmd.output, None);
    }

    #[test]
    fn fetch_command_all_flags() {
        let cmd = parse(&[
            "abc",
            "--lang",
            "fr",
            "--format",
            "vtt",
            "--auto",
            "--translate",
            "en",
            "--output",
            "out.vtt",
        ]);
        assert_eq!(cmd.url, "abc");
        assert_eq!(cmd.lang, "fr");
        assert_eq!(cmd.format, CliFormat::Vtt);
        assert!(cmd.auto);
        assert_eq!(cmd.translate.as_deref(), Some("en"));
        assert_eq!(cmd.output.as_deref(), Some("out.vtt"));
    }

    #[test]
    fn fetch_command_short_output_flag() {
        let cmd = parse(&["abc", "-o", "out.srt"]);
        assert_eq!(cmd.output.as_deref(), Some("out.srt"));
    }

    #[test]
    fn write_output_to_file_writes_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        write_output("hello\n", Some(path.to_str().unwrap())).unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read, "hello\n");
    }

    #[test]
    fn write_output_to_stdout_returns_ok() {
        // Cannot easily capture stdout here; just exercise the branch.
        write_output("noop", None).unwrap();
    }

    #[test]
    fn write_output_invalid_path_errors() {
        let err = write_output("x", Some("/nonexistent_dir_for_test/out.txt")).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    use crate::transcript::cue::Cue;
    use crate::transcript::source::{TrackKind, Transcript};

    #[test]
    fn fetch_opts_maps_flags() {
        let cmd = parse(&["abc", "--lang", "fr", "--auto", "--translate", "en"]);
        let opts = cmd.fetch_opts();
        assert_eq!(opts.language, "fr");
        assert!(opts.allow_auto);
        assert_eq!(opts.translate_to.as_deref(), Some("en"));
    }

    #[test]
    fn render_and_write_renders_selected_format_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let transcript = Transcript {
            source: "stub".into(),
            locator_id: "vid".into(),
            language: "en".into(),
            kind: TrackKind::Manual,
            cues: vec![Cue::new(0, 1_000, "hello world")],
        };
        render_and_write(&transcript, CliFormat::Txt, Some(path.to_str().unwrap())).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("hello world"));
    }
}
