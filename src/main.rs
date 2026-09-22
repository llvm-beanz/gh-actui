mod github;
mod repository;
mod tui;

use std::process::ExitCode;

use clap::Parser;
use github::{GhWorkflowSource, WorkflowSource};
use repository::Repository;
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Browse GitHub Actions workflows in an interactive terminal"
)]
struct Cli {
    /// GitHub repository URL or OWNER/REPO
    repository: Repository,
}

#[derive(Debug, Error)]
enum Error {
    #[error(transparent)]
    Github(#[from] github::Error),
    #[error(transparent)]
    Tui(#[from] std::io::Error),
}

fn main() -> ExitCode {
    match run(Cli::parse(), &GhWorkflowSource) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli, source: &impl WorkflowSource) -> Result<(), Error> {
    let workflows = source.list_workflows(&cli.repository)?;
    tui::run(cli.repository, workflows)?;
    Ok(())
}
