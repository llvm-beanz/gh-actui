use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{github::Workflow, repository::Repository};

const CURRENT_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewPane {
    pub workflow_ids: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_workflow_id: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ViewLayout {
    Pane {
        index: usize,
    },
    Split {
        direction: SplitDirection,
        #[serde(
            default = "default_split_ratio",
            skip_serializing_if = "is_equal_split"
        )]
        ratio: u16,
        first: Box<ViewLayout>,
        second: Box<ViewLayout>,
    },
}

pub const EQUAL_SPLIT_RATIO: u16 = 500;

fn default_split_ratio() -> u16 {
    EQUAL_SPLIT_RATIO
}

fn is_equal_split(ratio: &u16) -> bool {
    *ratio == EQUAL_SPLIT_RATIO
}

impl Default for ViewLayout {
    fn default() -> Self {
        Self::Pane { index: 0 }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewTab {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub workflow_ids: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_workflow_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_views: Vec<ViewPane>,
    #[serde(default, skip_serializing_if = "is_default_layout")]
    pub layout: ViewLayout,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub active_view: usize,
}

fn is_default_layout(layout: &ViewLayout) -> bool {
    layout == &ViewLayout::default()
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ViewState {
    version: u32,
    pub repository: Repository,
    pub workflow_ids: Vec<u64>,
    pub tabs: Vec<ViewTab>,
    pub active_tab: usize,
}

#[derive(Debug, Deserialize)]
struct StoredViewState {
    version: u32,
    repository: Repository,
    #[serde(default)]
    workflow_ids: Vec<u64>,
    #[serde(default)]
    workflows: Vec<StoredWorkflow>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    tabs: Vec<ViewTab>,
    #[serde(default)]
    active_tab: usize,
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
    #[error("state file contains an invalid split layout in tab {tab}")]
    InvalidLayout { tab: usize },
    #[error("could not serialize view state: {0}")]
    Serialize(serde_json::Error),
    #[error("could not write state file {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl ViewState {
    pub fn new(
        repository: Repository,
        workflows: &[Workflow],
        tabs: Vec<ViewTab>,
        active_tab: usize,
    ) -> Self {
        Self {
            version: CURRENT_VERSION,
            repository,
            workflow_ids: workflows.iter().map(|workflow| workflow.id).collect(),
            active_tab: active_tab.min(tabs.len().saturating_sub(1)),
            tabs,
        }
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
        let mut tabs = stored.tabs;
        if tabs.is_empty() {
            tabs.push(ViewTab {
                name: None,
                workflow_ids: workflow_ids.clone(),
                filter: stored.filter,
                sort: stored.sort,
                selected_workflow_id: None,
                additional_views: Vec::new(),
                layout: ViewLayout::default(),
                active_view: 0,
            });
        }
        for (index, tab) in tabs.iter().enumerate() {
            if !Self::valid_tab_layout(tab) {
                return Err(Error::InvalidLayout { tab: index + 1 });
            }
        }
        Ok(Self {
            version: stored.version,
            repository: stored.repository,
            workflow_ids,
            active_tab: stored.active_tab.min(tabs.len().saturating_sub(1)),
            tabs,
        })
    }

    fn valid_tab_layout(tab: &ViewTab) -> bool {
        let view_count = 1 + tab.additional_views.len();
        if tab.active_view >= view_count {
            return false;
        }
        fn collect(layout: &ViewLayout, indexes: &mut Vec<usize>) {
            match layout {
                ViewLayout::Pane { index } => indexes.push(*index),
                ViewLayout::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    if !(1..1000).contains(ratio) {
                        indexes.push(usize::MAX);
                        return;
                    }
                    collect(first, indexes);
                    collect(second, indexes);
                }
            }
        }
        let mut indexes = Vec::new();
        collect(&tab.layout, &mut indexes);
        indexes.sort_unstable();
        indexes == (0..view_count).collect::<Vec<_>>()
    }

    pub fn resolve_workflows(workflow_ids: &[u64], workflows: Vec<Workflow>) -> Vec<Workflow> {
        let mut workflows_by_id: HashMap<_, _> = workflows
            .into_iter()
            .map(|workflow| (workflow.id, workflow))
            .collect();
        workflow_ids
            .iter()
            .filter_map(|id| workflows_by_id.remove(id))
            .collect()
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
                run_metrics: crate::github::RunMetrics::default(),
            },
            Workflow {
                id: 7,
                name: "Test".to_owned(),
                path: ".github/workflows/test.yml".to_owned(),
                state: "active".to_owned(),
                run_status: crate::github::RunStatus::Failure,
                is_in_progress: false,
                run_metrics: crate::github::RunMetrics::default(),
            },
        ];
        ViewState::new(
            "owner/repository".parse().unwrap(),
            &workflows,
            vec![ViewTab {
                name: Some("Failures".to_owned()),
                workflow_ids: vec![42, 7],
                filter: Some("status:success".to_owned()),
                sort: Some("name:desc".to_owned()),
                selected_workflow_id: Some(7),
                additional_views: Vec::new(),
                layout: ViewLayout::default(),
                active_view: 0,
            }],
            0,
        )
    }

