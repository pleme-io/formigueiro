//! # outorga — the generic progressive-authority promotion FSM
//!
//! *outorga* (Brazilian-Portuguese: the formal **granting** of an authority or a
//! concession) — the machine that grants a target *more control over itself as
//! trust is earned*: it starts by only **observing** (shadow), and once a
//! clean-observation window proves it safe, it **grants** the authority to
//! **apply** for real (effect). A single fleet **freeze** re-shadows everything
//! instantly.
//!
//! This is a **k8s-free lift** of breathe's promotion lifecycle
//! (`breathe-crd::Band::{promotion_mode,confirm_gate_passed,effective_dry_run}`,
//! the `ShadowConfirmEffect` default at 1800 s). breathe stays coupled to its
//! bands + carve; the *promotion algebra itself* lives here, so both
//! **formigueiro** (fleet updates) and **breathe** (resource homeostasis) — and
//! any future consumer that must shadow-then-progressively-apply — stand on one
//! tested abstraction.
//!
//! The FSM is **pure**: [`PromotionPolicy::decide`] takes the observation and
//! the current epoch explicitly and returns a typed [`PromotionDecision`]. No
//! I/O, no clock reads inside the decision — so every reachable outcome is a
//! table-driven unit test. A [`Clock`] trait is provided for the *driver* layer
//! (with [`SystemClock`] / [`FixedClock`]), never for the decision itself.
//!
//! ## The one law
//!
//! `apply` is reachable only when the target is **promoted** AND the fleet is
//! **not frozen** — the two-key rule. A blind apply (write without a promotion
//! decision) is not expressible: [`PromotionPolicy::decide`] is the only door,
//! and it always returns a *typed reason* when it holds in shadow.

use serde::{Deserialize, Serialize};

/// How a target moves from observing (shadow) to acting (effect).
///
/// Mirrors breathe's `PromotionMode`. The fleet default is
/// [`PromotionMode::ShadowConfirmEffect`]: no target is parked in permanent
/// shadow, and none goes live unconfirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PromotionMode {
    /// Observe forever; never apply. For deliberate, eyes-open holds.
    Shadow,
    /// Apply immediately — skip the confirm gate. Explicit go-live.
    Effect,
    /// DEFAULT. Shadow until the confirm gate passes, then apply. The gate is a
    /// clean-observation window (`Ready ∧ ¬Stale ∧ ¬Conflict` held for
    /// `confirm_after_secs`) OR an operator fast-path confirm. Losing readiness
    /// safely re-shadows.
    #[default]
    ShadowConfirmEffect,
    /// Frozen — never apply AND stop deciding.
    Suspended,
}

/// The fleet default clean-observation window, in seconds (breathe parity: 1800).
pub const DEFAULT_CONFIRM_AFTER_SECS: u64 = 1800;

/// The observed health of a promotion target at an instant. A trait so every
/// consumer plugs its own observation source (formigueiro's shadow-plan result,
/// breathe's band conditions, …) while the FSM stays domain-agnostic.
pub trait Observation {
    /// The target is enrolled and healthy (metric present, config parses, no error).
    fn ready(&self) -> bool;
    /// The target's signal is too old to trust.
    fn stale(&self) -> bool;
    /// Another writer owns the target (a field-manager / lease collision).
    fn conflict(&self) -> bool;
    /// Unix epoch (secs) at which [`Observation::ready`] most recently became
    /// true; `None` when not currently ready.
    fn ready_since(&self) -> Option<i64>;
    /// Operator fast-path: an explicit confirm promotes immediately.
    fn operator_confirmed(&self) -> bool;
}

/// A typed, exhaustive reason a target is held in shadow rather than applied.
/// Consumers surface this verbatim (status, receipt, log) — never a bare bool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "reason")]
pub enum ShadowReason {
    /// The fleet master switch (freeze) is engaged — overrides every mode.
    Frozen,
    /// Explicit [`PromotionMode::Shadow`].
    ModeShadow,
    /// Explicit [`PromotionMode::Suspended`].
    Suspended,
    /// Observation is not [`Observation::ready`].
    NotReady,
    /// Observation is [`Observation::stale`].
    Stale,
    /// Observation reports a [`Observation::conflict`].
    Conflict,
    /// `ShadowConfirmEffect` and the clean-observation window has not yet elapsed.
    ConfirmPending {
        /// How long the target has held Ready-and-healthy so far.
        held_secs: i64,
        /// How long it must hold before promotion.
        need_secs: i64,
    },
}

