//! # converge — sustained-quiescence detection over the report stream
//!
//! [`SwarmReport::is_quiescent`] answers "is anything pending *this cycle*". But
//! convergence is temporal: a single quiescent cycle is not "at head" — a flake
//! input could move next tick. The fleet is **converged** only once it has been
//! quiescent for a run of consecutive cycles. [`ConvergenceTracker`] folds the
//! report stream into that judgment, and [`Convergence`] is the typed state a Viggy
//! `(defpromessa "fleet at head")` reads and attests.

use serde::{Deserialize, Serialize};

use crate::SwarmReport;

/// The swarm's convergence state at a point in the cycle stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "convergence", content = "detail")]
pub enum Convergence {
    /// A mutation is in flight this cycle (not quiescent).
    Converging {
        /// How many mutations are pending.
        pending: u32,
    },
    /// Quiescent, but not yet for long enough to call it settled.
    Settling {
        /// Consecutive quiescent cycles so far.
        stable_cycles: u32,
        /// The run length required to declare convergence.
        need: u32,
    },
    /// Quiescent for the required run of consecutive cycles — the fleet is at head.
    Converged,
}

impl Convergence {
    /// Whether this state is [`Convergence::Converged`].
    #[must_use]
    pub fn is_converged(&self) -> bool {
        matches!(self, Self::Converged)
    }
}

/// Detects convergence over the report stream: the fleet is [`Convergence::Converged`]
/// once it has been quiescent (no pending mutation) for `required` **consecutive**
/// cycles. Any non-quiescent cycle resets the run — sustained quiescence, not an
/// instantaneous snapshot, is convergence.
#[derive(Clone, Copy, Debug)]
pub struct ConvergenceTracker {
    required: u32,
    consecutive_quiescent: u32,
}

impl ConvergenceTracker {
    /// A tracker requiring `required_stable_cycles` consecutive quiescent cycles
    /// (clamped to at least 1) to declare convergence.
    #[must_use]
    pub fn new(required_stable_cycles: u32) -> Self {
        Self {
            required: required_stable_cycles.max(1),
            consecutive_quiescent: 0,
        }
    }

    /// Fold one cycle's report into the run and return the resulting state.
    pub fn observe(&mut self, report: &SwarmReport) -> Convergence {
        if report.is_quiescent() {
            self.consecutive_quiescent = self.consecutive_quiescent.saturating_add(1);
        } else {
            self.consecutive_quiescent = 0;
        }
        self.state(report)
    }

    /// The current state without folding a new report.
    #[must_use]
    pub fn state(&self, report: &SwarmReport) -> Convergence {
        if !report.is_quiescent() {
            Convergence::Converging {
                pending: report.pending_mutations(),
            }
        } else if self.consecutive_quiescent >= self.required {
            Convergence::Converged
        } else {
            Convergence::Settling {
                stable_cycles: self.consecutive_quiescent,
                need: self.required,
            }
        }
    }

    /// Whether the fleet is currently converged.
    #[must_use]
    pub fn is_converged(&self) -> bool {
        self.consecutive_quiescent >= self.required
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiescent() -> SwarmReport {
        SwarmReport::empty(0) // 0 pending
    }
    fn busy() -> SwarmReport {
        let mut r = SwarmReport::empty(0);
        r.shadowed = 1; // a pending mutation → not quiescent
        r
    }

    #[test]
    fn convergence_requires_a_sustained_run_of_quiescent_cycles() {
        let mut t = ConvergenceTracker::new(3);
        assert_eq!(
            t.observe(&quiescent()),
            Convergence::Settling { stable_cycles: 1, need: 3 }
        );
        assert_eq!(
            t.observe(&quiescent()),
            Convergence::Settling { stable_cycles: 2, need: 3 }
        );
        assert_eq!(t.observe(&quiescent()), Convergence::Converged);
        assert!(t.is_converged());
    }

    #[test]
    fn any_pending_cycle_resets_the_run() {
        let mut t = ConvergenceTracker::new(3);
        t.observe(&quiescent());
        t.observe(&quiescent()); // stable=2
        assert_eq!(t.observe(&busy()), Convergence::Converging { pending: 1 });
        assert!(!t.is_converged());
        // the run restarts from zero
        assert_eq!(
            t.observe(&quiescent()),
            Convergence::Settling { stable_cycles: 1, need: 3 }
        );
    }

    #[test]
    fn required_is_clamped_to_at_least_one() {
        let mut t = ConvergenceTracker::new(0);
        assert_eq!(t.observe(&quiescent()), Convergence::Converged);
    }
}
