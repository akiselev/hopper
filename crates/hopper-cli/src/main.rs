//! The `hopper` binary.

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = hopper_cli::Cli::parse();
    let stdin = std::io::stdin();
    match hopper_cli::run(cli, stdin.lock(), std::io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hopper: {e}");
            ExitCode::FAILURE
        }
    }
}
