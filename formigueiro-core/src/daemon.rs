//! # daemon — the swarm control object
//!
//! [`Swarm`] runs a cycle when handed a `now`; a [`SwarmDaemon`] is the object that
//! *owns the loop's remaining seams* so the running daemon is a tested value rather
//! than untested `main()` glue:
//!
//! - **[`Clock`]** — the source of `now` (testable time; the real [`SystemClock`]
//!   vs a fixed clock in tests).
//! - **[`ReportSink`]** — *where a cycle's [`SwarmReport`] goes*: a log line, a
//!   file, a NATS subject, an OutcomeChain attestation. The output/attestation
//!   seam (the doctrine's "attest onto an OutcomeChain") — mockable, so a cycle's
//!   emission is asserted without any real sink.
//!
//! [`SwarmDaemon::tick`] is the whole per-cycle step: read `now` from the clock,
//! run the cycle over an injected source + env, emit the report to the sink, return
//! it. The actual pacing (sleep between ticks) is the one thin async wrapper left to
//! the binary — everything decision-bearing is here and tested. The source + env are
//! injected per tick because they are re-derived each cycle (a flake's lock is
//! re-read after an apply); the daemon owns what persists (swarm, clock, sink).

use crate::{PlanStore, SignalSource, Swarm, SwarmReport, UpdateEnv};

/// A source of the current time as Unix epoch seconds. A trait so the daemon loop
/// is exercised without real time.
pub trait Clock {
    /// The current time (epoch seconds).
    fn now_epoch(&self) -> i64;
}

/// The real wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_epoch(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .unwrap_or(0)
    }
}

/// Where a cycle's [`SwarmReport`] goes — the output/attestation seam. A log line,
/// a file, a NATS subject, an OutcomeChain: each is a `ReportSink`. Takes `&self`
/// (a sink with state uses interior mutability) so the daemon can hold it by shared
/// reference across ticks.
pub trait ReportSink {
    /// Record one cycle's report.
    fn emit(&self, report: &SwarmReport);
}

/// A sink that drops every report — for shadow-only smoke runs where the pending
/// plan (not the per-cycle rollup) is what the operator reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSink;

impl ReportSink for NullSink {
    fn emit(&self, _report: &SwarmReport) {}
}

/// The swarm control object: owns the [`Swarm`] (colony + store), the [`Clock`], and
/// the [`ReportSink`]. [`SwarmDaemon::tick`] is one paced-loop iteration minus the
/// sleep.
pub struct SwarmDaemon<S: PlanStore, C: Clock, K: ReportSink> {
    swarm: Swarm<S>,
    clock: C,
    sink: K,
}

impl<S: PlanStore, C: Clock, K: ReportSink> SwarmDaemon<S, C, K> {
    /// Assemble a daemon from a swarm, a clock, and a report sink.
    pub fn new(swarm: Swarm<S>, clock: C, sink: K) -> Self {
        Self { swarm, clock, sink }
    }

    /// The owned swarm (for `pending_plan` / store queries between ticks).
    #[must_use]
    pub fn swarm(&self) -> &Swarm<S> {
        &self.swarm
    }

    /// One cycle: read `now` from the clock, run the swarm over `source` + `env`,
    /// emit the report to the sink, and return it. The source + env are injected
    /// (re-derived each cycle by the caller); the daemon owns what persists.
    pub fn tick<Src: SignalSource>(&mut self, source: &Src, env: &dyn UpdateEnv) -> SwarmReport {
        let now = self.clock.now_epoch();
        let report = self.swarm.run_cycle_from(source, env, now);
        self.sink.emit(&report);
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BlockReason, Colony, FlakeInputKind, MemPlanStore, ShadowReason, UpdateEnv, UpdateSignal,
    };
    use outorga::{PromotionMode, PromotionPolicy};
    use std::cell::{Cell, RefCell};

    /// A clock the test advances by hand.
    struct FixedClock(Cell<i64>);
    impl Clock for FixedClock {
        fn now_epoch(&self) -> i64 {
            self.0.get()
        }
    }