/// The FSM's typed decision for one tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "decision", content = "detail")]
pub enum PromotionDecision {
    /// Promoted and unfrozen — a real apply is authorized.
    Apply,
    /// Held in shadow: compute-and-report only, never write. Carries the reason.
    Shadow(ShadowReason),
}

impl PromotionDecision {
    /// `true` iff a real apply is authorized this tick.
    #[must_use]
    pub fn is_apply(self) -> bool {
        matches!(self, Self::Apply)
    }

    /// `true` iff the target is held in shadow this tick.
    #[must_use]
    pub fn is_shadow(self) -> bool {
        matches!(self, Self::Shadow(_))
    }

    /// The shadow reason, if held.
    #[must_use]
    pub fn shadow_reason(self) -> Option<ShadowReason> {
        match self {
            Self::Shadow(r) => Some(r),
            Self::Apply => None,
        }
    }
}

/// The intermediate confirm-gate outcome (kept typed so consumers can surface
/// the exact blocking condition or the remaining window).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmGate {
    /// The window has elapsed (or the operator confirmed) — promotion allowed.
    Passed,
    /// A hard condition (not-ready / stale / conflict) blocks promotion.
    Blocked(ShadowReason),
    /// Ready-and-healthy, but the window has not yet elapsed.
    Pending {
        /// Seconds held so far (clamped ≥ 0).
        held_secs: i64,
        /// Seconds required.
        need_secs: i64,
    },
}

/// The promotion policy for one target: its mode + its clean-window length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionPolicy {
    /// The lifecycle mode.
    pub mode: PromotionMode,
    /// The clean-observation window a `ShadowConfirmEffect` target holds before
    /// auto-promoting.
    pub confirm_after_secs: u64,
}

impl Default for PromotionPolicy {
    fn default() -> Self {
        Self {
            mode: PromotionMode::default(),
            confirm_after_secs: DEFAULT_CONFIRM_AFTER_SECS,
        }
    }
}