    #[test]
    fn view_state_round_trips_as_json() {
        let json = serde_json::to_vec(&state()).unwrap();
        let restored: StoredViewState = serde_json::from_slice(&json).unwrap();

        assert_eq!(restored.version, CURRENT_VERSION);
        assert_eq!(restored.repository, state().repository);
        assert_eq!(restored.workflow_ids, vec![42, 7]);
        assert_eq!(restored.tabs[0].filter.as_deref(), Some("status:success"));
        assert_eq!(restored.tabs[0].sort.as_deref(), Some("name:desc"));
        assert_eq!(restored.tabs[0].name.as_deref(), Some("Failures"));
        assert_eq!(restored.tabs[0].selected_workflow_id, Some(7));
        assert!(restored.workflows.is_empty());
    }

    #[test]
    fn view_state_excludes_fetched_workflow_data() {
        let json = serde_json::to_value(state()).unwrap();

        assert_eq!(json["workflow_ids"], serde_json::json!([42, 7]));
        assert_eq!(json["tabs"][0]["workflow_ids"], serde_json::json!([42, 7]));
        assert!(json.get("workflows").is_none());
        let serialized = json.to_string();
        assert!(!serialized.contains("Build"));
        assert!(!serialized.contains("build.yml"));
        assert!(!serialized.contains("run_status"));
    }

    #[test]
    fn view_state_round_trips_split_views() {
        let mut state = state();
        state.tabs[0].additional_views.push(ViewPane {
            workflow_ids: vec![7],
            filter: Some("status:failure".to_owned()),
            sort: Some("name".to_owned()),
            selected_workflow_id: Some(7),
        });
        state.tabs[0].layout = ViewLayout::Split {
            direction: SplitDirection::Vertical,
            ratio: 650,
            first: Box::new(ViewLayout::Pane { index: 0 }),
            second: Box::new(ViewLayout::Pane { index: 1 }),
        };
        state.tabs[0].active_view = 1;
        let path =
            std::env::temp_dir().join(format!("gh-actui-split-state-{}.json", std::process::id()));

        state.save(&path).unwrap();
        let restored = ViewState::load(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(restored.tabs, state.tabs);
    }

    #[test]
    fn split_layout_without_ratio_uses_equal_weight() {
        let layout: ViewLayout = serde_json::from_value(serde_json::json!({
            "type": "split",
            "direction": "horizontal",
            "first": { "type": "pane", "index": 0 },
            "second": { "type": "pane", "index": 1 }
        }))
        .unwrap();

        assert!(matches!(
            layout,
            ViewLayout::Split {
                ratio: EQUAL_SPLIT_RATIO,
                ..
            }
        ));
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
                run_metrics: crate::github::RunMetrics::default(),
            },
            Workflow {
                id: 42,
                name: "Current Build".to_owned(),
                path: "current-build.yml".to_owned(),
                state: "active".to_owned(),
                run_status: crate::github::RunStatus::Failure,
                is_in_progress: false,
                run_metrics: crate::github::RunMetrics::default(),
            },
        ];

        let resolved = ViewState::resolve_workflows(&saved.workflow_ids, workflows);

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
                ],
                "filter": "status:failure",
                "sort": "name:desc"
            }"#,
        )
        .unwrap();

        let restored = ViewState::load(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(restored.workflow_ids, vec![42]);
        assert_eq!(restored.tabs[0].workflow_ids, vec![42]);
        assert_eq!(restored.tabs[0].filter.as_deref(), Some("status:failure"));
        assert_eq!(restored.tabs[0].sort.as_deref(), Some("name:desc"));
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
