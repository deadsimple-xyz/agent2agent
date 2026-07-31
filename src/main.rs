use std::process::ExitCode;

use clap::Parser;

use agent2agent::cli::{self, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli::run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("agent2agent: {e:#}");
            ExitCode::FAILURE
        }
    }
}
