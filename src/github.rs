use std::{process::Command, thread};

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use thiserror::Error;

use crate::repository::Repository;

const MAX_CONCURRENT_REQUESTS: usize = 8;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Workflow {
    pub id: u64,
    pub name: String,
    pub path: String,
    pub state: String,
    #[serde(skip)]
    pub run_status: RunStatus,
    #[serde(skip)]
    pub is_in_progress: bool,
    #[serde(skip)]
    pub run_metrics: RunMetrics,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RunStatus {
    Success,
    Failure,
    #[default]
    Other,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunMetrics {
    pub last_24_hours: RunCounts,
    pub last_7_days: RunCounts,
    pub last_14_days: RunCounts,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunCounts {
    pub passed: u32,
    pub failed: u32,
    pub total: u32,
}

impl RunCounts {
    pub fn pass_percentage(self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            f64::from(self.passed) / f64::from(self.total) * 100.0
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowPage {
    workflows: Vec<Workflow>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRunPage {
    workflow_runs: Vec<WorkflowRun>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRun {
    status: Option<String>,
    conclusion: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not start GitHub CLI: {0}")]
    Start(#[source] std::io::Error),
    #[error("GitHub CLI request failed: {0}")]
    Request(String),
    #[error("GitHub CLI returned invalid Actions data: {0}")]
    InvalidResponse(#[from] serde_json::Error),
    #[error("a workflow status worker stopped unexpectedly")]
    StatusWorker,
}

pub trait WorkflowSource: Send + Sync {
    fn list_workflows(&self, repository: &Repository) -> Result<Vec<Workflow>, Error>;
}

pub struct GhWorkflowSource;

impl WorkflowSource for GhWorkflowSource {
    fn list_workflows(&self, repository: &Repository) -> Result<Vec<Workflow>, Error> {
        let workflows_path = repository.workflows_api_path();
        let workflow_output = run_gh(&["api", "--paginate", "--slurp", &workflows_path])?;
        let mut workflows = parse_workflows(&successful_stdout(workflow_output)?)?;

        load_run_statuses(repository, &mut workflows, Utc::now())?;

        Ok(workflows)
    }
}

struct GhOutput {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn successful_stdout(output: GhOutput) -> Result<Vec<u8>, Error> {
    if !output.success {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(Error::Request(if message.is_empty() {
            format!("gh exited with status {}", output.status)
        } else {
            message
        }));
    }

    Ok(output.stdout)
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
    Ok(pages
        .into_iter()
        .flat_map(|page| page.workflows)
        .filter(|workflow| workflow.state == "active")
        .collect())
}

fn load_run_statuses(
    repository: &Repository,
    workflows: &mut [Workflow],
    now: DateTime<Utc>,
) -> Result<(), Error> {
    for chunk in workflows.chunks_mut(MAX_CONCURRENT_REQUESTS) {
        let summaries = thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|workflow| {
                    let runs_path = repository.workflow_runs_api_path(workflow.id);
                    scope.spawn(move || fetch_run_status(&runs_path, now))
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().map_err(|_| Error::StatusWorker)?)
                .collect::<Result<Vec<_>, Error>>()
        })?;

        for (workflow, summary) in chunk.iter_mut().zip(summaries) {
            workflow.run_status = summary.run_status;
            workflow.is_in_progress = summary.is_in_progress;
            workflow.run_metrics = summary.run_metrics;
        }
    }

    Ok(())
}

fn fetch_run_status(runs_path: &str, now: DateTime<Utc>) -> Result<RunSummary, Error> {
    let created = format!("created=>={}", (now - Duration::days(14)).to_rfc3339());
    let output = run_gh(&[
        "api",
        "--paginate",
        "--slurp",
        "-X",
        "GET",
        "-f",
        &created,
        runs_path,
    ])?;
    let pages: Vec<WorkflowRunPage> = serde_json::from_slice(&successful_stdout(output)?)?;
    Ok(summarize_runs(
        pages
            .into_iter()
            .flat_map(|page| page.workflow_runs)
            .collect(),
        now,
    ))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RunSummary {
    run_status: RunStatus,
    is_in_progress: bool,
    run_metrics: RunMetrics,
}

fn summarize_runs(runs: Vec<WorkflowRun>, now: DateTime<Utc>) -> RunSummary {
    let mut summary = RunSummary::default();
    let mut found_completed = false;
    for run in runs {
        if run.status.as_deref() == Some("in_progress") {
            summary.is_in_progress = true;
        }

        if run.status.as_deref() == Some("completed") && !found_completed {
            summary.run_status = match run.conclusion.as_deref() {
                Some("success") => RunStatus::Success,
                Some("failure") => RunStatus::Failure,
                _ => RunStatus::Other,
            };
            found_completed = true;
        }

        if run.status.as_deref() == Some("completed") {
            let age = now.signed_duration_since(run.created_at);
            if age <= Duration::days(14) {
                record_run(
                    &mut summary.run_metrics.last_14_days,
                    run.conclusion.as_deref(),
                );
            }
            if age <= Duration::days(7) {
                record_run(
                    &mut summary.run_metrics.last_7_days,
                    run.conclusion.as_deref(),
                );
            }
            if age <= Duration::hours(24) {
                record_run(
                    &mut summary.run_metrics.last_24_hours,
                    run.conclusion.as_deref(),
                );
            }
        }
    }

    summary
}

fn record_run(counts: &mut RunCounts, conclusion: Option<&str>) {
    counts.total += 1;
    match conclusion {
        Some("success") => counts.passed += 1,
        Some("failure") => counts.failed += 1,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-09-23T12:00:00Z".parse().unwrap()
    }

    fn run(status: &str, conclusion: Option<&str>, age: Duration) -> WorkflowRun {
        WorkflowRun {
            status: Some(status.to_owned()),
            conclusion: conclusion.map(str::to_owned),
            created_at: now() - age,
        }
    }

    #[test]
    fn parse_workflows_combines_paginated_responses() {
        let response = br#"[
            {"workflows":[{"id":1,"name":"Build","path":".github/workflows/build.yml","state":"active"}]},
            {"workflows":[{"id":2,"name":"Test","path":".github/workflows/test.yml","state":"active"}]}
        ]"#;

        let workflows = parse_workflows(response).unwrap();

        assert_eq!(workflows.len(), 2);
        assert_eq!(workflows[0].name, "Build");
        assert_eq!(workflows[1].id, 2);
    }

    #[test]
    fn parse_workflows_excludes_inactive_workflows() {
        let response = br#"[
            {"workflows":[
                {"id":1,"name":"Build","path":".github/workflows/build.yml","state":"active"},
                {"id":2,"name":"Old Build","path":".github/workflows/old.yml","state":"disabled_manually"},
                {"id":3,"name":"Fork Build","path":".github/workflows/fork.yml","state":"disabled_fork"}
            ]}
        ]"#;

        let workflows = parse_workflows(response).unwrap();

        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].name, "Build");
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

        let error = successful_stdout(output).unwrap_err();

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

        let error = successful_stdout(output).unwrap_err();

        assert!(
            matches!(error, Error::Request(message) if message == "gh exited with status exit code: 1")
        );
    }

    #[test]
    fn summarize_runs_uses_latest_completed_run() {
        let runs = vec![
            run("completed", Some("success"), Duration::hours(1)),
            run("completed", Some("failure"), Duration::hours(2)),
            run("completed", Some("failure"), Duration::hours(3)),
        ];

        let summary = summarize_runs(runs, now());

        assert_eq!(summary.run_status, RunStatus::Success);
    }

    #[test]
    fn summarize_runs_marks_any_in_progress_run() {
        let runs = vec![
            run("in_progress", None, Duration::minutes(5)),
            run("completed", Some("success"), Duration::hours(1)),
        ];

        let summary = summarize_runs(runs, now());

        assert!(summary.is_in_progress);
        assert_eq!(summary.run_status, RunStatus::Success);
    }

    #[test]
    fn summarize_runs_uses_other_without_completed_run() {
        let summary = summarize_runs(vec![run("queued", None, Duration::minutes(5))], now());

        assert_eq!(summary.run_status, RunStatus::Other);
        assert!(!summary.is_in_progress);
    }

    #[test]
    fn summarize_runs_counts_completed_runs_in_time_windows() {
        let runs = vec![
            run("completed", Some("success"), Duration::hours(2)),
            run("completed", Some("failure"), Duration::days(3)),
            run("completed", Some("cancelled"), Duration::days(10)),
            run("in_progress", None, Duration::minutes(5)),
            run("completed", Some("success"), Duration::days(15)),
        ];

        let summary = summarize_runs(runs, now());

        assert_eq!(
            summary.run_metrics.last_24_hours,
            RunCounts {
                passed: 1,
                failed: 0,
                total: 1
            }
        );
        assert_eq!(
            summary.run_metrics.last_7_days,
            RunCounts {
                passed: 1,
                failed: 1,
                total: 2
            }
        );
        assert_eq!(
            summary.run_metrics.last_14_days,
            RunCounts {
                passed: 1,
                failed: 1,
                total: 3
            }
        );
    }

    #[test]
    fn pass_percentage_uses_total_completed_runs() {
        let counts = RunCounts {
            passed: 10,
            failed: 15,
            total: 28,
        };

        assert!((counts.pass_percentage() - 35.714).abs() < 0.001);
        assert_eq!(RunCounts::default().pass_percentage(), 0.0);
    }
}
