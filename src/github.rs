use std::{collections::BTreeMap, process::Command, sync::Mutex, thread};

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    cache::{CachedRun, CachedWorkflowRuns, RunCache, run_cache_key, run_cache_path},
    repository::Repository,
};

// GitHub's REST best practices advise against issuing many concurrent
// requests, so keep the fan-out modest while still hiding request latency.
pub const MAX_CONCURRENT_REQUESTS: usize = 4;
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

impl From<WorkflowRun> for CachedRun {
    fn from(run: WorkflowRun) -> Self {
        Self {
            id: run.id,
            status: run.status,
            conclusion: run.conclusion,
            event: run.event,
            created_at: run.created_at,
        }
    }
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
    #[error("GitHub API rate limit reached{0}")]
    RateLimited(String),
    #[error("GitHub CLI returned invalid Actions data: {0}")]
    InvalidResponse(#[from] serde_json::Error),
    #[error("a workflow status worker stopped unexpectedly")]
    StatusWorker,
}

pub trait WorkflowSource: Send + Sync {
    fn list_workflows(&self, repository: &Repository) -> Result<Vec<Workflow>, Error>;
    fn load_workflow_status(
        &self,
        _repository: &Repository,
        workflow: &Workflow,
    ) -> Result<Workflow, Error> {
        Ok(workflow.clone())
    }
    fn triage_workflows(
        &self,
        repository: &Repository,
        workflows: &[Workflow],
    ) -> Result<Vec<WorkflowTriage>, Error>;
    /// Persist any cached data gathered during a load.
    fn flush(&self) {}
}

pub struct GhWorkflowSource {
    cache: Mutex<RunCache>,
}

impl Default for GhWorkflowSource {
    fn default() -> Self {
        Self::new()
    }
}

impl GhWorkflowSource {
    pub fn new() -> Self {
        let cache = run_cache_path()
            .map(|path| RunCache::load(&path).unwrap_or_default())
            .unwrap_or_default();
        Self {
            cache: Mutex::new(cache),
        }
    }
}

impl WorkflowSource for GhWorkflowSource {
    fn list_workflows(&self, repository: &Repository) -> Result<Vec<Workflow>, Error> {
        let workflows_path = repository.workflows_api_path();
        let workflow_output = run_gh(&["api", "--paginate", "--slurp", &workflows_path])?;
        parse_workflows(&successful_stdout(workflow_output)?)
    }

    fn load_workflow_status(
        &self,
        repository: &Repository,
        workflow: &Workflow,
    ) -> Result<Workflow, Error> {
        let mut workflow = workflow.clone();
        let runs_path = repository.workflow_runs_api_path(workflow.id);
        let key = run_cache_key(&repository.to_string(), workflow.id);
        let cached = self
            .cache
            .lock()
            .map(|cache| cache.get(&key))
            .unwrap_or_default();

        let (summary, updated) =
            fetch_run_status_with(&runs_path, Utc::now(), &cached, |path, etag| {
                fetch_page_conditional(path, etag)
            })?;

        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(key, updated);
        }

        workflow.run_status = summary.run_status;
        workflow.is_in_progress = summary.is_in_progress;
        workflow.run_metrics = summary.run_metrics;
        Ok(workflow)
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

