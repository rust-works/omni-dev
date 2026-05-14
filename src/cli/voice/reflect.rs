//! `omni-dev voice reflect` — feed a `transcript.jsonl` through the
//! configured [`AiClient`](crate::claude::ai::AiClient) and emit
//! reflection events (per the #799 schema) to `events.jsonl`.
//!
//! Input precedence: `<transcript>` path arg → `--session <id>` →
//! stdin. A literal `-` as the path also means stdin. When `--session`
//! is given, events are appended to that session's `events.jsonl` and
//! `meta.last_reflected_event_id` is advanced; otherwise events stream
//! to stdout.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Parser;

use crate::claude::client::create_default_claude_client;
use crate::voice::clock::SystemClock;
use crate::voice::det::SystemUlidRng;
use crate::voice::reflect::{run_reflect, ReflectOptions, TranscriptSource};

/// Reflects on a transcript and emits reflection events.
///
/// The transcript source is resolved in this order: positional
/// `<transcript>` argument → `--session <id>` (reads
/// `~/.omni-dev/voice/<id>/transcript.jsonl` incrementally) → stdin.
/// A literal `-` as the positional argument also means stdin.
///
/// When `--session` is given, the resulting events are appended to that
/// session's `events.jsonl` and `meta.last_reflected_event_id` is
/// advanced so the next invocation only reflects on newly-arrived
/// transcript events; otherwise events stream to stdout.
#[derive(Parser)]
pub struct ReflectCommand {
    /// Path to a `transcript.jsonl` file. Pass `-` for stdin. Omit to
    /// fall back to `--session` or stdin.
    #[arg(value_name = "TRANSCRIPT")]
    pub transcript: Option<PathBuf>,

    /// Reflect against a named voice session under
    /// `~/.omni-dev/voice/<id>/`. Mutually exclusive with a positional
    /// transcript path.
    #[arg(long)]
    pub session: Option<String>,
}

impl ReflectCommand {
    /// Executes the reflect command. Async because the AI invocation
    /// is async; the caller dispatches inside `#[tokio::main]`.
    pub async fn execute(self) -> Result<()> {
        let source = resolve_source(self.transcript, self.session)?;
        // Fail fast on missing input before paying the AI-client
        // construction cost — otherwise a typo'd path is masked by
        // unrelated credential errors ("API key not found", etc.) on
        // hosts without the configured AI backend.
        if let TranscriptSource::Path(p) = &source {
            if !p.exists() {
                bail!("transcript file does not exist: {}", p.display());
            }
        }
        let ai = create_default_claude_client(None, None)
            .await?
            .into_ai_client();
        let opts = ReflectOptions {
            source,
            ulid_rng: Box::new(SystemUlidRng),
            clock: Box::new(SystemClock),
            ai,
            session_root_override: None,
        };
        // Buffer events in memory rather than holding the StdoutLock
        // across the AI await — the lock guard is `!Send`, which
        // poisons this future for use in a multi-thread Tokio runtime.
        // For session-backed runs the buffer stays empty (events go to
        // events.jsonl, not stdout).
        let mut buf: Vec<u8> = Vec::new();
        run_reflect(opts, &mut buf).await?;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&buf)?;
        stdout.flush()?;
        Ok(())
    }
}

fn resolve_source(
    transcript: Option<PathBuf>,
    session: Option<String>,
) -> Result<TranscriptSource> {
    match (transcript, session) {
        (Some(_), Some(_)) => {
            bail!("voice reflect: pass either a transcript path or --session, not both")
        }
        (Some(path), None) => {
            if path.as_os_str() == "-" {
                Ok(TranscriptSource::Stdin)
            } else {
                Ok(TranscriptSource::Path(path))
            }
        }
        (None, Some(id)) => Ok(TranscriptSource::Session(id)),
        (None, None) => Ok(TranscriptSource::Stdin),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        reflect: ReflectCommand,
    }

    #[test]
    fn parses_no_args_defaults_to_stdin() {
        let cli = TestCli::try_parse_from(["test"]).unwrap();
        assert!(cli.reflect.transcript.is_none());
        assert!(cli.reflect.session.is_none());
        let source = resolve_source(cli.reflect.transcript, cli.reflect.session).unwrap();
        assert!(matches!(source, TranscriptSource::Stdin));
    }

    #[test]
    fn parses_positional_path() {
        let cli = TestCli::try_parse_from(["test", "/tmp/t.jsonl"]).unwrap();
        let source = resolve_source(cli.reflect.transcript, cli.reflect.session).unwrap();
        assert!(
            matches!(source, TranscriptSource::Path(p) if p == std::path::Path::new("/tmp/t.jsonl"))
        );
    }

    #[test]
    fn parses_dash_as_stdin() {
        let cli = TestCli::try_parse_from(["test", "-"]).unwrap();
        let source = resolve_source(cli.reflect.transcript, cli.reflect.session).unwrap();
        assert!(matches!(source, TranscriptSource::Stdin));
    }

    #[test]
    fn parses_session_flag() {
        let cli = TestCli::try_parse_from(["test", "--session", "morning"]).unwrap();
        let source = resolve_source(cli.reflect.transcript, cli.reflect.session).unwrap();
        match source {
            TranscriptSource::Session(s) => assert_eq!(s, "morning"),
            other => panic!("expected Session, got {other:?}"),
        }
    }

    #[test]
    fn rejects_both_transcript_and_session() {
        let cli =
            TestCli::try_parse_from(["test", "/tmp/t.jsonl", "--session", "morning"]).unwrap();
        let err = resolve_source(cli.reflect.transcript, cli.reflect.session).unwrap_err();
        assert!(
            err.to_string()
                .contains("either a transcript path or --session, not both"),
            "got: {err}"
        );
    }
}
