//! `anthropic-api-drift` — compare the Anthropic gateway's field policy with
//! the current Anthropic Messages API, as the official Python SDK's generated
//! types describe it.
//!
//! Used by `.github/workflows/anthropic-api-drift.yml` on a weekly schedule.
//! Writes `drift-report.md` and `drift-report.json` to `--output-dir` and
//! emits `drift=<bool>` to stdout (and to `$GITHUB_OUTPUT` if set), so the
//! workflow can open or update a tracking issue.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use omni_dev::gateway::drift::fetch_drift_report;

#[derive(Debug, Parser)]
#[command(
    name = "anthropic-api-drift",
    about = "Detect drift between the Anthropic gateway's field policy and the Anthropic API"
)]
struct Cli {
    /// Directory to write report files into. Created if it does not exist.
    #[arg(long, default_value = ".")]
    output_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let report = fetch_drift_report().await?;
    let github_output = std::env::var_os("GITHUB_OUTPUT").map(PathBuf::from);
    report.write(
        &cli.output_dir,
        github_output.as_deref(),
        &mut std::io::stdout().lock(),
    )
}
