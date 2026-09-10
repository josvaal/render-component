//! Selection state shared by the API, the watcher and the TUI (C07/C09/C11).

use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::sync::{RwLock, watch};

use crate::pipeline::Strategy;

#[derive(Debug, Clone, serde::Serialize)]
pub struct CurrentSelection {
    pub component: String,
    pub root: String,
    pub strategy: String,
    /// Bumped on every applied change; the host page reloads when it changes.
    pub revision: u64,
    /// Last validation error, surfaced on the page and in the status bar (C10).
    pub error: Option<String>,
    /// Required signal inputs — the host mounts with `{}` placeholders (C28).
    #[serde(rename = "requiredInputs")]
    pub required_inputs: Vec<String>,
    /// Fictional typed values for the component inputs (C32).
    #[serde(rename = "inputValues")]
    pub input_values: serde_json::Map<String, Value>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SelectOutcome {
    /// A different component was applied; page will hot-swap.
    Updated(u64),
    /// Same component already being served: no rebuild, no event (C09).
    Unchanged,
}

pub struct AppState {
    inner: RwLock<CurrentSelection>,
    revision_tx: watch::Sender<u64>,
}

impl AppState {
    pub fn new(
        component: &Path,
        root: &Path,
        strategy: Strategy,
        required_inputs: Vec<String>,
        input_values: serde_json::Map<String, Value>,
    ) -> Self {
        let current = CurrentSelection {
            component: component.display().to_string(),
            root: root.display().to_string(),
            strategy: strategy.as_str().to_string(),
            revision: 1,
            error: None,
            required_inputs,
            input_values,
        };
        let (revision_tx, _) = watch::channel(current.revision);
        AppState {
            inner: RwLock::new(current),
            revision_tx,
        }
    }

    /// Subscribe to revision changes (SSE hot-swap signal, C07).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision_tx.subscribe()
    }

    pub async fn current(&self) -> CurrentSelection {
        self.inner.read().await.clone()
    }

    /// Apply a new selection. Validation happens in the caller (`pipeline::validate`).
    /// Calls are sequential (single watcher task / API handler), so under a rapid
    /// A→B race the last call wins and the final state is B (C11).
    pub async fn select(
        &self,
        component: &Path,
        root: &Path,
        strategy: Strategy,
        input_values: serde_json::Map<String, Value>,
    ) -> SelectOutcome {
        let mut guard = self.inner.write().await;
        let same = guard.component == component.display().to_string()
            && guard.root == root.display().to_string();
        if same {
            return SelectOutcome::Unchanged;
        }
        guard.component = component.display().to_string();
        guard.root = root.display().to_string();
        guard.strategy = strategy.as_str().to_string();
        guard.input_values = input_values;
        guard.error = None;
        guard.revision += 1;
        let revision = guard.revision;
        let _ = self.revision_tx.send(revision);
        SelectOutcome::Updated(revision)
    }

    /// Record a validation failure without changing the served component (C10).
    pub async fn report_error(&self, message: String) {
        let mut guard = self.inner.write().await;
        guard.error = Some(message);
        let _ = self.revision_tx.send(guard.revision);
    }
}

/// The selection IPC file written by the TUI and read by the watcher (see plan.md).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SelectionFile {
    pub component: PathBuf,
    pub root: PathBuf,
}

pub fn write_selection_file(path: &Path, component: &Path, root: &Path) -> std::io::Result<()> {
    let payload = SelectionFile {
        component: component.to_path_buf(),
        root: root.to_path_buf(),
    };
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(&payload).unwrap())?;
    std::fs::rename(&tmp, path)
}

pub fn read_selection_file(path: &Path) -> Option<SelectionFile> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokio_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn initial_revision_is_one() {
        let rt = tokio_runtime();
        rt.block_on(async {
            let state = AppState::new(
                Path::new("/a/x.component.ts"),
                Path::new("/a"),
                Strategy::Standalone,
                vec![],
                serde_json::Map::new(),
            );
            let current = state.current().await;
            assert_eq!(current.revision, 1);
            assert_eq!(current.strategy, "standalone");
        });
    }

    #[test]
    fn selecting_same_component_is_unchanged() {
        let rt = tokio_runtime();
        rt.block_on(async {
            let state = AppState::new(
                Path::new("/a/x.component.ts"),
                Path::new("/a"),
                Strategy::Standalone,
                vec![],
                serde_json::Map::new(),
            );
            let out = state
                .select(
                    Path::new("/a/x.component.ts"),
                    Path::new("/a"),
                    Strategy::Standalone,
                    serde_json::Map::new(),
                )
                .await;
            assert_eq!(out, SelectOutcome::Unchanged);
            assert_eq!(state.current().await.revision, 1);
        });
    }

    #[test]
    fn selecting_different_component_bumps_revision() {
        let rt = tokio_runtime();
        rt.block_on(async {
            let state = AppState::new(
                Path::new("/a/x.component.ts"),
                Path::new("/a"),
                Strategy::Standalone,
                vec![],
                serde_json::Map::new(),
            );
            let out = state
                .select(
                    Path::new("/a/y.component.ts"),
                    Path::new("/a"),
                    Strategy::Module,
                    serde_json::Map::new(),
                )
                .await;
            assert_eq!(out, SelectOutcome::Updated(2));
            let current = state.current().await;
            assert_eq!(current.component, "/a/y.component.ts");
            assert_eq!(current.strategy, "module");
        });
    }

    #[test]
    fn rapid_selection_race_last_wins() {
        let rt = tokio_runtime();
        rt.block_on(async {
            let state = AppState::new(
                Path::new("/a/x.component.ts"),
                Path::new("/a"),
                Strategy::Standalone,
                vec![],
                serde_json::Map::new(),
            );
            // Two rapid selections: A→B→C style; both apply in order, last call is final.
            state
                .select(
                    Path::new("/a/b.component.ts"),
                    Path::new("/a"),
                    Strategy::Standalone,
                    serde_json::Map::new(),
                )
                .await;
            state
                .select(
                    Path::new("/a/c.component.ts"),
                    Path::new("/a"),
                    Strategy::Standalone,
                    serde_json::Map::new(),
                )
                .await;
            let current = state.current().await;
            assert!(
                current.component.ends_with("c.component.ts"),
                "last selection wins"
            );
            assert_eq!(current.revision, 3);
        });
    }

    #[test]
    fn error_report_keeps_component_and_bumps_revision_signal() {
        let rt = tokio_runtime();
        rt.block_on(async {
            let state = AppState::new(
                Path::new("/a/x.component.ts"),
                Path::new("/a"),
                Strategy::Standalone,
                vec![],
                serde_json::Map::new(),
            );
            state.report_error("boom".into()).await;
            let current = state.current().await;
            assert_eq!(current.component, "/a/x.component.ts");
            assert_eq!(current.error.as_deref(), Some("boom"));
        });
    }

    #[test]
    fn selection_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("selection.json");
        write_selection_file(&file, Path::new("/a/x.component.ts"), Path::new("/a")).unwrap();
        let read = read_selection_file(&file).expect("parsed");
        assert_eq!(read.component, PathBuf::from("/a/x.component.ts"));
        assert_eq!(read.root, PathBuf::from("/a"));
    }
}
