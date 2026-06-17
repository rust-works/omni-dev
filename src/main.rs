use std::process;

use clap::Parser;
use omni_dev::Cli;

fn main() {
    init_tracing();

    let cli = Cli::parse();

    // The macOS menu-bar daemon needs the GUI event loop on the main thread,
    // which a tokio runtime cannot own. Detect `daemon run` (without
    // `--no-menu`) and hand the main thread to the tray; every other invocation
    // runs the async CLI on a multi-thread runtime exactly as before.
    #[cfg(all(target_os = "macos", feature = "menu-bar"))]
    if let Some(run_config) = cli.menu_bar_run_config() {
        let cfg = match run_config {
            Ok(cfg) => cfg,
            Err(e) => die(&e),
        };
        match omni_dev::daemon::tray::run(cfg) {
            Ok(()) => return,
            Err(e) => die(&e),
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("Error: failed to start the tokio runtime: {e}");
            process::exit(1);
        }
    };

    if let Err(e) = runtime.block_on(cli.execute()) {
        die(&e);
    }
}

/// Initializes the tracing subscriber (stderr, `RUST_LOG`-driven, default
/// `warn`), keeping daemon/debug logs off stdout.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
}

/// Prints an error and its source chain to stderr, then exits non-zero.
fn die(e: &anyhow::Error) -> ! {
    eprintln!("Error: {e}");
    let mut source = e.source();
    while let Some(err) = source {
        eprintln!("  Caused by: {err}");
        source = err.source();
    }
    process::exit(1);
}
