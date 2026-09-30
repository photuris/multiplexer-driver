//! Entry point: parse arguments, install tracing, run, and map the
//! outcome to an exit code (§5.5).

use std::{
    io::{self, Write},
    process::ExitCode,
};

use clap::{Parser, error::ErrorKind};
use multiplexer_driver::{cli::Cli, driver::Error, error_record, run};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => return clap_exit(&e),
    };

    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(EnvFilter::new(&cli.log_level))
        .init();

    let mut stdout = io::stdout().lock();
    let outcome = run(cli, &mut stdout);
    let _ = stdout.flush(); // nothing useful to do if stdout is gone

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => report(&err),
    }
}

/// Prints the error record as the last stderr line.
fn report(err: &Error) -> ExitCode {
    eprintln!("{}", error_record(err));

    ExitCode::from(err.exit_code())
}

/// Handles clap's help, version, and usage errors.
fn clap_exit(e: &clap::Error) -> ExitCode {
    let _ = e.print(); // help to stdout, usage text to stderr

    if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        return ExitCode::SUCCESS;
    }

    report(&Error::Usage(e.to_string().trim().replace('\n', " ")))
}
