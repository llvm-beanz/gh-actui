mod github;
mod repository;
mod state;
mod tui;

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use github::{GhWorkflowSource, WorkflowSource};
use state::ViewState;
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Browse GitHub Actions workflows in an interactive terminal"
)]
struct Cli {
    /// GitHub repository URL, OWNER/REPO, or existing view state file
    target: String,
}

#[derive(Debug, Error)]
enum Error {
    #[error(transparent)]
    Github(#[from] github::Error),
    #[error(transparent)]
    Tui(#[from] std::io::Error),
    #[error(transparent)]
    State(#[from] state::Error),
    #[error(transparent)]
    Repository(#[from] repository::ParseRepositoryError),
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
    let state_path = PathBuf::from(&cli.target);
    let (repository, workflows, state_path) = if state_path.is_file() {
        let state = ViewState::load(&state_path)?;
        let workflows = source.list_workflows(&state.repository)?;
        let workflows = state.resolve_workflows(workflows);
        (state.repository, workflows, Some(state_path))
    } else {
        let repository = cli.target.parse()?;
        let workflows = source.list_workflows(&repository)?;
        (repository, workflows, None)
    };
    tui::run(repository, workflows, state_path, source)?;
    Ok(())
}