    fn flush(&self) {
        let (Some(path), Ok(cache)) = (run_cache_path(), self.cache.lock()) else {
            return;
        };
        let _ = cache.save(&path);
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

#[derive(Debug, Eq, PartialEq)]
enum PageResponse {
    NotModified,
    Modified { body: Vec<u8>, etag: Option<String> },
}

/// Request a page of workflow runs, reusing a cached validator when present.
///
/// GitHub does not charge the primary rate limit for a conditional request
/// that returns `304 Not Modified`, which makes this the cheapest way to poll
/// unchanged history.
fn fetch_page_conditional(path: &str, etag: Option<&str>) -> Result<PageResponse, Error> {
    let header = etag.map(|etag| format!("If-None-Match: {etag}"));
    let mut arguments = vec!["api", "-i", "-X", "GET", path];
    if let Some(header) = header.as_deref() {
        arguments.push("-H");
        arguments.push(header);
    }
    parse_page_response(run_gh(&arguments)?)
}

fn parse_page_response(output: GhOutput) -> Result<PageResponse, Error> {
    let (status_code, headers, body) = split_http_response(&output.stdout);
    match status_code {
        Some(304) => Ok(PageResponse::NotModified),
        Some(code) if (200..300).contains(&code) => Ok(PageResponse::Modified {
            body,
            etag: header_value(&headers, "etag"),
        }),
        Some(code) if code == 403 || code == 429 => Err(rate_limit_error(&headers, &output)),
        _ => Err(request_error(&output)),
    }
}

fn rate_limit_error(headers: &BTreeMap<String, String>, output: &GhOutput) -> Error {
    let remaining = header_value(headers, "x-ratelimit-remaining");
    // A 403 that is not a rate limit (for example a permissions problem)
    // should keep its original message.
    if remaining.as_deref() != Some("0") {
        let message = String::from_utf8_lossy(&output.stderr);
        if !message.to_lowercase().contains("rate limit") {
            return request_error(output);
        }
    }

    let resource = header_value(headers, "x-ratelimit-resource")
        .map(|resource| format!(" for {resource} requests"))
        .unwrap_or_default();
    let reset = header_value(headers, "retry-after")
        .and_then(|seconds| seconds.trim().parse::<i64>().ok())
        .map(|seconds| format!("; retry in {seconds}s"))
        .or_else(|| {
            let reset = header_value(headers, "x-ratelimit-reset")?
                .trim()
                .parse::<i64>()
                .ok()?;
            let reset = DateTime::from_timestamp(reset, 0)?;
            let minutes = (reset - Utc::now()).num_minutes().max(0);
            Some(format!("; resets in {minutes}m"))
        })
        .unwrap_or_default();
    Error::RateLimited(format!("{resource}{reset}"))
}

fn request_error(output: &GhOutput) -> Error {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Error::Request(if message.is_empty() {
        format!("gh exited with status {}", output.status)
    } else {
        message
    })
}

/// Split `gh api --include` output into a status code, headers, and body.
fn split_http_response(stdout: &[u8]) -> (Option<u16>, BTreeMap<String, String>, Vec<u8>) {
    let text = String::from_utf8_lossy(stdout);
    let (head, body) = match text.find("\r\n\r\n") {
        Some(index) => (&text[..index], &text[index + 4..]),
        None => match text.find("\n\n") {
            Some(index) => (&text[..index], &text[index + 2..]),
            None => (text.as_ref(), ""),
        },
    };

    let mut lines = head.lines();
    let status_code = lines.next().and_then(|line| {
        line.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
    });
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_lowercase(), value.trim().to_owned()))
        })
        .collect();

    (status_code, headers, body.as_bytes().to_vec())
}

fn header_value(headers: &BTreeMap<String, String>, name: &str) -> Option<String> {
    headers.get(name).cloned()
}

fn parse_workflows(response: &[u8]) -> Result<Vec<Workflow>, Error> {
    let pages: Vec<WorkflowPage> = serde_json::from_slice(response)?;
    Ok(pages
        .into_iter()
        .flat_map(|page| page.workflows)
        .filter(|workflow| workflow.state == "active")
        .collect())
}

