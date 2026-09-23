use std::{process::Command, thread};

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use thiserror::Error;

use crate::repository::Repository;

const MAX_CONCURRENT_REQUESTS: usize = 8;
const RUNS_PER_PAGE: usize = 100;

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
    id: Option<u64>,
    status: Option<String>,
    conclusion: Option<String>,
    event: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowTriage {
    pub workflow_id: u64,
    pub failed_jobs: String,
    pub failed_steps: String,
    pub failed_tests: Vec<String>,
    pub unexpectedly_passed_tests: Vec<String>,
    pub lit_summary: String,
}

#[derive(Debug, Deserialize)]
struct JobsPage {
    jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
struct Job {
    id: u64,
    name: String,
    conclusion: Option<String>,
    #[serde(default)]
    steps: Vec<JobStep>,
}

#[derive(Debug, Deserialize)]
struct JobStep {
    name: String,
    conclusion: Option<String>,
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
    fn triage_workflows(
        &self,
        repository: &Repository,
        workflows: &[Workflow],
    ) -> Result<Vec<WorkflowTriage>, Error>;
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

    fn triage_workflows(
        &self,
        repository: &Repository,
        workflows: &[Workflow],
    ) -> Result<Vec<WorkflowTriage>, Error> {
        let mut triage = Vec::new();
        for chunk in workflows.chunks(MAX_CONCURRENT_REQUESTS) {
            let results = thread::scope(|scope| {
                let handles = chunk
                    .iter()
                    .map(|workflow| scope.spawn(move || triage_workflow(repository, workflow)))
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| handle.join().map_err(|_| Error::StatusWorker)?)
                    .collect::<Result<Vec<_>, Error>>()
            })?;
            triage.extend(results.into_iter().flatten());
        }
        Ok(triage)
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
    fetch_run_status_with(runs_path, now, |path| {
        successful_stdout(run_gh(&["api", "-X", "GET", path])?)
    })
}

fn fetch_run_status_with(
    runs_path: &str,
    now: DateTime<Utc>,
    mut fetch_page: impl FnMut(&str) -> Result<Vec<u8>, Error>,
) -> Result<RunSummary, Error> {
    let cutoff = now - Duration::days(14);
    let mut runs = Vec::new();
    let mut page_number = 1;
    let mut found_completed = false;

    loop {
        // GitHub's server-side `created` filter can return stale workflow-run
        // results for workflows with large histories. Fetch the unfiltered,
        // newest-first stream and enforce the time window locally instead.
        let page_path = format!("{runs_path}&page={page_number}");
        let page: WorkflowRunPage = serde_json::from_slice(&fetch_page(&page_path)?)?;
        let page_len = page.workflow_runs.len();
        let reached_cutoff = page.workflow_runs.iter().any(|run| run.created_at < cutoff);
        found_completed |= page
            .workflow_runs
            .iter()
            .any(|run| run.status.as_deref() == Some("completed"));
        runs.extend(page.workflow_runs);

        if page_len < RUNS_PER_PAGE || (reached_cutoff && found_completed) {
            break;
        }
        page_number += 1;
    }

    Ok(summarize_runs(runs, now))
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

fn triage_workflow(
    repository: &Repository,
    workflow: &Workflow,
) -> Result<Option<WorkflowTriage>, Error> {
    if !workflow_has_schedule_trigger(repository, &workflow.path)? {
        return Ok(None);
    }
    let runs_path = repository.workflow_runs_api_path(workflow.id);
    let Some(run_id) = find_failed_scheduled_run(&runs_path)? else {
        return Ok(None);
    };
    let failures = fetch_failed_jobs(repository, run_id)?;
    let failed_jobs = failures
        .iter()
        .map(|failure| failure.job_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let failed_steps = failures
        .iter()
        .map(|failure| failure.step_name.as_deref().unwrap_or("(no step info)"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut summaries = Vec::new();
    let mut failed_tests = Vec::new();
    let mut unexpectedly_passed_tests = Vec::new();
    for failure in failures
        .iter()
        .filter(|failure| failure.step_name.as_deref() == Some("Run HLSL Tests"))
    {
        let path = repository.job_logs_api_path(failure.job_id);
        let log = successful_stdout(run_gh(&["api", &path])?)?;
        let log = String::from_utf8_lossy(&log);
        let (job_failed_tests, job_unexpectedly_passed_tests) = extract_lit_test_names(&log);
        failed_tests.extend(job_failed_tests);
        unexpectedly_passed_tests.extend(job_unexpectedly_passed_tests);
        if let Some(summary) = extract_lit_summary(&log) {
            summaries.push(summary);
        }
    }
    failed_tests.sort();
    failed_tests.dedup();
    unexpectedly_passed_tests.sort();
    unexpectedly_passed_tests.dedup();

    Ok(Some(WorkflowTriage {
        workflow_id: workflow.id,
        failed_jobs: if failed_jobs.is_empty() {
            "(no failed jobs reported)".to_owned()
        } else {
            failed_jobs
        },
        failed_steps,
        failed_tests,
        unexpectedly_passed_tests,
        lit_summary: summaries.join("\n\n"),
    }))
}

fn workflow_has_schedule_trigger(
    repository: &Repository,
    workflow_path: &str,
) -> Result<bool, Error> {
    let path = repository.workflow_content_api_path(workflow_path);
    let contents = successful_stdout(run_gh(&[
        "api",
        "-H",
        "Accept: application/vnd.github.raw+json",
        &path,
    ])?)?;
    Ok(has_schedule_trigger(&String::from_utf8_lossy(&contents)))
}

fn has_schedule_trigger(contents: &str) -> bool {
    let mut in_on_block = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if !in_on_block {
            if line.trim_end() == "on:" {
                in_on_block = true;
            }
            continue;
        }
        if !line.is_empty() && !line.starts_with([' ', '\t']) {
            return false;
        }
        if line.starts_with([' ', '\t']) && trimmed == "schedule:" {
            return true;
        }
    }
    false
}

fn find_failed_scheduled_run(runs_path: &str) -> Result<Option<u64>, Error> {
    find_failed_scheduled_run_with(runs_path, |path| {
        successful_stdout(run_gh(&["api", "-X", "GET", path])?)
    })
}

fn find_failed_scheduled_run_with(
    runs_path: &str,
    mut fetch_page: impl FnMut(&str) -> Result<Vec<u8>, Error>,
) -> Result<Option<u64>, Error> {
    let mut page_number = 1;
    let mut latest_scheduled = None;
    let mut latest_completed = None;
    loop {
        let path = format!("{runs_path}&page={page_number}");
        let page: WorkflowRunPage = serde_json::from_slice(&fetch_page(&path)?)?;
        let page_len = page.workflow_runs.len();
        for run in page.workflow_runs {
            if run.event.as_deref() != Some("schedule") {
                continue;
            }
            if latest_scheduled.is_none() {
                latest_scheduled = Some((run.id, run.status.clone(), run.conclusion.clone()));
            }
            if run.status.as_deref() == Some("completed") && latest_completed.is_none() {
                latest_completed = Some((run.id, run.conclusion));
            }
        }
        if (latest_scheduled.is_some() && latest_completed.is_some()) || page_len < RUNS_PER_PAGE {
            break;
        }
        page_number += 1;
    }

    let Some((latest_id, status, conclusion)) = latest_scheduled else {
        return Ok(None);
    };
    if status.as_deref() == Some("completed") {
        return Ok((conclusion.as_deref() == Some("failure"))
            .then_some(latest_id)
            .flatten());
    }
    Ok(latest_completed
        .filter(|(_, conclusion)| conclusion.as_deref() == Some("failure"))
        .and_then(|(id, _)| id))
}

struct FailedJob {
    job_id: u64,
    job_name: String,
    step_name: Option<String>,
}

fn fetch_failed_jobs(repository: &Repository, run_id: u64) -> Result<Vec<FailedJob>, Error> {
    let base_path = repository.run_jobs_api_path(run_id);
    let mut failures = Vec::new();
    let mut page_number = 1;
    loop {
        let path = format!("{base_path}&page={page_number}");
        let page: JobsPage =
            serde_json::from_slice(&successful_stdout(run_gh(&["api", "-X", "GET", &path])?)?)?;
        let page_len = page.jobs.len();
        for job in page.jobs {
            if matches!(
                job.conclusion.as_deref(),
                None | Some("success" | "skipped" | "neutral")
            ) {
                continue;
            }
            let failed_step = job.steps.into_iter().find(|step| {
                !matches!(
                    step.conclusion.as_deref(),
                    None | Some("success" | "skipped")
                )
            });
            failures.push(FailedJob {
                job_id: job.id,
                job_name: job.name,
                step_name: failed_step.map(|step| step.name),
            });
        }
        if page_len < RUNS_PER_PAGE {
            break;
        }
        page_number += 1;
    }
    Ok(failures)
}

fn extract_lit_summary(log: &str) -> Option<String> {
    let lines = log.lines().map(strip_log_timestamp).collect::<Vec<_>>();
    let testing_time = lines
        .iter()
        .rposition(|line| line.starts_with("Testing Time:"));
    let start = testing_time
        .and_then(|testing_time| {
            let section_start = testing_time + 1;
            lines[section_start..]
                .iter()
                .position(|line| {
                    line.starts_with("Failed Tests (")
                        || line.starts_with("Unexpectedly Passed Tests (")
                })
                .map(|offset| section_start + offset)
                .or(Some(testing_time))
        })
        .or_else(|| {
            let failed = lines
                .iter()
                .rposition(|line| line.starts_with("Failed Tests ("));
            let unexpectedly_passed = lines
                .iter()
                .rposition(|line| line.starts_with("Unexpectedly Passed Tests ("));
            match (failed, unexpectedly_passed) {
                (Some(failed), Some(unexpected)) => Some(failed.max(unexpected)),
                (Some(index), None) | (None, Some(index)) => Some(index),
                (None, None) => None,
            }
        })?;
    let mut end = lines[start..]
        .iter()
        .position(|line| line.starts_with("##["))
        .map_or(lines.len(), |offset| start + offset);
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some(lines[start..end].join("\n"))
}

fn strip_log_timestamp(line: &str) -> &str {
    let (prefix, remainder) = line.split_once(' ').unwrap_or((line, ""));
    if prefix.len() >= 20
        && prefix.as_bytes().get(4) == Some(&b'-')
        && prefix.as_bytes().get(7) == Some(&b'-')
        && prefix.ends_with('Z')
    {
        remainder
    } else {
        line
    }
}

fn extract_lit_test_names(log: &str) -> (Vec<String>, Vec<String>) {
    let lines = log.lines().map(strip_log_timestamp).collect::<Vec<_>>();
    let mut failed = Vec::new();
    let mut unexpectedly_passed = Vec::new();
    let mut section = None;

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with("Failed Tests (") {
            section = Some(true);
            continue;
        }
        if trimmed.starts_with("Unexpectedly Passed Tests (") {
            section = Some(false);
            continue;
        }
        if trimmed.starts_with("**") || trimmed.starts_with("##[") {
            section = None;
            continue;
        }
        if trimmed.starts_with("FAIL:") || trimmed.starts_with("XPASS:") {
            section = None;
            continue;
        }
        if let (Some(failed_section), Some((_, test))) = (section, trimmed.split_once("::")) {
            let target = if failed_section {
                &mut failed
            } else {
                &mut unexpectedly_passed
            };
            target.push(strip_lit_test_suffix(test));
        }
    }

    for line in lines {
        let trimmed = line.trim();
        if let Some(test) = detailed_lit_test_name(trimmed, "FAIL:") {
            failed.push(test);
        } else if let Some(test) = detailed_lit_test_name(trimmed, "XPASS:") {
            unexpectedly_passed.push(test);
        }
    }
    failed.sort();
    failed.dedup();
    unexpectedly_passed.sort();
    unexpectedly_passed.dedup();
    (failed, unexpectedly_passed)
}

fn detailed_lit_test_name(line: &str, prefix: &str) -> Option<String> {
    let remainder = line.strip_prefix(prefix)?.trim();
    let (_, test) = remainder.split_once("::")?;
    Some(strip_lit_test_suffix(test))
}

fn strip_lit_test_suffix(test: &str) -> String {
    let test = test.trim();
    test.rsplit_once(" (")
        .filter(|(_, suffix)| {
            suffix
                .strip_suffix(')')
                .is_some_and(|suffix| suffix.contains(" of "))
        })
        .map_or(test, |(test, _)| test)
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use serde_json::json;

    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-09-23T12:00:00Z".parse().unwrap()
    }

    fn run(status: &str, conclusion: Option<&str>, age: Duration) -> WorkflowRun {
        WorkflowRun {
            id: None,
            status: Some(status.to_owned()),
            conclusion: conclusion.map(str::to_owned),
            event: None,
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
    fn fetch_run_status_pages_unfiltered_results_until_cutoff_and_completion() {
        let recent_queued = (0..RUNS_PER_PAGE)
            .map(|_| {
                json!({
                    "status": "queued",
                    "conclusion": null,
                    "created_at": (now() - Duration::hours(1)).to_rfc3339()
                })
            })
            .collect::<Vec<_>>();
        let old_completed = json!([{
            "status": "completed",
            "conclusion": "failure",
            "created_at": (now() - Duration::days(15)).to_rfc3339()
        }]);
        let mut responses = VecDeque::from([
            serde_json::to_vec(&json!({"workflow_runs": recent_queued})).unwrap(),
            serde_json::to_vec(&json!({"workflow_runs": old_completed})).unwrap(),
        ]);
        let mut requested_paths = Vec::new();

        let summary = fetch_run_status_with(
            "repos/owner/repo/actions/workflows/42/runs?per_page=100",
            now(),
            |path| {
                requested_paths.push(path.to_owned());
                Ok(responses.pop_front().unwrap())
            },
        )
        .unwrap();

        assert_eq!(
            requested_paths,
            [
                "repos/owner/repo/actions/workflows/42/runs?per_page=100&page=1",
                "repos/owner/repo/actions/workflows/42/runs?per_page=100&page=2"
            ]
        );
        assert!(requested_paths.iter().all(|path| !path.contains("created")));
        assert_eq!(summary.run_status, RunStatus::Failure);
        assert_eq!(summary.run_metrics.last_14_days.total, 0);
    }

    #[test]
    fn fetch_run_status_stops_after_full_page_crosses_cutoff() {
        let mut runs = (0..RUNS_PER_PAGE - 1)
            .map(|_| {
                json!({
                    "status": "completed",
                    "conclusion": "success",
                    "created_at": (now() - Duration::hours(1)).to_rfc3339()
                })
            })
            .collect::<Vec<_>>();
        runs.push(json!({
            "status": "completed",
            "conclusion": "failure",
            "created_at": (now() - Duration::days(15)).to_rfc3339()
        }));
        let response = serde_json::to_vec(&json!({"workflow_runs": runs})).unwrap();
        let mut request_count = 0;

        let summary = fetch_run_status_with("runs?per_page=100", now(), |_| {
            request_count += 1;
            Ok(response.clone())
        })
        .unwrap();

        assert_eq!(request_count, 1);
        assert_eq!(summary.run_metrics.last_14_days.total, 99);
    }

    #[test]
    fn scheduled_triage_ignores_newer_non_scheduled_runs() {
        let response = serde_json::to_vec(&json!({
            "workflow_runs": [
                {
                    "id": 1,
                    "status": "completed",
                    "conclusion": "failure",
                    "event": "push",
                    "created_at": now().to_rfc3339()
                },
                {
                    "id": 2,
                    "status": "completed",
                    "conclusion": "success",
                    "event": "schedule",
                    "created_at": now().to_rfc3339()
                }
            ]
        }))
        .unwrap();

        let run =
            find_failed_scheduled_run_with("runs?per_page=100", |_| Ok(response.clone())).unwrap();

        assert_eq!(run, None);
    }

    #[test]
    fn schedule_trigger_detection_only_matches_top_level_on_block() {
        assert!(has_schedule_trigger(
            "name: Scheduled\non:\n  push:\n  schedule:\n    - cron: '0 0 * * *'\njobs:\n  test:\n"
        ));
        assert!(!has_schedule_trigger(
            "name: Push\non:\n  push:\njobs:\n  schedule:\n    runs-on: ubuntu-latest\n"
        ));
        assert!(!has_schedule_trigger(
            "name: Text\non: push\njobs:\n  test:\n    steps:\n      - run: echo schedule:\n"
        ));
    }

    #[test]
    fn scheduled_triage_uses_prior_failure_while_latest_run_is_in_flight() {
        let response = serde_json::to_vec(&json!({
            "workflow_runs": [
                {
                    "id": 10,
                    "status": "in_progress",
                    "conclusion": null,
                    "event": "schedule",
                    "created_at": now().to_rfc3339()
                },
                {
                    "id": 9,
                    "status": "completed",
                    "conclusion": "failure",
                    "event": "schedule",
                    "created_at": (now() - Duration::days(1)).to_rfc3339()
                }
            ]
        }))
        .unwrap();

        let run =
            find_failed_scheduled_run_with("runs?per_page=100", |_| Ok(response.clone())).unwrap();

        assert_eq!(run, Some(9));
    }

    #[test]
    fn lit_summary_uses_last_failure_section_and_strips_timestamps() {
        let log = "\
2026-09-23T10:00:00.000Z Failed Tests (1):\n\
2026-09-23T10:00:00.001Z   Suite :: old.test\n\
2026-09-23T10:01:00.000Z Failed Tests (2):\n\
2026-09-23T10:01:00.001Z   Suite :: one.test\n\
2026-09-23T10:01:00.002Z   Suite :: two.test\n\
2026-09-23T10:01:00.003Z ##[error]Process completed\n";

        assert_eq!(
            extract_lit_summary(log).as_deref(),
            Some("Failed Tests (2):\n  Suite :: one.test\n  Suite :: two.test")
        );
    }

    #[test]
    fn lit_summary_includes_failed_and_unexpectedly_passed_tests() {
        let log = "\
2026-09-23T12:00:00Z Testing Time: 1.23s
2026-09-23T12:00:01Z Unexpectedly Passed Tests (1):
2026-09-23T12:00:02Z   Suite :: unexpected.test
2026-09-23T12:00:03Z
2026-09-23T12:00:04Z Failed Tests (2):
2026-09-23T12:00:05Z   Suite :: failed-one.test
2026-09-23T12:00:06Z   Suite :: failed-two.test
2026-09-23T12:00:07Z ##[error]Process completed with exit code 1.";

        assert_eq!(
            extract_lit_summary(log).as_deref(),
            Some(
                "Unexpectedly Passed Tests (1):\n  Suite :: unexpected.test\n\nFailed Tests (2):\n  Suite :: failed-one.test\n  Suite :: failed-two.test"
            )
        );
    }

    #[test]
    fn lit_summary_includes_unexpectedly_passed_tests_without_failures() {
        let log = "\
Testing Time: 1.23s
Unexpectedly Passed Tests (1):
  Suite :: unexpected.test
##[error]Process completed with exit code 1.";

        assert_eq!(
            extract_lit_summary(log).as_deref(),
            Some("Unexpectedly Passed Tests (1):\n  Suite :: unexpected.test")
        );
    }

    #[test]
    fn lit_test_names_use_summary_sections_and_detailed_result_fallbacks() {
        let log = "\
Testing Time: 1.23s
Unexpectedly Passed Tests (1):
  Suite :: unexpected.test
Failed Tests (1):
  Suite :: failed.test
FAIL: Suite :: fallback-failed.test (1 of 20)
XPASS: Suite :: fallback-unexpected.test (2 of 20)
##[error]Process completed with exit code 1.";

        let (failed, unexpectedly_passed) = extract_lit_test_names(log);

        assert_eq!(failed, ["failed.test", "fallback-failed.test"]);
        assert_eq!(
            unexpectedly_passed,
            ["fallback-unexpected.test", "unexpected.test"]
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
