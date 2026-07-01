//! # plan_store — the per-target memory that makes promotion temporal
//!
//! The shadow algebra is per-tick and stateless. But `ShadowConfirmEffect`
//! promotion is **temporal**: a mutation must hold *stable* for a clean-observation
//! window before it earns `Apply`. Something has to remember *when* the current
//! pending mutation first appeared — that is the [`PlanStore`].
//!
//! Each tick, [`fold`] merges a fresh [`ShadowOutcome`] into the stored
//! [`TargetState`] for a `(kind, subject)`:
//! - **same** pending `to` as last tick → keep `stable_since` (the window
//!   accumulates → the target ages toward promotion);
//! - a **different** `to` → reset `stable_since = now` (a new target re-shadows the
//!   window — you never promote a mutation you only just saw);
//! - **UpToDate** → clear (nothing to promote);
//! - **Blocked(Conflict)** → keep the pending target but mark conflict (blocks
//!   promotion); other **Blocked** → hold the prior state unchanged (a transient
//!   observation failure must not reset a maturing window).
//!
//! [`TargetState`] then IS the [`outorga::Observation`] fed to the promotion
//! decision — so `ready_since` is real elapsed stability, not a per-call guess.
//! [`PlanStore`] is a trait: [`MemPlanStore`] is the M0/in-memory impl; a
//! CRD/Postgres impl (the durable, restart-safe home) satisfies the same contract.

use std::collections::BTreeMap;

use outorga::Observation;
use serde::{Deserialize, Serialize};

use crate::{BlockReason, ShadowOutcome};

/// The tracked state of one `(kind, subject)` target across ticks. Impls
/// [`outorga::Observation`] so it feeds the promotion decision directly.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetState {
    /// The current pending mutation's `to` value, if a mutation is pending.
    pub pending_to: Option<String>,
    /// Unix epoch when `pending_to` first became stable (the window start).
    pub stable_since: Option<i64>,
    /// Whether the last observation reported the target stale.
    pub stale: bool,
    /// Whether the last observation reported another writer (conflict).
    pub conflict: bool,
    /// Operator fast-path confirm for this target.
    pub confirmed: bool,
}

impl TargetState {
    /// The idle state — nothing pending.
    #[must_use]
    pub fn idle() -> Self {
        Self::default()
    }

    /// Set the operator confirm flag (the promotion fast-path). Chainable.
    #[must_use]
    pub fn confirm(mut self) -> Self {
        self.confirmed = true;
        self
    }
}

impl Observation for TargetState {
    fn ready(&self) -> bool {
        self.pending_to.is_some()
    }
    fn stale(&self) -> bool {
        self.stale
    }
    fn conflict(&self) -> bool {
        self.conflict
    }
    fn ready_since(&self) -> Option<i64> {
        if self.pending_to.is_some() {
            self.stable_since
        } else {
            None
        }
    }
    fn operator_confirmed(&self) -> bool {
        self.confirmed
    }
}

/// Merge a fresh shadow outcome into the prior [`TargetState`] at `now`, returning
/// the updated state. The temporal core (see the module doc). Pure. `confirmed`
/// is carried forward from `prev` (the operator sets it out-of-band via the store).
#[must_use]
pub fn fold(prev: Option<&TargetState>, outcome: &ShadowOutcome, now_epoch: i64) -> TargetState {
    let confirmed = prev.is_some_and(|p| p.confirmed);
    match outcome {
        ShadowOutcome::UpToDate => TargetState {
            confirmed,
            ..TargetState::idle()
        },
        ShadowOutcome::Blocked(BlockReason::Conflict) => {
            let mut s = prev.cloned().unwrap_or_default();
            s.conflict = true;
            s.confirmed = confirmed;
            s
        }
        // A transient observation failure must NOT reset a maturing window.
        ShadowOutcome::Blocked(_) => prev.cloned().unwrap_or_default(),
        ShadowOutcome::WouldApply { to, .. } => {
            let same = prev.and_then(|p| p.pending_to.as_deref()) == Some(to.as_str());
            let stable_since = if same {
                prev.and_then(|p| p.stable_since).or(Some(now_epoch))
            } else {
                Some(now_epoch)
            };
            TargetState {
                pending_to: Some(to.clone()),
                stable_since,
                stale: false,
                conflict: false,
                confirmed: if same { confirmed } else { false },
            }
        }
    }
}

