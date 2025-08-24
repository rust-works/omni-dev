use std::env;
use std::process;

use omni_dev::VERSION;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 {
        match args[1].as_str() {
            "--version" | "-v" => {
                println!("omni-dev {}", VERSION);
                return;
            }
            "--help" | "-h" => {
                print_help();
                return;
            }
            _ => {
                eprintln!("Unknown argument: {}", args[1]);
                print_help();
                process::exit(1);
            }
        }
    }

    println!("Welcome to omni-dev!");
    println!("A comprehensive development toolkit written in Rust.");
    println!();
    println!("Use --help for more information.");
}

fn print_help() {
    println!("omni-dev {}", VERSION);
    println!("A comprehensive development toolkit written in Rust");
    println!();
    println!("USAGE:");
    println!("    omni-dev [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    -h, --help       Print help information");
    println!("    -v, --version    Print version information");
}