/// Summarize a workflow's recent runs, paging only as far as the cache allows.
///
/// Pages are newest-first, so once a page contains nothing but runs already
/// cached as completed there is no newer history left to discover and paging
/// can stop. Runs that were still queued or in progress when cached are the
/// exception: they may have finished since, so paging continues until it has
/// covered them.
fn fetch_run_status_with(
    runs_path: &str,
    now: DateTime<Utc>,
    cached: &CachedWorkflowRuns,
    mut fetch_page: impl FnMut(&str, Option<&str>) -> Result<PageResponse, Error>,
) -> Result<(RunSummary, CachedWorkflowRuns), Error> {
    let cutoff = now - Duration::days(14);
    let oldest_unsettled = cached.oldest_unsettled();
    let mut fetched: Vec<CachedRun> = Vec::new();
    let mut etag = cached.etag.clone();
    let mut page_number = 1;
    let mut found_completed = false;

    loop {
        // GitHub's server-side `created` filter can return stale workflow-run
        // results for workflows with large histories. Fetch the unfiltered,
        // newest-first stream and enforce the time window locally instead.
        let page_path = format!("{runs_path}&page={page_number}");
        let validator = (page_number == 1).then_some(etag.as_deref()).flatten();
        let page = match fetch_page(&page_path, validator)? {
            PageResponse::NotModified => {
                // The first page is unchanged, so no run has started or
                // finished since the last refresh and the cache still holds
                // the complete picture.
                let entry = prune_runs(cached.runs.clone(), cutoff, etag);
                return Ok((summarize_runs(&entry.runs, now), entry));
            }
            PageResponse::Modified { body, etag: value } => {
                if page_number == 1 {
                    etag = value;
                }
                serde_json::from_slice::<WorkflowRunPage>(&body)?
            }
        };

        let runs: Vec<CachedRun> = page
            .workflow_runs
            .into_iter()
            .map(CachedRun::from)
            .collect();
        let page_len = runs.len();
        let reached_cutoff = runs.iter().any(|run| run.created_at < cutoff);
        found_completed |= runs.iter().any(CachedRun::is_completed);
        let covered_unsettled = runs
            .iter()
            .map(|run| run.created_at)
            .min()
            .zip(oldest_unsettled)
            .is_some_and(|(oldest_on_page, unsettled)| oldest_on_page <= unsettled);
        let reached_known_history = page_len > 0
            && runs
                .iter()
                .any(|run| run.id.is_some_and(|id| cached.contains(id)));

        fetched.extend(runs);

        if page_len < RUNS_PER_PAGE || (reached_cutoff && found_completed) {
            break;
        }
        // This page reached runs that are already cached, so every older run
        // is cached as well and there is nothing new left to discover.
        if reached_known_history && (oldest_unsettled.is_none() || covered_unsettled) {
            break;
        }
        page_number += 1;
    }

    let entry = prune_runs(merge_runs(cached.runs.clone(), fetched), cutoff, etag);
    Ok((summarize_runs(&entry.runs, now), entry))
}

/// Combine cached runs with freshly fetched ones, preferring fresh data.
fn merge_runs(cached: Vec<CachedRun>, fetched: Vec<CachedRun>) -> Vec<CachedRun> {
    let mut merged: Vec<CachedRun> = Vec::with_capacity(cached.len() + fetched.len());
    let mut seen = BTreeMap::new();
    for run in fetched.into_iter().chain(cached) {
        match run.id {
            Some(id) => {
                if seen.insert(id, ()).is_none() {
                    merged.push(run);
                }
            }
            // Runs without an identifier cannot be de-duplicated, so keep the
            // freshly fetched copy only.
            None => merged.push(run),
        }
    }
    merged
}

