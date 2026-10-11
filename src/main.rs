use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = fadercut::Args::parse();
    match fadercut::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}
