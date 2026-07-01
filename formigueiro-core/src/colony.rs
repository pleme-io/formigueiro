//! # colony — the swarm orchestrator
//!
//! A [`Colony`] is the typed catalog of update kinds the swarm knows how to run —
//! each paired with its [`outorga::PromotionPolicy`] — plus the fleet **freeze**
//! master switch. It dispatches one [`UpdateSignal`] to its kind's [`Formiga`]
//! (shadow → promotion decision) and returns a typed [`ColonyOutcome`].
//!
//! It is **dispatch-only**: it holds no update state and does no I/O — the
//! [`UpdateEnv`] (observation source) and the promotion [`Observation`] (the
//! per-target readiness, owned by a future PlanStore) are injected per call. So
//! the whole swarm decision stays a pure, total, testable function.
//!
//! Per CATALOG REFLECTION, [`Colony::kind_names`] self-describes what the swarm
//! supports; the freeze ([`Colony::is_frozen`]) is the two-key partner of each
//! kind's own promotion mode — engaged, it re-shadows every kind at once.

use std::collections::BTreeMap;

use outorga::{Observation, PromotionPolicy};
use serde::{Deserialize, Serialize};

use crate::{Formiga, TickOutcome, UpdateEnv, UpdateKind, UpdateSignal};

/// One registered update kind + its promotion policy.
struct Entry {
    kind: Box<dyn UpdateKind + Send + Sync>,
    policy: PromotionPolicy,
}

/// The swarm's registry of update kinds + the fleet freeze. Build it with
/// [`Colony::register`] (chainable); dispatch with [`Colony::ingest`].
#[derive(Default)]
pub struct Colony {
    entries: BTreeMap<String, Entry>,
    frozen: bool,
}

impl Colony {
    /// An empty colony (no kinds registered, not frozen).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an update kind (under its own [`UpdateKind::name`]) with a
    /// promotion policy. Chainable. Re-registering a name replaces it.
    #[must_use]
    pub fn register(
        mut self,
        kind: Box<dyn UpdateKind + Send + Sync>,
        policy: PromotionPolicy,
    ) -> Self {
        self.entries
            .insert(kind.name().to_owned(), Entry { kind, policy });
        self
    }

    /// Engage (`true`) or release (`false`) the fleet freeze. Chainable. While
    /// frozen, every ingest is held in shadow regardless of each kind's mode.
    #[must_use]
    pub fn frozen(mut self, frozen: bool) -> Self {
        self.frozen = frozen;
        self
    }

