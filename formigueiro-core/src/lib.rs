//! # formigueiro-core — the pure algebra of the update swarm
//!
//! One *formiga* (ant) carries one small update mutation. This crate is the pure,
//! I/O-free algebra of a formiga's decision:
//!
//! ```text
//!   UpdateSignal ──kind.shadow(env)──▶ ShadowOutcome ──Formiga::tick + outorga──▶ TickOutcome
//! ```
//!
//! - **`UpdateKind`** — a class of update (flake-input, image-tag, …). Its
//!   `shadow` computes *what it would change* — never writing.
//! - **`UpdateEnv`** — the side-effect boundary (observe current + latest). A
//!   trait, so tests mock it and the algebra stays pure (the TYPED-SPEC triplet's
//!   Environment contract — the trait IS the testability guarantee).
//! - **`Formiga::tick`** — composes the shadow outcome with an
//!   [`outorga::PromotionPolicy`] decision into one typed [`TickOutcome`]. A real
//!   apply is reachable *only* through the promotion decision; the M0 swarm runs
//!   every formiga frozen (shadow-only), so nothing writes until promoted.
//!
//! Nothing here does I/O, spawns a task, or holds a clock — it is a total typed
//! function of (signal, env, observation, now, frozen). Every outcome is a table
//! test.

use outorga::{Observation, PromotionDecision, PromotionPolicy, ShadowReason};
use serde::{Deserialize, Serialize};

pub mod colony;
pub mod plan_store;
pub mod swarm;
pub use colony::{Colony, ColonyOutcome};
pub use plan_store::{fold, MemPlanStore, PlanStore, TargetState};
pub use swarm::{Swarm, SwarmReport};

/// An ingested update event: "input `subject` of kind `kind` may have moved to
/// `revision`". `revision` is a hint (the event's claimed new value); the kind's
/// shadow re-observes the truth from the [`UpdateEnv`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSignal {
    /// The update kind's name (e.g. `flake-input`).
    pub kind: String,
    /// The thing to update (e.g. an input name, an image ref).
    pub subject: String,
    /// The event's claimed new value, if any (a hint — the shadow re-observes).
    pub revision: Option<String>,
}

impl UpdateSignal {
    /// A signal for `kind`/`subject` with no revision hint.
    #[must_use]
    pub fn new(kind: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            subject: subject.into(),
            revision: None,
        }
    }
}

/// Why a shadow could not compute a target — a typed, exhaustive reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "block")]
pub enum BlockReason {
    /// Local and upstream diverged; no fast-forward target.
    Diverged,
    /// The upstream could not be reached.
    Unreachable,
    /// Another writer owns the subject.
    Conflict,
    /// Any other typed error.
    Error(String),
}

/// The shadow-computed result of one update — WITHOUT writing anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "outcome", content = "detail")]
pub enum ShadowOutcome {
    /// Already at the latest reachable value — nothing to do.
    UpToDate,
    /// The mutation the formiga *would* make.
    WouldApply {
        /// The current value.
        from: String,
        /// The value it would become.
        to: String,
    },
    /// Could not compute a target.
    Blocked(BlockReason),
}

/// The side-effect boundary: observe the current and latest-reachable value for a
/// signal. A trait so every consumer plugs its own source (git, a registry, a
/// lockfile) and tests mock it — no real network in a unit test.
pub trait UpdateEnv {
    /// The currently locked/pinned value for the signal, if any.
    fn current(&self, sig: &UpdateSignal) -> Option<String>;
    /// The latest reachable upstream value, or why it could not be observed.
    ///
    /// # Errors
    /// Returns a [`BlockReason`] when the upstream can't be reached / resolved.
    fn latest(&self, sig: &UpdateSignal) -> Result<String, BlockReason>;
}

/// A class of update that knows how to shadow-compute itself.
pub trait UpdateKind {
    /// The kind's stable name (matched against [`UpdateSignal::kind`]).
    fn name(&self) -> &str;
    /// Compute what this update *would* do, without writing.
    fn shadow(&self, sig: &UpdateSignal, env: &dyn UpdateEnv) -> ShadowOutcome;
}

/// The default shadow most kinds reuse: compare `current` vs `latest`; a
/// difference is a [`ShadowOutcome::WouldApply`], equality is
/// [`ShadowOutcome::UpToDate`]. A brand-new subject (no current) is a WouldApply
/// from the empty string.
#[must_use]
pub fn diff_shadow(sig: &UpdateSignal, env: &dyn UpdateEnv) -> ShadowOutcome {
    let latest = match env.latest(sig) {
        Ok(v) => v,
        Err(r) => return ShadowOutcome::Blocked(r),
    };
    let current = env.current(sig).unwrap_or_default();
    if current == latest {
        ShadowOutcome::UpToDate
    } else {
        ShadowOutcome::WouldApply {
            from: current,
            to: latest,
        }
    }
}

/// The `flake-input` update kind — "bump this flake input from its locked hash to
/// the upstream head." Shadow = the default diff.
#[derive(Clone, Copy, Debug, Default)]
pub struct FlakeInputKind;

