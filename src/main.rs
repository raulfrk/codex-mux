use std::process::ExitCode;

use clap::Parser;
use codex_mux::{app, cli::Cli};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match app::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("codex-mux: {error}");
            ExitCode::FAILURE
        }
    }
}