fn prune_runs(
    mut runs: Vec<CachedRun>,
    cutoff: DateTime<Utc>,
    etag: Option<String>,
) -> CachedWorkflowRuns {
    runs.sort_by_key(|run| std::cmp::Reverse(run.created_at));
    // Metrics only cover the last 14 days, but the status column reflects the
    // most recent completed run even when it is older than that window, so
    // that run has to survive pruning.
    let newest_completed = runs.iter().position(CachedRun::is_completed);
    let mut index = 0;
    runs.retain(|run| {
        let keep = run.created_at >= cutoff || Some(index) == newest_completed;
        index += 1;
        keep
    });
    CachedWorkflowRuns { etag, runs }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RunSummary {
    run_status: RunStatus,
    is_in_progress: bool,
    run_metrics: RunMetrics,
}

fn summarize_runs(runs: &[CachedRun], now: DateTime<Utc>) -> RunSummary {
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

    fn run(status: &str, conclusion: Option<&str>, age: Duration) -> CachedRun {
        CachedRun {
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

        let summary = summarize_runs(&runs, now());

        assert_eq!(summary.run_status, RunStatus::Success);
    }

    #[test]
    fn summarize_runs_marks_any_in_progress_run() {
        let runs = vec![
            run("in_progress", None, Duration::minutes(5)),
            run("completed", Some("success"), Duration::hours(1)),
        ];

        let summary = summarize_runs(&runs, now());

        assert!(summary.is_in_progress);
        assert_eq!(summary.run_status, RunStatus::Success);
    }

    #[test]
    fn summarize_runs_uses_other_without_completed_run() {
        let summary = summarize_runs(&[run("queued", None, Duration::minutes(5))], now());

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

        let summary = summarize_runs(&runs, now());

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

        let (summary, _) = fetch_run_status_with(
            "repos/owner/repo/actions/workflows/42/runs?per_page=100",
            now(),
            &CachedWorkflowRuns::default(),
            |path, _| {
                requested_paths.push(path.to_owned());
                Ok(PageResponse::Modified {
                    body: responses.pop_front().unwrap(),
                    etag: None,
                })
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

        let (summary, _) = fetch_run_status_with(
            "runs?per_page=100",
            now(),
            &CachedWorkflowRuns::default(),
            |_, _| {
                request_count += 1;
                Ok(PageResponse::Modified {
                    body: response.clone(),
                    etag: None,
                })
            },
        )
        .unwrap();

        assert_eq!(request_count, 1);
        assert_eq!(summary.run_metrics.last_14_days.total, 99);
    }

    #[test]
    fn fetch_run_status_reuses_cache_when_first_page_is_unchanged() {
        let cached = CachedWorkflowRuns {
            etag: Some("W/\"abc\"".to_owned()),
            runs: vec![
                run("completed", Some("success"), Duration::hours(1)),
                run("completed", Some("failure"), Duration::days(2)),
            ],
        };
        let mut validators = Vec::new();

        let (summary, updated) =
            fetch_run_status_with("runs?per_page=100", now(), &cached, |_, etag| {
                validators.push(etag.map(str::to_owned));
                Ok(PageResponse::NotModified)
            })
            .unwrap();

        assert_eq!(validators, [Some("W/\"abc\"".to_owned())]);
        assert_eq!(summary.run_status, RunStatus::Success);
        assert_eq!(summary.run_metrics.last_7_days.total, 2);
        assert_eq!(updated.etag.as_deref(), Some("W/\"abc\""));
    }

    #[test]
    fn persisted_cache_supplies_the_validator_for_the_next_process() {
        let directory =
            std::env::temp_dir().join(format!("gh-actui-validator-{}", std::process::id()));
        let path = directory.join("run-cache.json");
        let key = run_cache_key("owner/repository", 7);
        let mut cache = RunCache::new();
        cache.insert(
            key.clone(),
            CachedWorkflowRuns {
                etag: Some("W/\"abc\"".to_owned()),
                runs: vec![run("completed", Some("success"), Duration::hours(1))],
            },
        );
        cache.save(&path).unwrap();

        // A later process only ever sees the cache through the file, so the
        // validator has to survive the full save/load round trip.
        let reloaded = RunCache::load(&path).unwrap();
        let cached = reloaded.get(&key);
        let mut validators = Vec::new();
        let (summary, _) = fetch_run_status_with("runs?per_page=100", now(), &cached, |_, etag| {
            validators.push(etag.map(str::to_owned));
            Ok(PageResponse::NotModified)
        })
        .unwrap();

        assert_eq!(validators, [Some("W/\"abc\"".to_owned())]);
        assert_eq!(summary.run_status, RunStatus::Success);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn fetch_run_status_stops_at_history_already_cached_as_completed() {
        let cached_runs = (0..RUNS_PER_PAGE)
            .map(|index| CachedRun {
                id: Some(index as u64),
                status: Some("completed".to_owned()),
                conclusion: Some("success".to_owned()),
                event: None,
                created_at: now() - Duration::hours(index as i64 + 1),
            })
            .collect::<Vec<_>>();
        let page = (0..RUNS_PER_PAGE)
            .map(|index| {
                json!({
                    "id": index,
                    "status": "completed",
                    "conclusion": "success",
                    "created_at": (now() - Duration::hours(index as i64 + 1)).to_rfc3339()
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({"workflow_runs": page})).unwrap();
        let cached = CachedWorkflowRuns {
            etag: Some("W/\"stale\"".to_owned()),
            runs: cached_runs,
        };
        let mut request_count = 0;

        let (summary, updated) =
            fetch_run_status_with("runs?per_page=100", now(), &cached, |_, _| {
                request_count += 1;
                Ok(PageResponse::Modified {
                    body: body.clone(),
                    etag: Some("W/\"fresh\"".to_owned()),
                })
            })
            .unwrap();

        // A full page of runs already cached as completed means there is no
        // newer history to discover, so paging stops immediately.
        assert_eq!(request_count, 1);
        assert_eq!(summary.run_metrics.last_14_days.total, RUNS_PER_PAGE as u32);
        assert_eq!(updated.etag.as_deref(), Some("W/\"fresh\""));
    }

    #[test]
    fn fetch_run_status_stops_once_a_page_reaches_known_runs() {
        // A new run pushes older history down, but the rest of the page is
        // already cached, so one request is enough.
        let cached_runs = (1..RUNS_PER_PAGE)
            .map(|index| CachedRun {
                id: Some(index as u64),
                status: Some("completed".to_owned()),
                conclusion: Some("success".to_owned()),
                event: None,
                created_at: now() - Duration::hours(index as i64 + 1),
            })
            .collect::<Vec<_>>();
        let mut page = vec![json!({
            "id": 9_001,
            "status": "completed",
            "conclusion": "failure",
            "created_at": (now() - Duration::minutes(2)).to_rfc3339()
        })];
        page.extend((1..RUNS_PER_PAGE).map(|index| {
            json!({
                "id": index,
                "status": "completed",
                "conclusion": "success",
                "created_at": (now() - Duration::hours(index as i64 + 1)).to_rfc3339()
            })
        }));
        let body = serde_json::to_vec(&json!({"workflow_runs": page})).unwrap();
        let cached = CachedWorkflowRuns {
            etag: None,
            runs: cached_runs,
        };
        let mut request_count = 0;

        let (summary, updated) =
            fetch_run_status_with("runs?per_page=100", now(), &cached, |_, _| {
                request_count += 1;
                Ok(PageResponse::Modified {
                    body: body.clone(),
                    etag: None,
                })
            })
            .unwrap();

        assert_eq!(request_count, 1);
        assert_eq!(summary.run_status, RunStatus::Failure);
        assert_eq!(updated.runs.len(), RUNS_PER_PAGE);
        assert_eq!(updated.runs[0].id, Some(9_001));
    }

    #[test]
    fn fetch_run_status_keeps_paging_past_cached_runs_that_were_unsettled() {
        let cached = CachedWorkflowRuns {
            etag: None,
            runs: vec![
                CachedRun {
                    id: Some(1),
                    status: Some("completed".to_owned()),
                    conclusion: Some("success".to_owned()),
                    event: None,
                    created_at: now() - Duration::hours(1),
                },
                // Still running when it was cached, so a refresh must reach it
                // again to notice that it finished.
                CachedRun {
                    id: Some(2),
                    status: Some("in_progress".to_owned()),
                    conclusion: None,
                    event: None,
                    created_at: now() - Duration::days(3),
                },
            ],
        };
        let first_page = (0..RUNS_PER_PAGE)
            .map(|_| {
                json!({
                    "id": 1,
                    "status": "completed",
                    "conclusion": "success",
                    "created_at": (now() - Duration::hours(1)).to_rfc3339()
                })
            })
            .collect::<Vec<_>>();
        let second_page = json!([{
            "id": 2,
            "status": "completed",
            "conclusion": "failure",
            "created_at": (now() - Duration::days(3)).to_rfc3339()
        }]);
        let mut responses = VecDeque::from([
            serde_json::to_vec(&json!({"workflow_runs": first_page})).unwrap(),
            serde_json::to_vec(&json!({"workflow_runs": second_page})).unwrap(),
        ]);
        let mut request_count = 0;

        let (_, updated) = fetch_run_status_with("runs?per_page=100", now(), &cached, |_, _| {
            request_count += 1;
            Ok(PageResponse::Modified {
                body: responses.pop_front().unwrap(),
                etag: None,
            })
        })
        .unwrap();

        assert_eq!(request_count, 2);
        let refreshed = updated.runs.iter().find(|run| run.id == Some(2)).unwrap();
        assert_eq!(refreshed.status.as_deref(), Some("completed"));
        assert_eq!(refreshed.conclusion.as_deref(), Some("failure"));
    }

    #[test]
    fn fetch_run_status_merges_new_runs_ahead_of_cached_history() {
        let cached = CachedWorkflowRuns {
            etag: None,
            runs: vec![CachedRun {
                id: Some(1),
                status: Some("completed".to_owned()),
                conclusion: Some("success".to_owned()),
                event: None,
                created_at: now() - Duration::days(1),
            }],
        };
        let page = json!([{
            "id": 2,
            "status": "completed",
            "conclusion": "failure",
            "created_at": (now() - Duration::minutes(5)).to_rfc3339()
        }]);
        let body = serde_json::to_vec(&json!({"workflow_runs": page})).unwrap();

        let (summary, updated) =
            fetch_run_status_with("runs?per_page=100", now(), &cached, |_, _| {
                Ok(PageResponse::Modified {
                    body: body.clone(),
                    etag: None,
                })
            })
            .unwrap();

        assert_eq!(updated.runs.len(), 2);
        assert_eq!(updated.runs[0].id, Some(2));
        assert_eq!(summary.run_status, RunStatus::Failure);
        assert_eq!(summary.run_metrics.last_7_days.total, 2);
    }

    #[test]
    fn fetch_run_status_keeps_newest_completed_run_older_than_the_metrics_window() {
        // The status column shows the last completed run even when the only
        // completed run predates the 14-day metrics window.
        let cached = CachedWorkflowRuns {
            etag: Some("W/\"abc\"".to_owned()),
            runs: vec![
                run("queued", None, Duration::hours(1)),
                run("completed", Some("failure"), Duration::days(20)),
                run("completed", Some("success"), Duration::days(30)),
            ],
        };

        let (summary, updated) =
            fetch_run_status_with("runs?per_page=100", now(), &cached, |_, _| {
                Ok(PageResponse::NotModified)
            })
            .unwrap();

        assert_eq!(summary.run_status, RunStatus::Failure);
        assert_eq!(summary.run_metrics.last_14_days.total, 0);
        assert_eq!(updated.runs.len(), 2);
        assert!(
            updated
                .runs
                .iter()
                .all(|run| run.conclusion.as_deref() != Some("success"))
        );
    }

    #[test]
    fn fetch_run_status_drops_cached_runs_older_than_the_window() {
        let cached = CachedWorkflowRuns {
            etag: Some("W/\"abc\"".to_owned()),
            runs: vec![
                run("completed", Some("success"), Duration::hours(1)),
                run("completed", Some("success"), Duration::days(20)),
            ],
        };

        let (summary, updated) =
            fetch_run_status_with("runs?per_page=100", now(), &cached, |_, _| {
                Ok(PageResponse::NotModified)
            })
            .unwrap();

        assert_eq!(updated.runs.len(), 1);
        assert_eq!(summary.run_metrics.last_14_days.total, 1);
    }

    #[test]
    fn fetch_run_status_only_sends_a_validator_for_the_first_page() {
        let cached = CachedWorkflowRuns {
            etag: Some("W/\"abc\"".to_owned()),
            runs: Vec::new(),
        };
        let first_page = (0..RUNS_PER_PAGE)
            .map(|_| {
                json!({
                    "status": "queued",
                    "conclusion": null,
                    "created_at": (now() - Duration::hours(1)).to_rfc3339()
                })
            })
            .collect::<Vec<_>>();
        let second_page = json!([{
            "status": "completed",
            "conclusion": "success",
            "created_at": (now() - Duration::days(20)).to_rfc3339()
        }]);
        let mut responses = VecDeque::from([
            serde_json::to_vec(&json!({"workflow_runs": first_page})).unwrap(),
            serde_json::to_vec(&json!({"workflow_runs": second_page})).unwrap(),
        ]);
        let mut validators = Vec::new();

        fetch_run_status_with("runs?per_page=100", now(), &cached, |_, etag| {
            validators.push(etag.map(str::to_owned));
            Ok(PageResponse::Modified {
                body: responses.pop_front().unwrap(),
                etag: None,
            })
        })
        .unwrap();

        assert_eq!(validators, [Some("W/\"abc\"".to_owned()), None]);
    }

    #[test]
    fn split_http_response_separates_status_headers_and_body() {
        let raw =
            b"HTTP/2.0 200 OK\r\nETag: W/\"abc\"\r\nX-RateLimit-Remaining: 42\r\n\r\n{\"ok\":true}";

        let (status, headers, body) = split_http_response(raw);

        assert_eq!(status, Some(200));
        assert_eq!(header_value(&headers, "etag").as_deref(), Some("W/\"abc\""));
        assert_eq!(
            header_value(&headers, "x-ratelimit-remaining").as_deref(),
            Some("42")
        );
        assert_eq!(body, b"{\"ok\":true}");
    }

    #[test]
    fn split_http_response_handles_newline_only_separators() {
        let raw = b"HTTP/1.1 304 Not Modified\nETag: W/\"abc\"\n\n";

        let (status, headers, _) = split_http_response(raw);

        assert_eq!(status, Some(304));
        assert_eq!(header_value(&headers, "etag").as_deref(), Some("W/\"abc\""));
    }

    #[test]
    fn parse_page_response_reports_not_modified_even_though_gh_exits_nonzero() {
        // `gh api` treats 304 as a failure, but for a conditional request it
        // means the cached copy is still current.
        let output = GhOutput {
            success: false,
            status: "exit code: 1".to_owned(),
            stdout: b"HTTP/2.0 304 Not Modified\r\nETag: W/\"abc\"\r\n\r\n".to_vec(),
            stderr: b"gh: HTTP 304".to_vec(),
        };

        assert_eq!(
            parse_page_response(output).unwrap(),
            PageResponse::NotModified
        );
    }

    #[test]
    fn parse_page_response_reports_rate_limit_with_reset_guidance() {
        let reset = (now() + Duration::minutes(30)).timestamp();
        let output = GhOutput {
            success: false,
            status: "exit code: 1".to_owned(),
            stdout: format!(
                "HTTP/2.0 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Resource: core\r\nX-RateLimit-Reset: {reset}\r\n\r\n{{}}"
            )
            .into_bytes(),
            stderr: b"gh: API rate limit exceeded for user ID 1 (HTTP 403)".to_vec(),
        };

        let error = parse_page_response(output).unwrap_err();

        let Error::RateLimited(detail) = error else {
            panic!("expected a rate limit error, got {error:?}");
        };
        assert!(detail.contains("for core requests"), "{detail}");
    }

    #[test]
    fn parse_page_response_keeps_non_rate_limited_forbidden_errors() {
        let output = GhOutput {
            success: false,
            status: "exit code: 1".to_owned(),
            stdout: b"HTTP/2.0 403 Forbidden\r\nX-RateLimit-Remaining: 4999\r\n\r\n{}".to_vec(),
            stderr: b"gh: Resource not accessible by integration (HTTP 403)".to_vec(),
        };

        let error = parse_page_response(output).unwrap_err();

        assert!(
            matches!(error, Error::Request(message) if message.contains("not accessible")),
            "unexpected error variant"
        );
    }

    #[test]
    fn parse_page_response_prefers_retry_after_header() {
        let output = GhOutput {
            success: false,
            status: "exit code: 1".to_owned(),
            stdout: b"HTTP/2.0 429 Too Many Requests\r\nRetry-After: 45\r\nX-RateLimit-Remaining: 0\r\n\r\n{}".to_vec(),
            stderr: b"gh: You have exceeded a secondary rate limit (HTTP 429)".to_vec(),
        };

        let error = parse_page_response(output).unwrap_err();

        let Error::RateLimited(detail) = error else {
            panic!("expected a rate limit error, got {error:?}");
        };
        assert!(detail.contains("retry in 45s"), "{detail}");
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
