//! Tiny on-disk state for the orchestrator.
//!
//! Lives at `$GLASS_SYSTEM_DATA/state.json`. Currently records one thing:
//! `last_dm_id`, the Discord message-id of the most recent inbound DM Glass
//! has processed. On startup, the orchestrator queries Discord for messages
//! newer than this id and replays them — so DMs sent while she was offline
//! still get a turn.
//!
//! This is operational state, not user content (which lives in the vault)
//! or session state (which Loom owns). Lose this file and the worst case
//! is: next startup skips catch-up. No data loss; just a missed window.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestratorState {
    /// Discord snowflake id of the last inbound DM we processed. `None` on a
    /// fresh install; we don't catch up from "the beginning of time" because
    /// the agent's identity is still bootstrapping. After the first DM lands
    /// live, this gets populated and subsequent restarts catch up cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_dm_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read state from disk. Missing file → empty state. Malformed contents
    /// also → empty state with a warn log; we never crash on a corrupted
    /// file because the worst case (catch-up skipped) is recoverable.
    pub fn load(&self) -> OrchestratorState {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return OrchestratorState::default()
            }
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "state: failed to read ({e}); assuming empty"
                );
                return OrchestratorState::default();
            }
        };
        match serde_json::from_str::<OrchestratorState>(&raw) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "state: failed to parse ({e}); assuming empty"
                );
                OrchestratorState::default()
            }
        }
    }

    /// Atomic-rename write: serialize to a `.tmp` sibling, then rename over
    /// the real file. The state file is small (~50 bytes); cost is negligible
    /// even if called after every DM.
    pub fn save(&self, state: &OrchestratorState) -> Result<()> {
        let body = serde_json::to_string(state).context("state: serialize")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body.as_bytes())
            .with_context(|| format!("state: writing tmp {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("state: renaming over {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_on_missing_file_returns_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        assert_eq!(store.load(), OrchestratorState::default());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let s = OrchestratorState {
            last_dm_id: Some(123456789),
        };
        store.save(&s).unwrap();
        assert_eq!(store.load(), s);
    }

    #[test]
    fn save_is_atomic_no_partial_files_left_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        store
            .save(&OrchestratorState {
                last_dm_id: Some(7),
            })
            .unwrap();
        // After save, only the final file should exist; the .tmp sibling
        // should have been renamed away.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "state.json");
    }

    #[test]
    fn malformed_file_is_treated_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{not valid json").unwrap();
        let store = StateStore::new(&path);
        assert_eq!(store.load(), OrchestratorState::default());
    }
}
