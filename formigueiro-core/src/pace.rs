//! # pace — admission control for the write path
//!
//! The write seam is safe (only promoted mutations reach it) but *unbounded*: a
//! cycle that promotes many mutations at once would fire them all. A [`Pacer`]
//! bounds the mutation rate so the swarm never bursts against an upstream's limit —
//! the doctrine's samba pacing.
//!
//! [`execute_applies_paced`] is the paced driver: a promoted mutation the pacer
//! won't admit is **deferred, not dropped** — it stays pending and is retried next
//! cycle (the store keeps its maturing window), so pacing bounds the *rate* without
//! losing *work*. [`LeakyBucketPacer`] is the M0 token bucket; production wires the
//! fleet samba consumer (a `SambaPacer: Pacer`) — same trait, different backend, the
//! `PlanStore`/`ReportSink` pattern again.

use serde::{Deserialize, Serialize};

use crate::{AppliedMutation, ApplyError, ApplyExecutor, ApplyReceipt, SwarmReport};

/// Admission control for mutations: `admit` returns whether one may execute now,
/// consuming capacity if so. A trait so the rate policy is swappable (a local
/// bucket, samba, a fixed schedule) and testable without real time.
pub trait Pacer {
    /// Whether a mutation may execute at `now_epoch` (consumes a token if so).
    fn admit(&mut self, now_epoch: i64) -> bool;
}

/// A token-bucket pacer: up to `capacity` in a burst, refilling `refill_per_sec`
/// tokens per second. Time is injected (via `admit`'s `now_epoch`), so it is exact
/// and testable — no wall clock inside.
#[derive(Clone, Copy, Debug)]
pub struct LeakyBucketPacer {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last: Option<i64>,
}

impl LeakyBucketPacer {
    /// A bucket of `capacity` burst refilling `refill_per_sec` tokens/sec. Starts
    /// full. Negative inputs are clamped to zero.
    #[must_use]
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        let capacity = capacity.max(0.0);
        Self {
            capacity,
            refill_per_sec: refill_per_sec.max(0.0),
            tokens: capacity,
            last: None,
        }
    }

    /// A pacer admitting at most one mutation every `secs` seconds (burst 1). A
    /// non-positive `secs` admits without limit.
    #[must_use]
    pub fn one_every(secs: f64) -> Self {
        let rate = if secs > 0.0 { 1.0 / secs } else { f64::INFINITY };
        Self::new(1.0, rate)
    }

    /// The current token count (for observability / tests).
    #[must_use]
    pub fn tokens(&self) -> f64 {
        self.tokens
    }
}

impl Pacer for LeakyBucketPacer {
    #[allow(clippy::cast_precision_loss)] // epoch deltas are tiny vs f64's mantissa
    fn admit(&mut self, now_epoch: i64) -> bool {
        if let Some(last) = self.last {
            let elapsed = (now_epoch - last).max(0) as f64;
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        }
        self.last = Some(now_epoch);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The outcome of a promoted mutation under pacing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "paced", content = "detail")]
pub enum PacedApply {
    /// The pacer withheld admission — deferred, retried next cycle (not dropped).
    Deferred(AppliedMutation),
    /// The pacer admitted it; here is the executor's result.
    Executed(Result<ApplyReceipt, ApplyError>),
}

/// Execute a report's promoted mutations through `executor`, but only as fast as
/// `pacer` admits. Preserves order; a withheld mutation becomes
/// [`PacedApply::Deferred`] (retried next cycle), never lost. The write gate is
/// still structural — only [`crate::TickOutcome::Applied`] outcomes are considered.
pub fn execute_applies_paced<X: ApplyExecutor, P: Pacer>(
    report: &SwarmReport,
    executor: &X,
    pacer: &mut P,
    now_epoch: i64,
) -> Vec<PacedApply> {
    report
        .outcomes
        .iter()
        .filter_map(AppliedMutation::from_outcome)
        .map(|mutation| {
            if pacer.admit(now_epoch) {
                PacedApply::Executed(executor.apply(&mutation))
            } else {
                PacedApply::Deferred(mutation)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColonyOutcome, NullExecutor, TickOutcome};

    #[test]
    fn a_full_bucket_admits_its_burst_then_withholds() {
        let mut p = LeakyBucketPacer::new(2.0, 1.0);
        assert!(p.admit(0)); // 2 → 1
        assert!(p.admit(0)); // 1 → 0
        assert!(!p.admit(0)); // empty
    }

    #[test]
    fn the_bucket_refills_with_elapsed_time() {
        let mut p = LeakyBucketPacer::new(1.0, 1.0);
        assert!(p.admit(0));
        assert!(!p.admit(0)); // empty
        assert!(p.admit(1)); // +1s → +1 token → admits
    }

    #[test]
    fn one_every_paces_a_single_mutation_per_interval() {
        let mut p = LeakyBucketPacer::one_every(60.0);
        assert!(p.admit(1000)); // first admits
        assert!(!p.admit(1000)); // same instant → withheld
        assert!(!p.admit(1030)); // 30s < 60 → still withheld
        assert!(p.admit(1060)); // 60s → refilled → admits
    }

    #[test]
    fn paced_execution_defers_what_the_bucket_wont_admit() {
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
        let mut report = SwarmReport::empty(0);
        report.outcomes.push(applied("a"));
        report.outcomes.push(applied("b"));
        report.outcomes.push(applied("c"));

        // capacity 1: exactly one is admitted this cycle, two deferred.
        let mut pacer = LeakyBucketPacer::new(1.0, 0.0);
        let results = execute_applies_paced(&report, &NullExecutor, &mut pacer, 0);
        assert_eq!(results.len(), 3);
        let deferred = results
            .iter()
            .filter(|r| matches!(r, PacedApply::Deferred(_)))
            .count();
        let executed = results
            .iter()
            .filter(|r| matches!(r, PacedApply::Executed(_)))
            .count();
        assert_eq!((executed, deferred), (1, 2), "one admitted, two deferred (not dropped)");
    }
}
