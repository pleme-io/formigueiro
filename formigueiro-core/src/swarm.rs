//! # swarm — the stateful daemon object + the cycle report
//!
//! [`Colony`] is dispatch-only and [`PlanStore`] is the memory; a [`Swarm`] owns
//! both and runs a **cycle**: tick every signal through the store, classify the
//! outcomes, and return a typed [`SwarmReport`]. The M0 daemon holds exactly one
//! `Swarm`; each convergence tick calls [`Swarm::run_cycle`].
//!
//! The report is the **shadow-first observability contract**: it counts, for one
//! cycle, how many mutations were `applied` vs held in `shadow` vs already
//! `up_to_date` vs `blocked` vs of an `unknown_kind` — the "what would / did this
//! cycle do" surface the operator watches (and the OutcomeChain will attest)
//! *before* trusting the swarm with more control. [`SwarmReport::is_quiescent`]
//! ("nothing pending to converge") is the fleet-currency predicate a Viggy
//! `(defpromessa)` proves.
//!
//! Generic over the [`PlanStore`]: M0 holds a `Swarm<MemPlanStore>`; a durable
//! deployment holds `Swarm<CrdPlanStore>` — the cycle logic is identical.

use serde::{Deserialize, Serialize};

use crate::{Colony, ColonyOutcome, PlanStore, TickOutcome, UpdateEnv, UpdateSignal};

/// The stateful swarm: a [`Colony`] (kinds + policies + freeze) plus its
/// [`PlanStore`] (per-target window memory). Runs cycles; owns no I/O (the
/// [`UpdateEnv`] is injected per cycle). Not `Clone`/`Debug`: the colony holds
/// `dyn UpdateKind` trait objects, and a swarm is a single owned daemon anyway.
pub struct Swarm<S: PlanStore> {
    colony: Colony,
    store: S,
}

impl<S: PlanStore> Swarm<S> {
    /// Assemble a swarm from a colony + a store.
    pub fn new(colony: Colony, store: S) -> Self {
        Self { colony, store }
    }

    /// The colony (kinds + policies + freeze).
    #[must_use]
    pub fn colony(&self) -> &Colony {
        &self.colony
    }

    /// The plan store (read).
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The plan store (write) — for out-of-band edits (e.g. an operator confirm).
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// Run one convergence cycle: tick every signal through the store (folding each
    /// shadow into its per-target window), classify the outcomes, and roll them up
    /// into a [`SwarmReport`]. `now_epoch` is the cycle time (applied to every
    /// signal — a cycle is one logical instant).
    pub fn run_cycle(
        &mut self,
        signals: &[UpdateSignal],
        env: &dyn UpdateEnv,
        now_epoch: i64,
    ) -> SwarmReport {
        let mut report = SwarmReport::empty(now_epoch);
        for sig in signals {
            let outcome = self
                .colony
                .tick_with_store(&mut self.store, sig, env, now_epoch);
            report.record(outcome);
        }
        report
    }
}

/// A typed rollup of one swarm cycle — the shadow-first observability contract.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmReport {
    /// The cycle time.
    pub at_epoch: i64,
    /// Mutations that were promoted + applied this cycle.
    pub applied: u32,
    /// Mutations computed but held in shadow (the M0 steady state).
    pub shadowed: u32,
    /// Targets already at head (no mutation).
    pub up_to_date: u32,
    /// Targets whose shadow could not compute a target.
    pub blocked: u32,
    /// Signals naming a kind the colony does not know.
    pub unknown_kind: u32,
    /// Every per-signal outcome, in order.
    pub outcomes: Vec<ColonyOutcome>,
}

impl SwarmReport {
    /// An empty report for cycle time `at_epoch`.
    #[must_use]
    pub fn empty(at_epoch: i64) -> Self {
        Self {
            at_epoch,
            ..Self::default()
        }
    }

    /// Fold one outcome into the counts + the ordered list.
    fn record(&mut self, outcome: ColonyOutcome) {
        match &outcome {
            ColonyOutcome::UnknownKind { .. } => self.unknown_kind += 1,
            ColonyOutcome::Ticked { outcome: tick, .. } => match tick {
                TickOutcome::Applied { .. } => self.applied += 1,
                TickOutcome::Shadowed { .. } => self.shadowed += 1,
                TickOutcome::UpToDate => self.up_to_date += 1,
                TickOutcome::Blocked(_) => self.blocked += 1,
            },
        }
        self.outcomes.push(outcome);
    }

