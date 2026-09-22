use std::process::Command;

use serde::Deserialize;
use thiserror::Error;

use crate::repository::Repository;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Workflow {
    pub id: u64,
    pub name: String,
    pub path: String,
    pub state: String,
}

#[derive(Debug, Deserialize)]
struct WorkflowPage {
    workflows: Vec<Workflow>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not start GitHub CLI: {0}")]
    Start(#[source] std::io::Error),
    #[error("GitHub CLI request failed: {0}")]
    Request(String),
    #[error("GitHub CLI returned invalid workflow data: {0}")]
    InvalidResponse(#[from] serde_json::Error),
}

pub trait WorkflowSource {
    fn list_workflows(&self, repository: &Repository) -> Result<Vec<Workflow>, Error>;
}

pub struct GhWorkflowSource;

impl WorkflowSource for GhWorkflowSource {
    fn list_workflows(&self, repository: &Repository) -> Result<Vec<Workflow>, Error> {
        let api_path = repository.api_path();
        let output = run_gh(&["api", "--paginate", "--slurp", &api_path])?;
        parse_gh_output(output)
    }
}

struct GhOutput {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn parse_gh_output(output: GhOutput) -> Result<Vec<Workflow>, Error> {
    if !output.success {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(Error::Request(if message.is_empty() {
            format!("gh exited with status {}", output.status)
        } else {
            message
        }));
    }

    parse_workflows(&output.stdout)
}

fn run_gh(arguments: &[&str]) -> Result<GhOutput, Error> {
    let output = Command::new("gh")
        .args(arguments)
        .output()
        .map_err(Error::Start)?;
    Ok(GhOutput {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn parse_workflows(response: &[u8]) -> Result<Vec<Workflow>, Error> {
    let pages: Vec<WorkflowPage> = serde_json::from_slice(response)?;
    Ok(pages.into_iter().flat_map(|page| page.workflows).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_workflows_combines_paginated_responses() {
        let response = br#"[
            {"workflows":[{"id":1,"name":"Build","path":".github/workflows/build.yml","state":"active"}]},
            {"workflows":[{"id":2,"name":"Test","path":".github/workflows/test.yml","state":"disabled_manually"}]}
        ]"#;

        let workflows = parse_workflows(response).unwrap();

        assert_eq!(workflows.len(), 2);
        assert_eq!(workflows[0].name, "Build");
        assert_eq!(workflows[1].id, 2);
    }

    #[test]
    fn parse_workflows_rejects_invalid_response() {
        let error = parse_workflows(br#"{"workflows":[]}"#).unwrap_err();

        assert!(matches!(error, Error::InvalidResponse(_)));
    }

    #[test]
    fn parse_gh_output_reports_api_failure() {
        let output = GhOutput {
            success: false,
            status: "exit code: 1".to_owned(),
            stdout: Vec::new(),
            stderr: b"HTTP 404: Not Found".to_vec(),
        };

        let error = parse_gh_output(output).unwrap_err();

        assert!(matches!(error, Error::Request(message) if message == "HTTP 404: Not Found"));
    }

    #[test]
    fn parse_gh_output_reports_status_when_stderr_is_empty() {
        let output = GhOutput {
            success: false,
            status: "exit code: 1".to_owned(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        let error = parse_gh_output(output).unwrap_err();

        assert!(
            matches!(error, Error::Request(message) if message == "gh exited with status exit code: 1")
        );
    }
}
