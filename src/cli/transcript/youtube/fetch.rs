//! `omni-dev transcript youtube fetch` — download a transcript and render it
//! in the requested format.

use std::fs;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::transcript::format::CliFormat;
use crate::transcript::format::Format;
use crate::transcript::source::{FetchOpts, TranscriptSource};
use crate::transcript::sources::youtube::Youtube;

/// Fetches the transcript for a YouTube video.
#[derive(Parser)]
pub struct FetchCommand {
    /// YouTube video URL or bare 11-character video ID.
    pub url: String,

    /// Preferred caption language (e.g. `en`, `en-US`). Prefix fallback is
    /// applied — `en` matches `en-US`.
    #[arg(long, default_value = "en")]
    pub lang: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = CliFormat::Srt)]
    pub output: CliFormat,

    /// Deprecated: use `-o`/`--output` instead.
    #[arg(long = "format", hide = true)]
    pub format: Option<CliFormat>,

    /// Allow falling through to auto-generated (ASR) captions when no manual
    /// track matches.
    #[arg(long)]
    pub auto: bool,

    /// Synthesise a translated track in this target language when no native
    /// track matches.
    #[arg(long, value_name = "LANG")]
    pub translate: Option<String>,

    /// Output file (writes to stdout if omitted).
    #[arg(long = "out-file", value_name = "PATH")]
    pub out_file: Option<String>,
}

impl FetchCommand {
    /// Fetches the transcript and writes it to stdout or `--out-file`.
    pub async fn execute(mut self) -> Result<()> {
        if let Some(format) = self.format.take() {
            eprintln!("warning: --format is deprecated; use -o/--output instead");
            self.output = format;
        }

        let yt = Youtube::new()?;
        let opts = FetchOpts {
            language: self.lang,
            allow_auto: self.auto,
            translate_to: self.translate,
        };
        let transcript = yt.fetch(&self.url, &opts).await?;
        let rendered = Format::from(self.output).render(&transcript)?;
        write_output(&rendered, self.out_file.as_deref())
    }
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
    use clap::CommandFactory;

    /// Build a parser for `FetchCommand` rooted at `fetch` so we can drive
    /// it with realistic argv vectors.
    fn parse(args: &[&str]) -> FetchCommand {
        let cmd = FetchCommand::command().no_binary_name(true);
        let matches = cmd.try_get_matches_from(args).unwrap();
        FetchCommand::from_arg_matches(&matches).unwrap()
    }

    use clap::FromArgMatches;

    #[test]
    fn fetch_command_defaults() {
        let cmd = parse(&["https://youtu.be/dQw4w9WgXcQ"]);
        assert_eq!(cmd.url, "https://youtu.be/dQw4w9WgXcQ");
        assert_eq!(cmd.lang, "en");
        assert_eq!(cmd.output, CliFormat::Srt);
        assert_eq!(cmd.format, None);
        assert!(!cmd.auto);
        assert_eq!(cmd.translate, None);
        assert_eq!(cmd.out_file, None);
    }

    #[test]
    fn fetch_command_all_flags() {
        let cmd = parse(&[
            "abc",
            "--lang",
            "fr",
            "-o",
            "vtt",
            "--auto",
            "--translate",
            "en",
            "--out-file",
            "out.vtt",
        ]);
        assert_eq!(cmd.url, "abc");
        assert_eq!(cmd.lang, "fr");
        assert_eq!(cmd.output, CliFormat::Vtt);
        assert!(cmd.auto);
        assert_eq!(cmd.translate.as_deref(), Some("en"));
        assert_eq!(cmd.out_file.as_deref(), Some("out.vtt"));
    }

    #[test]
    fn fetch_command_out_file_flag() {
        let cmd = parse(&["abc", "--out-file", "out.srt"]);
        assert_eq!(cmd.out_file.as_deref(), Some("out.srt"));
    }

    #[test]
    fn fetch_command_output_accepts_each_variant() {
        for (arg, want) in [
            ("srt", CliFormat::Srt),
            ("vtt", CliFormat::Vtt),
            ("txt", CliFormat::Txt),
            ("json", CliFormat::Json),
        ] {
            let cmd = parse(&["abc", "-o", arg]);
            assert_eq!(cmd.output, want);
        }
    }

    #[test]
    fn fetch_command_deprecated_format_alias_still_parses() {
        // `--format` is captured separately; `execute` folds it into `output`
        // with a stderr warning.
        let cmd = parse(&["abc", "--format", "vtt"]);
        assert_eq!(cmd.format, Some(CliFormat::Vtt));
        assert_eq!(cmd.output, CliFormat::Srt);
    }

    #[tokio::test]
    async fn fetch_command_execute_folds_deprecated_format_flag() {
        // The deprecated `--format` folds into `output` with a warning. `"abc"`
        // is an invalid locator, so `fetch` short-circuits in `extract_video_id`
        // before any HTTP request — covering the fold with no network.
        let cmd = FetchCommand {
            url: "abc".to_string(),
            lang: "en".to_string(),
            output: CliFormat::Srt,
            format: Some(CliFormat::Vtt),
            auto: false,
            translate: None,
            out_file: None,
        };
        assert!(cmd.execute().await.is_err());
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
}
