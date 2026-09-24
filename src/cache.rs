use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 1;

/// A single workflow run retained between refreshes.
///
/// Completed runs never change, so caching them lets a refresh stop paging as
/// soon as it reaches history it has already seen.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedRun {
    pub id: Option<u64>,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub event: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl CachedRun {
    pub fn is_completed(&self) -> bool {
        self.status.as_deref() == Some("completed")
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedWorkflowRuns {
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub runs: Vec<CachedRun>,
}

impl CachedWorkflowRuns {
    /// The creation time of the oldest run that may still change.
    ///
    /// Refreshes must keep paging past cached history until they have covered
    /// every run that was still queued or in progress when it was cached.
    pub fn oldest_unsettled(&self) -> Option<DateTime<Utc>> {
        self.runs
            .iter()
            .filter(|run| !run.is_completed())
            .map(|run| run.created_at)
            .min()
    }

    /// Whether a run is already cached, regardless of how it finished.
    ///
    /// Run pages arrive newest-first, so reaching a run that is already cached
    /// means every older run is cached too and paging can stop.
    pub fn contains(&self, id: u64) -> bool {
        self.runs.iter().any(|run| run.id == Some(id))
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RunCache {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, CachedWorkflowRuns>,
}

impl Default for RunCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RunCache {
    pub fn new() -> Self {
        Self {
            version: CACHE_VERSION,
            entries: BTreeMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> CachedWorkflowRuns {
        self.entries.get(key).cloned().unwrap_or_default()
    }

    pub fn insert(&mut self, key: String, entry: CachedWorkflowRuns) {
        self.entries.insert(key, entry);
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(error) => return Err(error),
        };
        let cache: Self = match serde_json::from_str(&contents) {
            Ok(cache) => cache,
            // A corrupt or older cache is a performance hint, never required
            // for correctness, so start over instead of failing the load.
            Err(_) => return Ok(Self::new()),
        };
        if cache.version != CACHE_VERSION {
            return Ok(Self::new());
        }
        Ok(cache)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string(self)?)
    }
}

pub fn run_cache_key(repository: &str, workflow_id: u64) -> String {
    format!("{repository}#{workflow_id}")
}

pub fn run_cache_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("GH_ACTUI_CACHE").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if cfg!(windows) {
        return env::var_os("LOCALAPPDATA")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join("gh-actui").join("run-cache.json"));
    }
    env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("gh-actui").join("run-cache.json"))
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .map(|path| path.join(".cache").join("gh-actui").join("run-cache.json"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn run(id: u64, status: &str, age: Duration) -> CachedRun {
        CachedRun {
            id: Some(id),
            status: Some(status.to_owned()),
            conclusion: Some("success".to_owned()),
            event: Some("schedule".to_owned()),
            created_at: Utc::now() - age,
        }
    }

    #[test]
    fn oldest_unsettled_ignores_completed_runs() {
        let entry = CachedWorkflowRuns {
            etag: None,
            runs: vec![
                run(1, "completed", Duration::hours(1)),
                run(2, "in_progress", Duration::hours(2)),
                run(3, "completed", Duration::hours(9)),
            ],
        };

        assert_eq!(entry.oldest_unsettled(), Some(entry.runs[1].created_at));
    }

    #[test]
    fn oldest_unsettled_is_none_when_all_runs_are_completed() {
        let entry = CachedWorkflowRuns {
            etag: None,
            runs: vec![run(1, "completed", Duration::hours(1))],
        };

        assert_eq!(entry.oldest_unsettled(), None);
    }

    #[test]
    fn contains_matches_cached_runs_by_id_regardless_of_status() {
        let entry = CachedWorkflowRuns {
            etag: None,
            runs: vec![
                run(1, "completed", Duration::hours(1)),
                run(2, "queued", Duration::hours(2)),
            ],
        };

        assert!(entry.contains(1));
        assert!(entry.contains(2));
        assert!(!entry.contains(3));
    }

    #[test]
    fn cache_round_trips_through_a_file() {
        let directory = env::temp_dir().join(format!("gh-actui-cache-{}", std::process::id()));
        let path = directory.join("run-cache.json");
        let mut cache = RunCache::new();
        cache.insert(
            run_cache_key("owner/repository", 7),
            CachedWorkflowRuns {
                etag: Some("W/\"abc\"".to_owned()),
                runs: vec![run(1, "completed", Duration::hours(1))],
            },
        );
        cache.save(&path).unwrap();

        let loaded = RunCache::load(&path).unwrap();
        let entry = loaded.get(&run_cache_key("owner/repository", 7));

        assert_eq!(entry.etag.as_deref(), Some("W/\"abc\""));
        assert_eq!(entry.runs.len(), 1);
        fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn default_cache_uses_the_current_version_so_it_survives_a_save() {
        let directory = env::temp_dir().join(format!("gh-actui-cache-ver-{}", std::process::id()));
        let path = directory.join("run-cache.json");
        let mut cache = RunCache::default();
        cache.insert(
            run_cache_key("owner/repository", 1),
            CachedWorkflowRuns::default(),
        );
        cache.save(&path).unwrap();

        assert!(!RunCache::load(&path).unwrap().entries.is_empty());
        fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn load_returns_empty_cache_for_missing_file() {
        let path = env::temp_dir().join("gh-actui-cache-missing-file.json");
        fs::remove_file(&path).ok();

        let cache = RunCache::load(&path).unwrap();

        assert!(cache.entries.is_empty());
    }

    #[test]
    fn load_discards_corrupt_and_outdated_cache_files() {
        let directory = env::temp_dir().join(format!("gh-actui-cache-bad-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let corrupt = directory.join("corrupt.json");
        fs::write(&corrupt, "not json").unwrap();
        let outdated = directory.join("outdated.json");
        fs::write(&outdated, r#"{"version":0,"entries":{"a":{"runs":[]}}}"#).unwrap();

        assert!(RunCache::load(&corrupt).unwrap().entries.is_empty());
        assert!(RunCache::load(&outdated).unwrap().entries.is_empty());
        fs::remove_dir_all(&directory).ok();
    }
}