impl PromotionPolicy {
    /// A policy in `mode` with the default confirm window.
    #[must_use]
    pub fn new(mode: PromotionMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Builder: override the confirm window.
    #[must_use]
    pub fn confirm_after(mut self, secs: u64) -> Self {
        self.confirm_after_secs = secs;
        self
    }

    /// Evaluate the `ShadowConfirmEffect` confirm gate against an observation.
    ///
    /// Passes iff the operator confirmed, OR `Ready ∧ ¬Stale ∧ ¬Conflict` has
    /// held continuously for `confirm_after_secs`. A `Stale`/`Conflict`/`NotReady`
    /// observation blocks (and, on loss of readiness, safely re-shadows). This is
    /// breathe's `confirm_gate_passed` generalized to any [`Observation`].
    #[must_use]
    pub fn confirm_gate(&self, obs: &impl Observation, now_epoch: i64) -> ConfirmGate {
        if obs.operator_confirmed() {
            return ConfirmGate::Passed;
        }
        if obs.stale() {
            return ConfirmGate::Blocked(ShadowReason::Stale);
        }
        if obs.conflict() {
            return ConfirmGate::Blocked(ShadowReason::Conflict);
        }
        match obs.ready_since() {
            Some(since) => {
                let held = now_epoch - since;
                let need = i64::try_from(self.confirm_after_secs).unwrap_or(i64::MAX);
                if held >= need {
                    ConfirmGate::Passed
                } else {
                    ConfirmGate::Pending {
                        held_secs: held.max(0),
                        need_secs: need,
                    }
                }
            }
            None => ConfirmGate::Blocked(ShadowReason::NotReady),
        }
    }

    /// The full **two-key** decision for one tick: the target's own promotion gate
    /// AND the fleet master switch. `frozen == true` shadows unconditionally
    /// (breathe's `!writeEnabled`). This is the *only* door to an apply.
    #[must_use]
    pub fn decide(
        &self,
        obs: &impl Observation,
        now_epoch: i64,
        frozen: bool,
    ) -> PromotionDecision {
        if frozen {
            return PromotionDecision::Shadow(ShadowReason::Frozen);
        }
        match self.mode {
            PromotionMode::Effect => PromotionDecision::Apply,
            PromotionMode::Shadow => PromotionDecision::Shadow(ShadowReason::ModeShadow),
            PromotionMode::Suspended => PromotionDecision::Shadow(ShadowReason::Suspended),
            PromotionMode::ShadowConfirmEffect => match self.confirm_gate(obs, now_epoch) {
                ConfirmGate::Passed => PromotionDecision::Apply,
                ConfirmGate::Blocked(r) => PromotionDecision::Shadow(r),
                ConfirmGate::Pending {
                    held_secs,
                    need_secs,
                } => PromotionDecision::Shadow(ShadowReason::ConfirmPending {
                    held_secs,
                    need_secs,
                }),
            },
        }
    }

    /// The effective dry-run for this tick — `true` ⇒ compute-and-report only,
    /// never write. breathe's `effective_dry_run`, made two-key.
    #[must_use]
    pub fn effective_dry_run(&self, obs: &impl Observation, now_epoch: i64, frozen: bool) -> bool {
        self.decide(obs, now_epoch, frozen).is_shadow()
    }
}

/// A clock abstraction for the *driver* layer (the FSM decision itself is pure
/// and takes `now_epoch` explicitly). [`SystemClock`] in production,
/// [`FixedClock`] in tests.
pub trait Clock {
    /// The current Unix epoch, in seconds.
    fn now_epoch(&self) -> i64;
}

/// Wall-clock time.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_epoch(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// A frozen clock for tests.
#[derive(Clone, Copy, Debug)]
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now_epoch(&self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built observation for table-driven tests.
    #[derive(Clone, Copy, Default)]
    struct Obs {
        ready_since: Option<i64>,
        stale: bool,
        conflict: bool,
        confirmed: bool,
    }

    impl Observation for Obs {
        fn ready(&self) -> bool {
            self.ready_since.is_some()
        }
        fn stale(&self) -> bool {
            self.stale
        }
        fn conflict(&self) -> bool {
            self.conflict
        }
        fn ready_since(&self) -> Option<i64> {
            self.ready_since
        }
        fn operator_confirmed(&self) -> bool {
            self.confirmed
        }
    }

    const NOW: i64 = 1_000_000;

    fn scep() -> PromotionPolicy {
        PromotionPolicy::new(PromotionMode::ShadowConfirmEffect).confirm_after(1800)
    }

    #[test]
    fn default_mode_is_shadow_confirm_effect() {
        assert_eq!(PromotionMode::default(), PromotionMode::ShadowConfirmEffect);
        assert_eq!(
            PromotionPolicy::default().confirm_after_secs,
            DEFAULT_CONFIRM_AFTER_SECS
        );
    }

    #[test]
    fn effect_applies_regardless_of_observation() {
        let p = PromotionPolicy::new(PromotionMode::Effect);
        let never_ready = Obs::default();
        assert_eq!(p.decide(&never_ready, NOW, false), PromotionDecision::Apply);
    }

    #[test]
    fn explicit_shadow_and_suspended_never_apply() {
        let ready_forever = Obs {
            ready_since: Some(0),
            confirmed: true,
            ..Obs::default()
        };
        assert_eq!(
            PromotionPolicy::new(PromotionMode::Shadow).decide(&ready_forever, NOW, false),
            PromotionDecision::Shadow(ShadowReason::ModeShadow)
        );
        assert_eq!(
            PromotionPolicy::new(PromotionMode::Suspended).decide(&ready_forever, NOW, false),
            PromotionDecision::Shadow(ShadowReason::Suspended)
        );
    }

    #[test]
    fn freeze_is_the_master_switch_overriding_every_mode() {
        let ready = Obs {
            ready_since: Some(0),
            confirmed: true,
            ..Obs::default()
        };
        for mode in [
            PromotionMode::Shadow,
            PromotionMode::Effect,
            PromotionMode::ShadowConfirmEffect,
            PromotionMode::Suspended,
        ] {
            assert_eq!(
                PromotionPolicy::new(mode).decide(&ready, NOW, true),
                PromotionDecision::Shadow(ShadowReason::Frozen),
                "frozen must shadow in mode {mode:?}"
            );
        }
    }

    #[test]
    fn operator_confirm_is_the_fast_path() {
        let obs = Obs {
            ready_since: Some(NOW), // held 0s — window NOT met
            confirmed: true,
            ..Obs::default()
        };
        assert_eq!(scep().decide(&obs, NOW, false), PromotionDecision::Apply);
    }

    #[test]
    fn confirm_pending_until_window_elapses_then_applies() {
        // ready since NOW-1000; need 1800 → still pending
        let pending = Obs {
            ready_since: Some(NOW - 1000),
            ..Obs::default()
        };
        assert_eq!(
            scep().decide(&pending, NOW, false),
            PromotionDecision::Shadow(ShadowReason::ConfirmPending {
                held_secs: 1000,
                need_secs: 1800
            })
        );
        // exactly at the boundary → applies
        let at_boundary = Obs {
            ready_since: Some(NOW - 1800),
            ..Obs::default()
        };
        assert_eq!(
            scep().decide(&at_boundary, NOW, false),
            PromotionDecision::Apply
        );
        // well past → applies
        let past = Obs {
            ready_since: Some(NOW - 5000),
            ..Obs::default()
        };
        assert_eq!(scep().decide(&past, NOW, false), PromotionDecision::Apply);
    }

    #[test]
    fn stale_and_conflict_block_even_when_window_elapsed() {
        let long_ready_stale = Obs {
            ready_since: Some(NOW - 5000),
            stale: true,
            ..Obs::default()
        };
        assert_eq!(
            scep().decide(&long_ready_stale, NOW, false),
            PromotionDecision::Shadow(ShadowReason::Stale)
        );
        let long_ready_conflict = Obs {
            ready_since: Some(NOW - 5000),
            conflict: true,
            ..Obs::default()
        };
        assert_eq!(
            scep().decide(&long_ready_conflict, NOW, false),
            PromotionDecision::Shadow(ShadowReason::Conflict)
        );
    }

    #[test]
    fn not_ready_shadows_and_losing_readiness_safely_re_shadows() {
        // never ready
        let never = Obs::default();
        assert_eq!(
            scep().decide(&never, NOW, false),
            PromotionDecision::Shadow(ShadowReason::NotReady)
        );
        // a previously-promotable target that loses its signal re-shadows:
        // ready_since None ⇒ NotReady, regardless of prior state (FSM is per-tick pure).
        let lost = Obs {
            ready_since: None,
            ..Obs::default()
        };
        assert!(scep().decide(&lost, NOW, false).is_shadow());
    }

    #[test]
    fn stale_beats_conflict_beats_pending_ordering() {
        // both stale and conflict → stale reported first (breathe order)
        let both = Obs {
            ready_since: Some(NOW - 5000),
            stale: true,
            conflict: true,
            ..Obs::default()
        };
        assert_eq!(
            scep().confirm_gate(&both, NOW),
            ConfirmGate::Blocked(ShadowReason::Stale)
        );
    }

    #[test]
    fn effective_dry_run_agrees_with_decide() {
        let cases = [
            (PromotionMode::Effect, false, false),
            (PromotionMode::Shadow, false, true),
            (PromotionMode::Suspended, false, true),
            (PromotionMode::Effect, true, true), // frozen
        ];
        let ready = Obs {
            ready_since: Some(0),
            ..Obs::default()
        };
        for (mode, frozen, want_shadow) in cases {
            let p = PromotionPolicy::new(mode);
            assert_eq!(
                p.effective_dry_run(&ready, NOW, frozen),
                want_shadow,
                "mode {mode:?} frozen {frozen}"
            );
            assert_eq!(p.decide(&ready, NOW, frozen).is_shadow(), want_shadow);
        }
    }

    #[test]
    fn held_secs_never_negative_on_clock_skew() {
        // ready_since in the future (clock skew) → held clamped to 0, still pending
        let future = Obs {
            ready_since: Some(NOW + 500),
            ..Obs::default()
        };
        match scep().decide(&future, NOW, false) {
            PromotionDecision::Shadow(ShadowReason::ConfirmPending { held_secs, .. }) => {
                assert_eq!(held_secs, 0);
            }
            other => panic!("expected ConfirmPending, got {other:?}"),
        }
    }

    #[test]
    fn decisions_serialize_to_stable_typed_json() {
        let apply = serde_json::to_string(&PromotionDecision::Apply).unwrap();
        assert_eq!(apply, r#"{"decision":"apply"}"#);
        let frozen =
            serde_json::to_string(&PromotionDecision::Shadow(ShadowReason::Frozen)).unwrap();
        assert_eq!(frozen, r#"{"decision":"shadow","detail":{"reason":"frozen"}}"#);
        let pending = serde_json::to_string(&PromotionDecision::Shadow(
            ShadowReason::ConfirmPending {
                held_secs: 10,
                need_secs: 1800,
            },
        ))
        .unwrap();
        assert_eq!(
            pending,
            r#"{"decision":"shadow","detail":{"reason":"confirmPending","held_secs":10,"need_secs":1800}}"#
        );
    }

    #[test]
    fn fixed_clock_drives_now() {
        assert_eq!(FixedClock(42).now_epoch(), 42);
    }
}