impl UpdateKind for FlakeInputKind {
    fn name(&self) -> &str {
        "flake-input"
    }
    fn shadow(&self, sig: &UpdateSignal, env: &dyn UpdateEnv) -> ShadowOutcome {
        diff_shadow(sig, env)
    }
}

/// One formiga tick's typed outcome — the composition of a shadow with a
/// promotion decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "tick", content = "detail")]
pub enum TickOutcome {
    /// Nothing to do — already current.
    UpToDate,
    /// A mutation is available but held in shadow (compute-and-report only), with
    /// the promotion reason. The M0 swarm lives here (every formiga frozen).
    Shadowed {
        /// The mutation that would be applied once promoted.
        from: String,
        /// …to this value.
        to: String,
        /// Why it is held (from [`outorga`]).
        reason: ShadowReason,
    },
    /// Promoted and unfrozen — a real apply is authorized (M1+).
    Applied {
        /// The applied mutation.
        from: String,
        /// …to this value.
        to: String,
    },
    /// Could not even compute a target.
    Blocked(BlockReason),
}

impl TickOutcome {
    /// `true` iff a real apply was authorized this tick.
    #[must_use]
    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }
    /// `true` iff a mutation exists but was held in shadow.
    #[must_use]
    pub fn is_shadowed(&self) -> bool {
        matches!(self, Self::Shadowed { .. })
    }
}

/// A formiga: given a promotion policy, turn a signal into a typed [`TickOutcome`]
/// by shadowing the update and (if there is a mutation) running the
/// [`outorga::PromotionPolicy`] decision. This is the whole per-event algebra.
#[derive(Clone, Copy, Debug)]
pub struct Formiga {
    policy: PromotionPolicy,
}

impl Formiga {
    /// A formiga governed by `policy`.
    #[must_use]
    pub fn new(policy: PromotionPolicy) -> Self {
        Self { policy }
    }

    /// The promotion policy this formiga applies.
    #[must_use]
    pub fn policy(&self) -> PromotionPolicy {
        self.policy
    }

    /// One tick: shadow `sig` through `kind`/`env`; if there is a mutation, decide
    /// promotion via `outorga` (two-key: the observation's window AND `frozen`).
    /// UpToDate / Blocked short-circuit before any promotion decision.
    pub fn tick(
        &self,
        kind: &dyn UpdateKind,
        sig: &UpdateSignal,
        env: &dyn UpdateEnv,
        obs: &impl Observation,
        now_epoch: i64,
        frozen: bool,
    ) -> TickOutcome {
        self.decide(kind.shadow(sig, env), obs, now_epoch, frozen)
    }

