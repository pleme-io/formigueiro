//! # apply — the write seam, safe by construction
//!
//! The swarm mutates the world through exactly one door: an [`ApplyExecutor`]. And
//! it may only walk through it for a *promoted* mutation — because an
//! [`AppliedMutation`] (the executor's sole input) is **constructible only from a
//! [`TickOutcome::Applied`]** ([`AppliedMutation::from_outcome`]). A shadowed,
//! up-to-date, or blocked outcome yields `None`, so a write without a promotion
//! decision is *unrepresentable*, not merely discouraged — the UNREPRESENTABILITY
//! model applied to the one dangerous operation.
//!
//! [`execute_applies`] is the structural driver: it filters a [`SwarmReport`]'s
//! outcomes through that gate, so only promoted mutations ever reach the executor.
//! [`NullExecutor`] refuses every write (the shadow-only default); a real executor
//! (e.g. formigueiro-flake's `NixFlakeExecutor`) is an explicit opt-in.

use serde::{Deserialize, Serialize};

use crate::{ColonyOutcome, SwarmReport, TickOutcome};

/// A promoted mutation authorized for execution. Its fields are private and its
/// **only** constructor is [`AppliedMutation::from_outcome`] — you cannot fabricate
/// an authorization for an unpromoted outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedMutation {
    kind: String,
    subject: String,
    from: String,
    to: String,
}

impl AppliedMutation {
    /// Extract the promoted mutation from a colony outcome — `Some` iff the outcome
    /// is [`TickOutcome::Applied`]. The type-level gate: a shadowed / up-to-date /
    /// blocked / unknown outcome yields `None`.
    #[must_use]
    pub fn from_outcome(outcome: &ColonyOutcome) -> Option<Self> {
        match outcome {
            ColonyOutcome::Ticked {
                kind,
                subject,
                outcome: TickOutcome::Applied { from, to },
            } => Some(Self {
                kind: kind.clone(),
                subject: subject.clone(),
                from: from.clone(),
                to: to.clone(),
            }),
            _ => None,
        }
    }

    /// The update kind.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }
    /// The subject being mutated.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }
    /// The value being mutated from.
    #[must_use]
    pub fn from(&self) -> &str {
        &self.from
    }
    /// The value being mutated to.
    #[must_use]
    pub fn to(&self) -> &str {
        &self.to
    }
}

/// A typed receipt that a promoted mutation was executed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReceipt {
    /// The kind that was applied.
    pub kind: String,
    /// The subject that was mutated.
    pub subject: String,
    /// The value it was mutated from.
    pub from: String,
    /// The value it was mutated to.
    pub to: String,
}

impl ApplyReceipt {
    /// A receipt for a completed mutation.
    #[must_use]
    pub fn of(mutation: &AppliedMutation) -> Self {
        Self {
            kind: mutation.kind.clone(),
            subject: mutation.subject.clone(),
            from: mutation.from.clone(),
            to: mutation.to.clone(),
        }
    }
}

/// A typed apply failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "applyError", content = "detail")]
pub enum ApplyError {
    /// The executor is disabled (the shadow-only default) — no write happened.
    Disabled,
    /// This executor does not handle the mutation's kind.
    Unsupported(String),
    /// The mutation was attempted but failed.
    Failed(String),
}

/// The write seam: execute a promoted mutation. The **only** place the swarm mutates
/// the world. A trait so the write path is exercised in tests without touching disk
/// or network, and so each backend (nix flake, image tag, chart version) is a
/// separate typed implementation.
pub trait ApplyExecutor {
    /// Execute the promoted `mutation`.
    ///
    /// # Errors
    /// [`ApplyError`] when the executor is disabled, cannot handle the kind, or the
    /// underlying mutation fails.
    fn apply(&self, mutation: &AppliedMutation) -> Result<ApplyReceipt, ApplyError>;
}

/// The safe default executor: refuses every write. With no real executor wired, a
/// promoted mutation still does not touch the world.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullExecutor;

impl ApplyExecutor for NullExecutor {
    fn apply(&self, _mutation: &AppliedMutation) -> Result<ApplyReceipt, ApplyError> {
        Err(ApplyError::Disabled)
    }
}

/// Execute every **promoted** mutation in a cycle report through `executor`, in
/// order, returning one result each. The gate is structural: [`AppliedMutation::from_outcome`]
/// admits only [`TickOutcome::Applied`] outcomes, so a shadowed / blocked outcome
/// can never reach the executor.
pub fn execute_applies<X: ApplyExecutor>(
    report: &SwarmReport,
    executor: &X,
) -> Vec<Result<ApplyReceipt, ApplyError>> {
    report
        .outcomes
        .iter()
        .filter_map(AppliedMutation::from_outcome)
        .map(|mutation| executor.apply(&mutation))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ShadowReason;
    use std::cell::RefCell;

    fn applied(subject: &str) -> ColonyOutcome {
        ColonyOutcome::Ticked {
            kind: "flake-input".into(),
            subject: subject.into(),
            outcome: TickOutcome::Applied {
                from: "old".into(),
                to: "new".into(),
            },
        }
    }
    fn shadowed(subject: &str) -> ColonyOutcome {
        ColonyOutcome::Ticked {
            kind: "flake-input".into(),
            subject: subject.into(),
            outcome: TickOutcome::Shadowed {
                from: "old".into(),
                to: "new".into(),
                reason: ShadowReason::Frozen,
            },
        }
    }

    #[test]
    fn appliedmutation_is_constructible_only_from_an_applied_outcome() {
        assert!(AppliedMutation::from_outcome(&applied("x")).is_some());
        assert!(AppliedMutation::from_outcome(&shadowed("x")).is_none());
        assert!(AppliedMutation::from_outcome(&ColonyOutcome::Ticked {
            kind: "flake-input".into(),
            subject: "x".into(),
            outcome: TickOutcome::UpToDate,
        })
        .is_none());
        assert!(AppliedMutation::from_outcome(&ColonyOutcome::UnknownKind {
            kind: "k".into(),
            subject: "x".into(),
        })
        .is_none());
    }

    #[test]
    fn null_executor_refuses_every_write() {
        let m = AppliedMutation::from_outcome(&applied("x")).unwrap();
        assert_eq!(NullExecutor.apply(&m), Err(ApplyError::Disabled));
    }

    #[test]
    fn execute_applies_only_reaches_the_executor_for_promoted_outcomes() {
        // a recording executor
        struct Rec(RefCell<Vec<String>>);
        impl ApplyExecutor for Rec {
            fn apply(&self, m: &AppliedMutation) -> Result<ApplyReceipt, ApplyError> {
                self.0.borrow_mut().push(m.subject().to_owned());
                Ok(ApplyReceipt::of(m))
            }
        }
        // a report with one Applied + one Shadowed
        let mut report = SwarmReport::empty(0);
        // reuse the counting via the public field
        report.outcomes.push(applied("promoted"));
        report.outcomes.push(shadowed("held"));

        let exec = Rec(RefCell::new(Vec::new()));
        let results = execute_applies(&report, &exec);
        assert_eq!(results.len(), 1, "only the promoted outcome is applied");
        assert_eq!(*exec.0.borrow(), vec!["promoted".to_owned()]);
        assert!(matches!(&results[0], Ok(r) if r.subject == "promoted"));
    }
}
