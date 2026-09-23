use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Repository {
    owner: String,
    name: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ParseRepositoryError {
    #[error("repository must be a GitHub URL or OWNER/REPO")]
    InvalidFormat,
    #[error("repository URL must use github.com")]
    UnsupportedHost,
}

impl Repository {
    pub fn workflows_api_path(&self) -> String {
        format!(
            "repos/{}/{}/actions/workflows?per_page=100",
            self.owner, self.name
        )
    }

    pub fn workflow_runs_api_path(&self, workflow_id: u64) -> String {
        format!(
            "repos/{}/{}/actions/workflows/{workflow_id}/runs?per_page=100",
            self.owner, self.name,
        )
    }

    pub fn workflow_content_api_path(&self, workflow_path: &str) -> String {
        format!(
            "repos/{}/{}/contents/{workflow_path}",
            self.owner, self.name,
        )
    }

    pub fn run_jobs_api_path(&self, run_id: u64) -> String {
        format!(
            "repos/{}/{}/actions/runs/{run_id}/jobs?per_page=100",
            self.owner, self.name,
        )
    }

    pub fn job_logs_api_path(&self, job_id: u64) -> String {
        format!(
            "repos/{}/{}/actions/jobs/{job_id}/logs",
            self.owner, self.name,
        )
    }
}

impl fmt::Display for Repository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

impl FromStr for Repository {
    type Err = ParseRepositoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.starts_with("https://") || value.starts_with("http://") {
            return parse_url(value);
        }

        parse_owner_name(value)
    }
}

fn parse_url(value: &str) -> Result<Repository, ParseRepositoryError> {
    let url = Url::parse(value).map_err(|_| ParseRepositoryError::InvalidFormat)?;
    if url.host_str() != Some("github.com") {
        return Err(ParseRepositoryError::UnsupportedHost);
    }

    let segments: Vec<_> = url
        .path_segments()
        .ok_or(ParseRepositoryError::InvalidFormat)?
        .filter(|segment| !segment.is_empty())
        .collect();

    if segments.len() != 2 {
        return Err(ParseRepositoryError::InvalidFormat);
    }

    parse_owner_name(&format!("{}/{}", segments[0], segments[1]))
}

fn parse_owner_name(value: &str) -> Result<Repository, ParseRepositoryError> {
    let parts: Vec<_> = value.trim_end_matches('/').split('/').collect();
    if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
        return Err(ParseRepositoryError::InvalidFormat);
    }

    let name = parts[1].trim_end_matches(".git");
    if name.is_empty() {
        return Err(ParseRepositoryError::InvalidFormat);
    }

    Ok(Repository {
        owner: parts[0].to_owned(),
        name: name.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_parses_github_url() {
        let repository: Repository = "https://github.com/llvm/offload-test-suite"
            .parse()
            .unwrap();

        assert_eq!(repository.to_string(), "llvm/offload-test-suite");
    }

    #[test]
    fn from_str_parses_owner_and_name() {
        let repository: Repository = "llvm/offload-test-suite".parse().unwrap();

        assert_eq!(repository.to_string(), "llvm/offload-test-suite");
    }

    #[test]
    fn from_str_removes_git_suffix_and_trailing_slash() {
        let repository: Repository = "https://github.com/llvm/offload-test-suite.git/"
            .parse()
            .unwrap();

        assert_eq!(repository.to_string(), "llvm/offload-test-suite");
    }

    #[test]
    fn from_str_rejects_non_github_host() {
        let error = "https://example.com/llvm/offload-test-suite"
            .parse::<Repository>()
            .unwrap_err();

        assert_eq!(error, ParseRepositoryError::UnsupportedHost);
    }

    #[test]
    fn from_str_rejects_missing_repository() {
        let error = "llvm".parse::<Repository>().unwrap_err();

        assert_eq!(error, ParseRepositoryError::InvalidFormat);
    }

    #[test]
    fn from_str_rejects_empty_name_after_git_suffix() {
        let error = "llvm/.git".parse::<Repository>().unwrap_err();

        assert_eq!(error, ParseRepositoryError::InvalidFormat);
    }
}
