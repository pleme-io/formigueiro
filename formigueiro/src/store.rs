//! # store — a durable, file-backed `PlanStore` for the workstation daemon
//!
//! [`formigueiro_core::MemPlanStore`] loses every promotion window when the daemon
//! restarts (an hourly launchd relaunch, a reboot). [`FilePlanStore`] is the durable
//! second impl of the same [`PlanStore`] trait: it loads on construction and
//! persists on every write (atomic tmp+rename), so a `ShadowConfirmEffect` window
//! keeps maturing across restarts — the thing that makes a real unfreeze safe.
//!
//! This is the *workstation* flavor (a JSON file, like the `--status` plan the daemon
//! already publishes); a server deployment implements the same trait over Postgres.
//! `formigueiro-core` stays pure — the filesystem lives here, in the binary.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use formigueiro_core::{PlanStore, TargetState};
use serde::{Deserialize, Serialize};

/// One persisted target (JSON can't key a map on a tuple, so the store is a flat
/// list of these).
#[derive(Serialize, Deserialize)]
struct StoredEntry {
    kind: String,
    subject: String,
    state: TargetState,
}

/// A [`PlanStore`] backed by a JSON file. Loads on construction; each write persists
/// atomically. A missing or corrupt file degrades to a fresh store (never a crash),
/// and a write failure is best-effort (never crashes the daemon).
pub struct FilePlanStore {
    path: PathBuf,
    map: BTreeMap<(String, String), TargetState>,
}

impl FilePlanStore {
    /// Load the store from `path` — empty if it doesn't exist or can't parse.
    #[must_use]
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|json| serde_json::from_str::<Vec<StoredEntry>>(&json).ok())
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|e| ((e.kind, e.subject), e.state))
                    .collect()
            })
            .unwrap_or_default();
        Self { path, map }
    }

    fn persist(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).context("create store dir")?;
        }
        let entries: Vec<StoredEntry> = self
            .map
            .iter()
            .map(|((kind, subject), state)| StoredEntry {
                kind: kind.clone(),
                subject: subject.clone(),
                state: state.clone(),
            })
            .collect();
        let tmp = self.path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp).context("create store tmp")?;
        serde_json::to_writer(&mut file, &entries).context("write store")?;
        file.flush().context("flush store")?;
        std::fs::rename(&tmp, &self.path).context("commit store")?;
        Ok(())
    }
}

impl PlanStore for FilePlanStore {
    fn get(&self, kind: &str, subject: &str) -> Option<TargetState> {
        self.map.get(&(kind.to_owned(), subject.to_owned())).cloned()
    }
    fn put(&mut self, kind: &str, subject: &str, state: TargetState) {
        self.map.insert((kind.to_owned(), subject.to_owned()), state);
        let _ = self.persist(); // best-effort — a write failure must not crash the daemon
    }
    fn targets(&self) -> Vec<(String, String, TargetState)> {
        self.map
            .iter()
            .map(|((k, s), st)| (k.clone(), s.clone(), st.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }
    fn pending(to: &str, since: i64) -> TargetState {
        TargetState {
            pending_to: Some(to.to_owned()),
            stable_since: Some(since),
            ..TargetState::default()
        }
    }

    #[test]
    fn a_written_target_survives_a_reload_a_restart() {
        let path = tmp("formigueiro-store-survives.json");
        let _ = std::fs::remove_file(&path);
        {
            let mut store = FilePlanStore::load(path.clone());
            store.put("flake-input", "cachix", pending("e1d8", 1000));
        } // drop — simulates the daemon exiting
        // reload from disk (a fresh process)
        let reloaded = FilePlanStore::load(path.clone());
        let got = reloaded.get("flake-input", "cachix").expect("survives restart");
        assert_eq!(got.pending_to.as_deref(), Some("e1d8"));
        assert_eq!(got.stable_since, Some(1000), "the maturing window is preserved");
        assert_eq!(reloaded.targets().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_empty_not_a_crash() {
        assert!(FilePlanStore::load(tmp("formigueiro-store-absent.json"))
            .targets()
            .is_empty());
        let corrupt = tmp("formigueiro-store-corrupt.json");
        std::fs::write(&corrupt, b"{ not json").unwrap();
        assert!(FilePlanStore::load(corrupt.clone()).targets().is_empty());
        let _ = std::fs::remove_file(&corrupt);
    }
}
