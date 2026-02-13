use std::process;

use clap::Parser;
use omni_dev::Cli;

fn main() {
    // Initialize tracing subscriber with RUST_LOG environment variable support
    // Default to "warn" level if RUST_LOG is not set
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    if let Err(e) = cli.execute() {
        eprintln!("Error: {e}");

        // Print the full error chain if available
        let mut source = e.source();
        while let Some(err) = source {
            eprintln!("  Caused by: {err}");
            source = err.source();
        }

        process::exit(1);
    }
}
