use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = wombat::cli::Cli::parse();
    if let Err(err) = wombat::cli::run(cli) {
        eprintln!("error: {err:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
