//! # constrain — compose an update kind with a value constraint
//!
//! By default an [`UpdateKind`] surfaces a bump to whatever upstream value it
//! observes. Operators often want *"bump, but only to values I allow"* — skip
//! pre-releases, pin to a branch prefix, honor a denylist. [`ConstrainedKind`] is
//! that policy as a **decorator** over the [`UpdateKind`] trait: it wraps any kind
//! and, when the inner shadow yields a [`ShadowOutcome::WouldApply`] whose target
//! the [`Constraint`] disallows, converts it to [`ShadowOutcome::Blocked`] with
//! [`BlockReason::Constrained`] — so a forbidden bump is *reported* (visible in the
//! report as blocked) and **never promoted**, rather than silently hidden or blindly
//! taken. `UpToDate` / `Blocked` pass through unchanged.
//!
//! Because it is a decorator over the trait, it composes with *every* kind (present
//! and future) and stacks (a kind can carry several constraints by nesting).

use crate::{BlockReason, ShadowOutcome, UpdateEnv, UpdateKind, UpdateSignal};

/// A predicate on a candidate target value: *may the swarm bump to this?*
pub trait Constraint {
    /// Whether bumping to `to` is allowed.
    fn allows(&self, to: &str) -> bool;
}

/// Wrap an update kind so a `WouldApply` is surfaced only when its target passes
/// `constraint`; a disallowed target becomes `Blocked(Constrained)`.
#[derive(Clone, Copy, Debug)]
pub struct ConstrainedKind<K, C> {
    inner: K,
    constraint: C,
}

impl<K, C> ConstrainedKind<K, C> {
    /// Wrap `inner` with `constraint`.
    pub fn new(inner: K, constraint: C) -> Self {
        Self { inner, constraint }
    }
}

impl<K: UpdateKind, C: Constraint> UpdateKind for ConstrainedKind<K, C> {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn shadow(&self, sig: &UpdateSignal, env: &dyn UpdateEnv) -> ShadowOutcome {
        match self.inner.shadow(sig, env) {
            ShadowOutcome::WouldApply { to, .. } if !self.constraint.allows(&to) => {
                ShadowOutcome::Blocked(BlockReason::Constrained)
            }
            other => other,
        }
    }
}

/// A constraint that allows everything (the identity — a no-op wrap).
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl Constraint for AllowAll {
    fn allows(&self, _to: &str) -> bool {
        true
    }
}

/// A constraint that rejects any target containing one of the blocked substrings —
/// e.g. `Blocklist::new(["rc", "alpha", "beta", "-pre"])` to skip pre-releases.
#[derive(Clone, Debug, Default)]
pub struct Blocklist {
    blocked: Vec<String>,
}

impl Blocklist {
    /// A blocklist of the given substrings.
    pub fn new(blocked: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            blocked: blocked.into_iter().map(Into::into).collect(),
        }
    }
}

impl Constraint for Blocklist {
    fn allows(&self, to: &str) -> bool {
        !self.blocked.iter().any(|b| to.contains(b.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockReason, FlakeInputKind, UpdateSignal};

    struct Env {
        current: &'static str,
        latest: &'static str,
    }
    impl UpdateEnv for Env {
        fn current(&self, _s: &UpdateSignal) -> Option<String> {
            Some(self.current.into())
        }
        fn latest(&self, _s: &UpdateSignal) -> Result<String, BlockReason> {
            Ok(self.latest.into())
        }
    }

    fn sig() -> UpdateSignal {
        UpdateSignal::new("flake-input", "x")
    }

    #[test]
    fn a_disallowed_target_becomes_blocked_constrained_not_a_bump() {
        // upstream moved to a pre-release; the blocklist forbids it.
        let kind = ConstrainedKind::new(FlakeInputKind, Blocklist::new(["rc"]));
        let env = Env {
            current: "v1.0",
            latest: "v1.1-rc1",
        };
        assert_eq!(
            kind.shadow(&sig(), &env),
            ShadowOutcome::Blocked(BlockReason::Constrained)
        );
    }

    #[test]
    fn an_allowed_target_passes_through_as_a_normal_bump() {
        let kind = ConstrainedKind::new(FlakeInputKind, Blocklist::new(["rc"]));
        let env = Env {
            current: "v1.0",
            latest: "v1.1",
        };
        assert_eq!(
            kind.shadow(&sig(), &env),
            ShadowOutcome::WouldApply {
                from: "v1.0".into(),
                to: "v1.1".into()
            }
        );
    }

    #[test]
    fn uptodate_and_name_pass_through_unchanged() {
        let kind = ConstrainedKind::new(FlakeInputKind, Blocklist::new(["rc"]));
        assert_eq!(kind.name(), "flake-input");
        let env = Env {
            current: "same",
            latest: "same",
        };
        assert_eq!(kind.shadow(&sig(), &env), ShadowOutcome::UpToDate);
    }

    #[test]
    fn allow_all_is_the_identity_and_blocklist_matches_substrings() {
        assert!(AllowAll.allows("anything-rc-beta"));
        let bl = Blocklist::new(["alpha", "beta"]);
        assert!(bl.allows("v2.0.0"));
        assert!(!bl.allows("v2.0.0-beta.1"));
        assert!(!bl.allows("v2.0.0-alpha"));
    }
}
