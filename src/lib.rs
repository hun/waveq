//! waveq — query VCD/FST/GHW waveform dumps from the command line.
//!
//! Entry point for both the `waveq` binary and the integration tests:
//! [`run`] parses the CLI, dispatches to a command, prints the document and
//! returns the process exit code (`0` ok, `1` divergence, `2` usage/IO error).

pub mod cli;
pub mod clock;
pub mod cmd;
pub mod config;
pub mod format;
pub mod load;
pub mod output;
pub mod resolve;
pub mod sample;

use std::process::ExitCode;

/// Parse `std::env::args` and run.
pub fn main_entry() -> ExitCode {
    let cli = <cli::Cli as clap::Parser>::parse();
    run(cli)
}

pub fn run(cli: cli::Cli) -> ExitCode {
    match cmd::dispatch(cli) {
        Ok(doc) => ExitCode::from(doc.exit),
        Err(err) => {
            eprintln!("waveq: {err:#}");
            ExitCode::from(2)
        }
    }
}

/// Run with an explicit argument vector (used by tests).
pub fn run_args(args: &[String]) -> ExitCode {
    match <cli::Cli as clap::Parser>::try_parse_from(args) {
        Ok(cli) => run(cli),
        Err(err) => {
            let _ = err.print();
            ExitCode::from(2)
        }
    }
}