    /// Is the fleet freeze engaged?
    #[must_use]
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// The names of every registered kind (CATALOG REFLECTION).
    #[must_use]
    pub fn kind_names(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// The promotion policy for a kind, if registered.
    #[must_use]
    pub fn policy_for(&self, kind: &str) -> Option<PromotionPolicy> {
        self.entries.get(kind).map(|e| e.policy)
    }

    /// Ingest one signal: look up its kind, run a [`Formiga`] tick (shadow →
    /// promotion, with the fleet freeze folded in), and wrap the result. An
    /// unregistered kind is a typed [`ColonyOutcome::UnknownKind`], never a panic.
    pub fn ingest(
        &self,
        sig: &UpdateSignal,
        env: &dyn UpdateEnv,
        obs: &impl Observation,
        now_epoch: i64,
    ) -> ColonyOutcome {
        match self.entries.get(&sig.kind) {
            None => ColonyOutcome::UnknownKind {
                kind: sig.kind.clone(),
                subject: sig.subject.clone(),
            },
            Some(e) => {
                let outcome = Formiga::new(e.policy).tick(
                    e.kind.as_ref(),
                    sig,
                    env,
                    obs,
                    now_epoch,
                    self.frozen,
                );
                ColonyOutcome::Ticked {
                    kind: sig.kind.clone(),
                    subject: sig.subject.clone(),
                    outcome,
                }
            }
        }
    }
}

/// The typed result of ingesting one signal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "result", content = "detail")]
pub enum ColonyOutcome {
    /// The signal named a kind the colony does not know.
    UnknownKind {
        /// The unknown kind name.
        kind: String,
        /// The subject it named.
        subject: String,
    },
    /// The signal was dispatched to its kind's formiga.
    Ticked {
        /// The kind that handled it.
        kind: String,
        /// The subject.
        subject: String,
        /// The formiga's typed outcome.
        outcome: TickOutcome,
    },
}

impl ColonyOutcome {
    /// The inner [`TickOutcome`], if the signal was dispatched.
    #[must_use]
    pub fn tick_outcome(&self) -> Option<&TickOutcome> {
        match self {
            Self::Ticked { outcome, .. } => Some(outcome),
            Self::UnknownKind { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockReason, FlakeInputKind, ShadowReason};
    use outorga::PromotionMode;

    struct Env(Option<&'static str>, Result<&'static str, BlockReason>);
    impl UpdateEnv for Env {
        fn current(&self, _s: &UpdateSignal) -> Option<String> {
            self.0.map(String::from)
        }
        fn latest(&self, _s: &UpdateSignal) -> Result<String, BlockReason> {
            self.1.clone().map(String::from)
        }
    }

    #[derive(Default, Clone, Copy)]
    struct Obs;
    impl Observation for Obs {
        fn ready(&self) -> bool {
            false
        }
        fn stale(&self) -> bool {
            false
        }
        fn conflict(&self) -> bool {
            false
        }
        fn ready_since(&self) -> Option<i64> {
            None
        }
        fn operator_confirmed(&self) -> bool {
            false
        }
    }

    fn colony(frozen: bool) -> Colony {
        Colony::new()
            .register(
                Box::new(FlakeInputKind),
                PromotionPolicy::new(PromotionMode::Effect),
            )
            .frozen(frozen)
    }

    #[test]
    fn reflection_lists_registered_kinds() {
        let c = colony(false);
        assert_eq!(c.kind_names(), vec!["flake-input"]);
        assert!(c.policy_for("flake-input").is_some());
        assert!(c.policy_for("nope").is_none());
    }

    #[test]
    fn unknown_kind_is_typed_not_a_panic() {
        let c = colony(false);
        let sig = UpdateSignal::new("image-tag", "x");
        assert_eq!(
            c.ingest(&sig, &Env(None, Ok("v")), &Obs, 0),
            ColonyOutcome::UnknownKind {
                kind: "image-tag".into(),
                subject: "x".into()
            }
        );
    }

    #[test]
    fn known_kind_dispatches_and_effect_applies() {
        let c = colony(false);
        let sig = UpdateSignal::new("flake-input", "blackmatter");
        let out = c.ingest(&sig, &Env(Some("old"), Ok("new")), &Obs, 0);
        match out {
            ColonyOutcome::Ticked { outcome, .. } => assert_eq!(
                outcome,
                TickOutcome::Applied {
                    from: "old".into(),
                    to: "new".into()
                }
            ),
            other => panic!("expected Ticked/Applied, got {other:?}"),
        }
    }

    #[test]
    fn freeze_re_shadows_every_kind_at_once() {
        let c = colony(true); // frozen, even though the kind is Effect
        assert!(c.is_frozen());
        let sig = UpdateSignal::new("flake-input", "blackmatter");
        let out = c.ingest(&sig, &Env(Some("old"), Ok("new")), &Obs, 0);
        assert_eq!(
            out.tick_outcome(),
            Some(&TickOutcome::Shadowed {
                from: "old".into(),
                to: "new".into(),
                reason: ShadowReason::Frozen
            })
        );
    }

    #[test]
    fn uptodate_signal_ticks_uptodate() {
        let c = colony(false);
        let sig = UpdateSignal::new("flake-input", "blackmatter");
        let out = c.ingest(&sig, &Env(Some("v"), Ok("v")), &Obs, 0);
        assert_eq!(out.tick_outcome(), Some(&TickOutcome::UpToDate));
    }

    #[test]
    fn colony_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Colony>();
    }
}