    /// A sink that records every report it is handed.
    #[derive(Default)]
    struct CollectingSink(RefCell<Vec<SwarmReport>>);
    impl ReportSink for CollectingSink {
        fn emit(&self, report: &SwarmReport) {
            self.0.borrow_mut().push(report.clone());
        }
    }

    /// A one-signal source of a persistent bump (old → new).
    struct OneSource;
    impl SignalSource for OneSource {
        fn signals(&self) -> Vec<UpdateSignal> {
            vec![UpdateSignal::new("flake-input", "blackmatter")]
        }
    }
    struct Env;
    impl UpdateEnv for Env {
        fn current(&self, _s: &UpdateSignal) -> Option<String> {
            Some("old".into())
        }
        fn latest(&self, _s: &UpdateSignal) -> Result<String, BlockReason> {
            Ok("new".into())
        }
    }

    fn daemon(frozen: bool, confirm_after: u64) -> SwarmDaemon<MemPlanStore, FixedClock, CollectingSink> {
        SwarmDaemon::new(
            Swarm::new(
                Colony::new()
                    .register(
                        Box::new(FlakeInputKind),
                        PromotionPolicy::new(PromotionMode::ShadowConfirmEffect)
                            .confirm_after(confirm_after),
                    )
                    .frozen(frozen),
                MemPlanStore::new(),
            ),
            FixedClock(Cell::new(0)),
            CollectingSink::default(),
        )
    }

    #[test]
    fn tick_uses_the_clock_and_emits_to_the_sink() {
        let mut d = daemon(false, 600);
        d.clock.0.set(10_000);
        let report = d.tick(&OneSource, &Env);
        assert_eq!(report.at_epoch, 10_000, "cycle stamped with the clock's now");
        assert_eq!(report.shadowed, 1);
        // the sink saw exactly that report.
        let seen = d.sink.0.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].at_epoch, 10_000);
    }

    #[test]
    fn advancing_the_clock_matures_the_window_across_ticks() {
        // ShadowConfirmEffect 600s: tick at 10_000 holds, tick at 10_600 applies —
        // driven entirely by advancing the FixedClock (no real time).
        let mut d = daemon(false, 600);
        d.clock.0.set(10_000);
        let held = d.tick(&OneSource, &Env);
        assert!(matches!(
            held.outcomes[0].tick_outcome(),
            Some(crate::TickOutcome::Shadowed { reason: ShadowReason::ConfirmPending { .. }, .. })
        ));
        d.clock.0.set(10_600);
        let applied = d.tick(&OneSource, &Env);
        assert_eq!(applied.applied, 1, "window elapsed via the clock → apply");
        // The static test env keeps reporting old→new, so the plan still shows the
        // target ready-to-apply; a real daemon's next-cycle env would observe the
        // new rev → UpToDate → the target clears. pending_plan is queryable off the
        // owned swarm between ticks.
        assert_eq!(d.swarm().pending_plan(10_600).ready_to_apply(), 1);
        assert_eq!(d.sink.0.borrow().len(), 2, "one emission per tick");
    }

    #[test]
    fn a_frozen_daemon_never_applies_however_far_the_clock_runs() {
        let mut d = daemon(true, 0); // frozen, zero window
        for t in [1_i64, 100, 10_000, 1_000_000] {
            d.clock.0.set(t);
            let r = d.tick(&OneSource, &Env);
            assert_eq!(r.applied, 0);
            assert_eq!(r.shadowed, 1);
        }
        assert_eq!(d.sink.0.borrow().len(), 4);
    }

    #[test]
    fn null_sink_is_a_noop() {
        let mut d = SwarmDaemon::new(
            Swarm::new(
                Colony::new().register(
                    Box::new(FlakeInputKind),
                    PromotionPolicy::new(PromotionMode::Effect),
                ),
                MemPlanStore::new(),
            ),
            SystemClock,
            NullSink,
        );
        // system clock stamps a real (nonzero) now; the report still returns.
        let r = d.tick(&OneSource, &Env);
        assert!(r.at_epoch > 0);
        assert_eq!(r.applied, 1);
    }
}