/// A store of per-target state. A trait so the in-memory M0 impl and a future
/// durable (CRD/Postgres) impl share one contract.
pub trait PlanStore {
    /// The stored state for a target, if any.
    fn get(&self, kind: &str, subject: &str) -> Option<TargetState>;
    /// Write the state for a target.
    fn put(&mut self, kind: &str, subject: &str, state: TargetState);
    /// Every tracked `(kind, subject, state)` — for a fleet-wide pending view.
    /// (The durable CRD/Postgres impl backs this with a prefix scan.)
    fn targets(&self) -> Vec<(String, String, TargetState)>;
}

/// The in-memory [`PlanStore`] (M0 / tests). A durable impl (operator CRD /
/// Postgres) is the destination — see `theory/FORMIGUEIRO.md` §IV.4.
#[derive(Debug, Default, Clone)]
pub struct MemPlanStore {
    map: BTreeMap<(String, String), TargetState>,
}

impl MemPlanStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// The number of tracked targets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }
    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl PlanStore for MemPlanStore {
    fn get(&self, kind: &str, subject: &str) -> Option<TargetState> {
        self.map.get(&(kind.to_owned(), subject.to_owned())).cloned()
    }
    fn put(&mut self, kind: &str, subject: &str, state: TargetState) {
        self.map.insert((kind.to_owned(), subject.to_owned()), state);
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

    const T0: i64 = 1_000_000;
    fn would(to: &str) -> ShadowOutcome {
        ShadowOutcome::WouldApply {
            from: "old".into(),
            to: to.into(),
        }
    }

    #[test]
    fn a_stable_target_accumulates_its_window_across_ticks() {
        // first sighting at T0
        let s1 = fold(None, &would("new"), T0);
        assert_eq!(s1.ready_since(), Some(T0));
        // same target 1000s later → window START stays at T0 (it is maturing)
        let s2 = fold(Some(&s1), &would("new"), T0 + 1000);
        assert_eq!(s2.ready_since(), Some(T0));
        assert!(s2.ready());
    }

    #[test]
    fn a_changed_target_resets_the_window() {
        let s1 = fold(None, &would("new"), T0);
        // a DIFFERENT `to` appears later → the window restarts (never promote a
        // mutation you only just saw)
        let s2 = fold(Some(&s1), &would("newer"), T0 + 1000);
        assert_eq!(s2.pending_to.as_deref(), Some("newer"));
        assert_eq!(s2.ready_since(), Some(T0 + 1000));
    }

    #[test]
    fn uptodate_clears_the_pending_but_keeps_confirm() {
        let confirmed = fold(None, &would("new"), T0).confirm();
        let cleared = fold(Some(&confirmed), &ShadowOutcome::UpToDate, T0 + 5);
        assert!(!cleared.ready());
        assert_eq!(cleared.ready_since(), None);
        assert!(cleared.operator_confirmed(), "confirm carries forward");
    }

    #[test]
    fn conflict_holds_the_pending_and_flags_conflict() {
        let s1 = fold(None, &would("new"), T0);
        let c = fold(Some(&s1), &ShadowOutcome::Blocked(BlockReason::Conflict), T0 + 10);
        assert_eq!(c.pending_to.as_deref(), Some("new")); // still pending
        assert!(c.conflict());
        assert_eq!(c.ready_since(), Some(T0)); // window not reset by a conflict
    }

    #[test]
    fn a_transient_block_does_not_reset_a_maturing_window() {
        let s1 = fold(None, &would("new"), T0);
        let held = fold(Some(&s1), &ShadowOutcome::Blocked(BlockReason::Unreachable), T0 + 500);
        assert_eq!(held.ready_since(), Some(T0), "transient failure must not restart the window");
        assert_eq!(held.pending_to.as_deref(), Some("new"));
    }

    #[test]
    fn mem_store_round_trips() {
        let mut store = MemPlanStore::new();
        assert!(store.is_empty());
        let s = fold(None, &would("new"), T0);
        store.put("flake-input", "blackmatter", s.clone());
        assert_eq!(store.get("flake-input", "blackmatter"), Some(s));
        assert_eq!(store.get("flake-input", "other"), None);
        assert_eq!(store.len(), 1);
    }
}
