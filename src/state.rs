use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{github::Workflow, repository::Repository};

const CURRENT_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ViewState {
    version: u32,
    pub repository: Repository,
    pub workflow_ids: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct StoredViewState {
    version: u32,
    repository: Repository,
    #[serde(default)]
    workflow_ids: Vec<u64>,
    #[serde(default)]
    workflows: Vec<StoredWorkflow>,
}

#[derive(Debug, Deserialize)]
struct StoredWorkflow {
    id: u64,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not read state file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("state file {path} is not valid JSON: {source}")]
    Deserialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("state file version {found} is not supported (expected {CURRENT_VERSION})")]
    UnsupportedVersion { found: u32 },
    #[error("could not serialize view state: {0}")]
    Serialize(serde_json::Error),
    #[error("could not write state file {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not refresh saved workflows: {0}")]
    Refresh(#[from] crate::github::Error),
}

impl ViewState {
    pub fn new(repository: Repository, workflows: &[Workflow]) -> Self {
        Self {
            version: CURRENT_VERSION,
            repository,
            workflow_ids: workflows.iter().map(|workflow| workflow.id).collect(),
        }
    }

    pub fn resolve_workflows(&self, workflows: Vec<Workflow>) -> Vec<Workflow> {
        let mut workflows_by_id: HashMap<_, _> = workflows
            .into_iter()
            .map(|workflow| (workflow.id, workflow))
            .collect();
        self.workflow_ids
            .iter()
            .filter_map(|id| workflows_by_id.remove(id))
            .collect()
    }

    pub fn load(path: &Path) -> Result<Self, Error> {
        let contents = fs::read(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
        let stored: StoredViewState =
            serde_json::from_slice(&contents).map_err(|source| Error::Deserialize {
                path: path.to_owned(),
                source,
            })?;
        if stored.version != CURRENT_VERSION {
            return Err(Error::UnsupportedVersion {
                found: stored.version,
            });
        }
        let workflow_ids = if stored.workflow_ids.is_empty() {
            stored
                .workflows
                .into_iter()
                .map(|workflow| workflow.id)
                .collect()
        } else {
            stored.workflow_ids
        };
        Ok(Self {
            version: stored.version,
            repository: stored.repository,
            workflow_ids,
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let mut contents = serde_json::to_vec_pretty(self).map_err(Error::Serialize)?;
        contents.push(b'\n');
        fs::write(path, contents).map_err(|source| Error::Write {
            path: path.to_owned(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> ViewState {
        let workflows = vec![
            Workflow {
                id: 42,
                name: "Build".to_owned(),
                path: ".github/workflows/build.yml".to_owned(),
                state: "active".to_owned(),
                run_status: crate::github::RunStatus::Success,
                is_in_progress: true,
            },
            Workflow {
                id: 7,
                name: "Test".to_owned(),
                path: ".github/workflows/test.yml".to_owned(),
                state: "active".to_owned(),
                run_status: crate::github::RunStatus::Failure,
                is_in_progress: false,
            },
        ];
        ViewState::new("owner/repository".parse().unwrap(), &workflows)
    }

    #[test]
    fn view_state_round_trips_as_json() {
        let json = serde_json::to_vec(&state()).unwrap();
        let restored: StoredViewState = serde_json::from_slice(&json).unwrap();

        assert_eq!(restored.version, CURRENT_VERSION);
        assert_eq!(restored.repository, state().repository);
        assert_eq!(restored.workflow_ids, vec![42, 7]);
        assert!(restored.workflows.is_empty());
    }

    #[test]
    fn view_state_serializes_only_repository_and_workflow_ids() {
        let json = serde_json::to_value(state()).unwrap();

        assert_eq!(json["workflow_ids"], serde_json::json!([42, 7]));
        assert!(json.get("workflows").is_none());
        let serialized = json.to_string();
        assert!(!serialized.contains("Build"));
        assert!(!serialized.contains("build.yml"));
        assert!(!serialized.contains("run_status"));
    }

    #[test]
    fn resolve_workflows_uses_current_data_in_saved_order() {
        let saved = state();
        let workflows = vec![
            Workflow {
                id: 7,
                name: "Current Test".to_owned(),
                path: "current-test.yml".to_owned(),
                state: "active".to_owned(),
                run_status: crate::github::RunStatus::Success,
                is_in_progress: true,
            },
            Workflow {
                id: 42,
                name: "Current Build".to_owned(),
                path: "current-build.yml".to_owned(),
                state: "active".to_owned(),
                run_status: crate::github::RunStatus::Failure,
                is_in_progress: false,
            },
        ];

        let resolved = saved.resolve_workflows(workflows);

        assert_eq!(resolved[0].name, "Current Build");
        assert_eq!(resolved[1].name, "Current Test");
        assert!(resolved[1].is_in_progress);
    }

    #[test]
    fn view_state_saves_and_loads_file() {
        let path = std::env::temp_dir().join(format!(
            "gh-actui-state-round-trip-{}.json",
            std::process::id()
        ));

        state().save(&path).unwrap();
        let restored = ViewState::load(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(restored, state());
    }

    #[test]
    fn view_state_loads_ids_from_legacy_workflow_objects() {
        let path =
            std::env::temp_dir().join(format!("gh-actui-legacy-state-{}.json", std::process::id()));
        fs::write(
            &path,
            r#"{
                "version": 1,
                "repository": {"owner": "owner", "name": "repository"},
                "workflows": [
                    {"id": 42, "name": "Stale", "path": "stale.yml", "state": "active",
                     "run_status": "failure", "is_in_progress": true}
                ]
            }"#,
        )
        .unwrap();

        let restored = ViewState::load(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(restored.workflow_ids, vec![42]);
    }

    #[test]
    fn view_state_rejects_unsupported_version() {
        let path = std::env::temp_dir().join(format!(
            "gh-actui-unsupported-version-{}.json",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"{"version":2,"repository":{"owner":"o","name":"r"},"workflow_ids":[]}"#,
        )
        .unwrap();

        let error = ViewState::load(&path).unwrap_err();
        fs::remove_file(path).unwrap();

        assert!(matches!(error, Error::UnsupportedVersion { found: 2 }));
    }
}
