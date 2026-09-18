use std::process::ExitCode;

use clap::Parser;

use wtm::cli::Cli;

fn main() -> ExitCode {
    match wtm::run(Cli::parse()) {
        Ok(code) => ExitCode::from(code as u8),
        Err(error) => {
            eprintln!("wtm: {error}");
            ExitCode::from(error.exit_code() as u8)
        }
    }
}
