use clap::Parser;
use omni_dev::Cli;
use std::process;

fn main() {
    let cli = Cli::parse();

    if let Err(e) = cli.execute() {
        eprintln!("Error: {}", e);

        // Print the full error chain if available
        let mut source = e.source();
        while let Some(err) = source {
            eprintln!("  Caused by: {}", err);
            source = err.source();
        }

        process::exit(1);
    }
}
