mod cache;
mod github;
mod query;
mod repository;
mod state;
mod tui;

use std::{path::PathBuf, process::ExitCode, sync::Arc};

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
    Tui(#[from] std::io::Error),
    #[error(transparent)]
    State(#[from] state::Error),
    #[error(transparent)]
    Repository(#[from] repository::ParseRepositoryError),
}

fn main() -> ExitCode {
    match run(Cli::parse(), Arc::new(GhWorkflowSource::new())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli, source: Arc<dyn WorkflowSource>) -> Result<(), Error> {
    let state_path = PathBuf::from(&cli.target);
    let (repository, workflow_ids, tabs, active_tab, state_path) = if state_path.is_file() {
        let state = ViewState::load(&state_path)?;
        (
            state.repository,
            Some(state.workflow_ids),
            Some(state.tabs),
            state.active_tab,
            Some(state_path),
        )
    } else {
        let repository = cli.target.parse()?;
        (repository, None, None, 0, None)
    };
    tui::run(
        repository,
        workflow_ids,
        tabs,
        active_tab,
        state_path,
        source,
    )?;
    Ok(())
}