    /// The number of signals this cycle handled.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.applied + self.shadowed + self.up_to_date + self.blocked + self.unknown_kind
    }

    /// Targets with a pending mutation (applied OR held in shadow) — the work the
    /// swarm still has in flight.
    #[must_use]
    pub fn pending_mutations(&self) -> u32 {
        self.applied + self.shadowed
    }

    /// The fleet-currency predicate: no mutation is pending — every target is at
    /// head (or blocked/unknown, which pending-work can't clear). Nothing to
    /// converge. This is what a Viggy `(defpromessa "fleet at head")` proves.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.pending_mutations() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockReason, FlakeInputKind, MemPlanStore};
    use outorga::{PromotionMode, PromotionPolicy};

    /// An env keyed by subject so one env serves a mixed batch.
    struct Env;
    impl UpdateEnv for Env {
        fn current(&self, sig: &UpdateSignal) -> Option<String> {
            match sig.subject.as_str() {
                "current" => Some("v".into()),
                "stale" => Some("old".into()),
                "unreachable" => Some("x".into()),
                _ => None,
            }
        }
        fn latest(&self, sig: &UpdateSignal) -> Result<String, BlockReason> {
            match sig.subject.as_str() {
                "current" => Ok("v".into()),          // up-to-date
                "stale" => Ok("new".into()),          // would-apply
                "unreachable" => Err(BlockReason::Unreachable),
                _ => Ok("new".into()),
            }
        }
    }

    fn swarm() -> Swarm<MemPlanStore> {
        Swarm::new(
            Colony::new().register(
                Box::new(FlakeInputKind),
                PromotionPolicy::new(PromotionMode::Effect),
            ),
            MemPlanStore::new(),
        )
    }

    #[test]
    fn run_cycle_rolls_up_a_mixed_batch() {
        let mut s = swarm();
        let sigs = [
            UpdateSignal::new("flake-input", "current"),     // up-to-date
            UpdateSignal::new("flake-input", "stale"),       // would-apply → Effect applies
            UpdateSignal::new("flake-input", "unreachable"), // blocked
            UpdateSignal::new("image-tag", "x"),             // unknown kind
        ];
        let r = s.run_cycle(&sigs, &Env, 1_000);
        assert_eq!(r.at_epoch, 1_000);
        assert_eq!(r.up_to_date, 1);
        assert_eq!(r.applied, 1);
        assert_eq!(r.blocked, 1);
        assert_eq!(r.unknown_kind, 1);
        assert_eq!(r.total(), 4);
        assert_eq!(r.outcomes.len(), 4);
    }

    #[test]
    fn quiescence_is_no_pending_mutation() {
        let mut s = swarm();
        // only up-to-date + blocked + unknown → no pending mutation → quiescent
        let sigs = [
            UpdateSignal::new("flake-input", "current"),
            UpdateSignal::new("flake-input", "unreachable"),
            UpdateSignal::new("nope", "x"),
        ];
        assert!(s.run_cycle(&sigs, &Env, 1).is_quiescent());
        // a would-apply (Effect → applied) makes it non-quiescent this cycle
        let mut s2 = swarm();
        let r = s2.run_cycle(&[UpdateSignal::new("flake-input", "stale")], &Env, 1);
        assert!(!r.is_quiescent());
        assert_eq!(r.pending_mutations(), 1);
    }

    #[test]
    fn cycles_persist_state_in_the_store_across_calls() {
        // ShadowConfirmEffect: cycle 1 holds, cycle 2 (window elapsed) applies —
        // proving the Swarm carries the store between cycles.
        let mut s = Swarm::new(
            Colony::new().register(
                Box::new(FlakeInputKind),
                PromotionPolicy::new(PromotionMode::ShadowConfirmEffect).confirm_after(600),
            ),
            MemPlanStore::new(),
        );
        let sigs = [UpdateSignal::new("flake-input", "stale")];
        let c1 = s.run_cycle(&sigs, &Env, 10_000);
        assert_eq!((c1.shadowed, c1.applied), (1, 0));
        let c2 = s.run_cycle(&sigs, &Env, 10_600);
        assert_eq!((c2.shadowed, c2.applied), (0, 1), "window elapsed → apply");
        assert_eq!(s.store().len(), 1);
    }

    #[test]
    fn report_serializes_camel_case() {
        let r = SwarmReport::empty(5);
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains("\"atEpoch\":5") && j.contains("\"upToDate\":0"), "got {j}");
    }
}
