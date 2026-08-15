use std::process::ExitCode;

use clap::Parser;
use codex_mux::{MuxError, cli::Cli};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let surface = if cli.command.is_some() {
        "tmux configuration management"
    } else {
        "interactive TUI"
    };
    eprintln!("{}", MuxError::Unavailable(surface));
    ExitCode::from(2)
}