    /// Decide from an ALREADY-computed shadow outcome — the post-shadow half of
    /// [`Formiga::tick`], for callers that already hold the [`ShadowOutcome`] (e.g.
    /// a [`crate::PlanStore`]-driven tick that must fold the outcome into the store
    /// before deciding, and must not shadow twice). UpToDate / Blocked
    /// short-circuit; a WouldApply runs the two-key promotion decision.
    #[must_use]
    pub fn decide(
        &self,
        outcome: ShadowOutcome,
        obs: &impl Observation,
        now_epoch: i64,
        frozen: bool,
    ) -> TickOutcome {
        match outcome {
            ShadowOutcome::UpToDate => TickOutcome::UpToDate,
            ShadowOutcome::Blocked(r) => TickOutcome::Blocked(r),
            ShadowOutcome::WouldApply { from, to } => {
                match self.policy.decide(obs, now_epoch, frozen) {
                    PromotionDecision::Apply => TickOutcome::Applied { from, to },
                    PromotionDecision::Shadow(reason) => TickOutcome::Shadowed { from, to, reason },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use outorga::PromotionMode;

    /// A mock env: a fixed current + a latest that can be Ok or Blocked.
    struct MockEnv {
        current: Option<&'static str>,
        latest: Result<&'static str, BlockReason>,
    }
    impl UpdateEnv for MockEnv {
        fn current(&self, _sig: &UpdateSignal) -> Option<String> {
            self.current.map(String::from)
        }
        fn latest(&self, _sig: &UpdateSignal) -> Result<String, BlockReason> {
            self.latest.clone().map(String::from)
        }
    }

    /// A hand-built observation for the promotion side.
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
    fn sig() -> UpdateSignal {
        UpdateSignal::new("flake-input", "blackmatter")
    }

    #[test]
    fn shadow_uptodate_when_current_equals_latest() {
        let env = MockEnv {
            current: Some("abc"),
            latest: Ok("abc"),
        };
        assert_eq!(FlakeInputKind.shadow(&sig(), &env), ShadowOutcome::UpToDate);
    }

    #[test]
    fn shadow_wouldapply_on_difference_and_from_empty_for_new_subject() {
        let bumped = MockEnv {
            current: Some("old"),
            latest: Ok("new"),
        };
        assert_eq!(
            FlakeInputKind.shadow(&sig(), &bumped),
            ShadowOutcome::WouldApply {
                from: "old".into(),
                to: "new".into()
            }
        );
        let fresh = MockEnv {
            current: None,
            latest: Ok("new"),
        };
        assert_eq!(
            FlakeInputKind.shadow(&sig(), &fresh),
            ShadowOutcome::WouldApply {
                from: String::new(),
                to: "new".into()
            }
        );
    }

    #[test]
    fn shadow_blocked_propagates_the_reason() {
        let env = MockEnv {
            current: Some("x"),
            latest: Err(BlockReason::Diverged),
        };
        assert_eq!(
            FlakeInputKind.shadow(&sig(), &env),
            ShadowOutcome::Blocked(BlockReason::Diverged)
        );
    }

    #[test]
    fn tick_uptodate_and_blocked_short_circuit_before_promotion() {
        // Even with an Effect policy (would apply), UpToDate/Blocked never reach it.
        let f = Formiga::new(PromotionPolicy::new(PromotionMode::Effect));
        let obs = Obs::default();
        let up = MockEnv {
            current: Some("v"),
            latest: Ok("v"),
        };
        assert_eq!(
            f.tick(&FlakeInputKind, &sig(), &up, &obs, NOW, false),
            TickOutcome::UpToDate
        );
        let blocked = MockEnv {
            current: Some("v"),
            latest: Err(BlockReason::Unreachable),
        };
        assert_eq!(
            f.tick(&FlakeInputKind, &sig(), &blocked, &obs, NOW, false),
            TickOutcome::Blocked(BlockReason::Unreachable)
        );
    }

    #[test]
    fn tick_wouldapply_effect_applies() {
        let f = Formiga::new(PromotionPolicy::new(PromotionMode::Effect));
        let env = MockEnv {
            current: Some("old"),
            latest: Ok("new"),
        };
        assert_eq!(
            f.tick(&FlakeInputKind, &sig(), &env, &Obs::default(), NOW, false),
            TickOutcome::Applied {
                from: "old".into(),
                to: "new".into()
            }
        );
    }

    #[test]
    fn tick_m0_shadow_only_frozen_always_shadows_even_in_effect() {
        // The M0 swarm runs frozen: a mutation is computed but never applied.
        let f = Formiga::new(PromotionPolicy::new(PromotionMode::Effect));
        let env = MockEnv {
            current: Some("old"),
            latest: Ok("new"),
        };
        let out = f.tick(&FlakeInputKind, &sig(), &env, &Obs::default(), NOW, true);
        assert_eq!(
            out,
            TickOutcome::Shadowed {
                from: "old".into(),
                to: "new".into(),
                reason: ShadowReason::Frozen
            }
        );
        assert!(out.is_shadowed() && !out.is_applied());
    }

    #[test]
    fn tick_shadow_confirm_effect_holds_until_window_then_applies() {
        let f = Formiga::new(PromotionPolicy::new(PromotionMode::ShadowConfirmEffect).confirm_after(1800));
        let env = MockEnv {
            current: Some("old"),
            latest: Ok("new"),
        };
        // ready 1000s < 1800 → held with ConfirmPending
        let pending = Obs {
            ready_since: Some(NOW - 1000),
            ..Obs::default()
        };
        match f.tick(&FlakeInputKind, &sig(), &env, &pending, NOW, false) {
            TickOutcome::Shadowed {
                reason: ShadowReason::ConfirmPending { held_secs, need_secs },
                ..
            } => {
                assert_eq!((held_secs, need_secs), (1000, 1800));
            }
            other => panic!("expected ConfirmPending, got {other:?}"),
        }
        // window elapsed → applies
        let ready = Obs {
            ready_since: Some(NOW - 1800),
            ..Obs::default()
        };
        assert!(f
            .tick(&FlakeInputKind, &sig(), &env, &ready, NOW, false)
            .is_applied());
    }

    #[test]
    fn tick_conflict_holds_the_apply() {
        let f = Formiga::new(PromotionPolicy::new(PromotionMode::ShadowConfirmEffect));
        let env = MockEnv {
            current: Some("old"),
            latest: Ok("new"),
        };
        let conflicted = Obs {
            ready_since: Some(NOW - 5000),
            conflict: true,
            ..Obs::default()
        };
        assert!(matches!(
            f.tick(&FlakeInputKind, &sig(), &env, &conflicted, NOW, false),
            TickOutcome::Shadowed {
                reason: ShadowReason::Conflict,
                ..
            }
        ));
    }

    #[test]
    fn outcomes_serialize_to_stable_typed_json() {
        assert_eq!(
            serde_json::to_string(&TickOutcome::UpToDate).unwrap(),
            r#"{"tick":"upToDate"}"#
        );
        assert_eq!(
            serde_json::to_string(&ShadowOutcome::Blocked(BlockReason::Diverged)).unwrap(),
            r#"{"outcome":"blocked","detail":{"block":"diverged"}}"#
        );
    }
}
