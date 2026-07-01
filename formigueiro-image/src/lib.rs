//! # formigueiro-image — the image-tag Environment
//!
//! The **second update domain**, and the proof that [`formigueiro_core`]'s algebra
//! is domain-agnostic: nothing here re-implements shadowing, promotion, pacing, or
//! the swarm — it supplies only the two seams the core observes, exactly as
//! `formigueiro-flake` does, but for container images:
//!
//! - **current** — the pinned digest the swarm tracks for an image ref (from the
//!   caller's pin set; pure, no I/O).
//! - **latest** — the image's upstream digest, resolved via a [`DigestResolver`].
//!
//! The kind is [`formigueiro_core::DiffKind::new`]`("image-tag")` — the same
//! generic "compare current vs latest" shape flake-input uses. [`DigestResolver`]
//! is the Environment seam (mockable); the real [`SkopeoDigestResolver`] runs
//! `skopeo inspect` with a **typed argv (no shell)**. All errors are typed
//! ([`ImageError`]); nothing panics or `format!()`s a composed message.

use std::collections::BTreeMap;
use std::fmt;
use std::process::Command;

use formigueiro_core::{BlockReason, SignalSource, UpdateEnv, UpdateSignal};

/// A typed error from resolving an image digest. Its [`fmt::Display`] impl is the
/// only string-render seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// The upstream registry could not be reached / the ref did not resolve.
    Unresolvable(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolvable(image) => {
                f.write_str("could not resolve image digest: ")?;
                f.write_str(image)
            }
        }
    }
}

impl std::error::Error for ImageError {}

/// Resolve the current upstream digest for an image ref. The **Environment seam**:
/// the real impl runs `skopeo inspect` (typed argv, no shell); tests mock it.
pub trait DigestResolver {
    /// The upstream digest for `image_ref` (e.g. `ghcr.io/pleme-io/hanabi:latest`).
    ///
    /// # Errors
    /// A [`BlockReason`] when the ref can't be resolved.
    fn resolve(&self, image_ref: &str) -> Result<String, BlockReason>;
}

/// The production [`DigestResolver`]: `skopeo inspect --format {{.Digest}}
/// docker://<ref>`, argv-typed, no shell.
#[derive(Clone, Copy, Debug, Default)]
pub struct SkopeoDigestResolver;

impl DigestResolver for SkopeoDigestResolver {
    fn resolve(&self, image_ref: &str) -> Result<String, BlockReason> {
        // build `docker://<ref>` by typed concatenation (no format!)
        let mut target = String::from("docker://");
        target.push_str(image_ref);
        let output = Command::new("skopeo")
            .args(["inspect", "--format", "{{.Digest}}", &target])
            .output()
            .map_err(|_| BlockReason::Unreachable)?;
        if !output.status.success() {
            return Err(BlockReason::Unreachable);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .split_whitespace()
            .next()
            .map(str::to_owned)
            .ok_or(BlockReason::Unreachable)
    }
}

/// The `image-tag` [`UpdateEnv`]: current = the pinned digest from `pins`, latest =
/// the [`DigestResolver`]'s upstream digest. A signal's `subject` is the image ref.
/// `pins` is the caller's tracked state (from a values file, a manifest, a lock —
/// whatever the operator pins from); this crate only observes it.
pub struct ImageEnv<'a, R: DigestResolver> {
    pins: &'a BTreeMap<String, String>,
    resolver: &'a R,
}

impl<'a, R: DigestResolver> ImageEnv<'a, R> {
    /// Compose an env from a pin set + a resolver.
    #[must_use]
    pub fn new(pins: &'a BTreeMap<String, String>, resolver: &'a R) -> Self {
        Self { pins, resolver }
    }
}

impl<R: DigestResolver> UpdateEnv for ImageEnv<'_, R> {
    fn current(&self, sig: &UpdateSignal) -> Option<String> {
        self.pins.get(&sig.subject).cloned()
    }
    fn latest(&self, sig: &UpdateSignal) -> Result<String, BlockReason> {
        self.resolver.resolve(&sig.subject)
    }
}

/// A [`SignalSource`] over the tracked image refs: emits an `image-tag` signal per
/// image — "consider every tracked image this cycle." The ingestion half pairing
/// with [`ImageEnv`].
pub struct ImageSignalSource<'a> {
    images: &'a [String],
}

impl<'a> ImageSignalSource<'a> {
    /// A signal source over `images`.
    #[must_use]
    pub fn new(images: &'a [String]) -> Self {
        Self { images }
    }
}

impl SignalSource for ImageSignalSource<'_> {
    fn signals(&self) -> Vec<UpdateSignal> {
        self.images
            .iter()
            .map(|image| UpdateSignal::new("image-tag", image))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use formigueiro_core::{
        Colony, DiffKind, MemPlanStore, ShadowOutcome, SignalSource, Swarm, UpdateKind,
    };
    use outorga::{PromotionMode, PromotionPolicy};

    struct MockResolver {
        digest: &'static str,
    }
    impl DigestResolver for MockResolver {
        fn resolve(&self, _image_ref: &str) -> Result<String, BlockReason> {
            Ok(self.digest.to_owned())
        }
    }

    fn pins() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("ghcr.io/pleme-io/hanabi:latest".to_owned(), "sha256:aaa".to_owned());
        m
    }

    #[test]
    fn image_env_reports_pinned_current_and_resolved_latest() {
        let pins = pins();
        let env = ImageEnv::new(&pins, &MockResolver { digest: "sha256:bbb" });
        let sig = UpdateSignal::new("image-tag", "ghcr.io/pleme-io/hanabi:latest");
        assert_eq!(env.current(&sig), Some("sha256:aaa".to_owned()));
        assert_eq!(env.latest(&sig), Ok("sha256:bbb".to_owned()));
    }

    #[test]
    fn an_untracked_image_has_no_current_pin() {
        let pins = pins();
        let env = ImageEnv::new(&pins, &MockResolver { digest: "sha256:x" });
        let sig = UpdateSignal::new("image-tag", "ghcr.io/other:latest");
        assert_eq!(env.current(&sig), None);
    }

    #[test]
    fn the_generic_diffkind_shadows_an_image_digest_bump() {
        let pins = pins();
        let env = ImageEnv::new(&pins, &MockResolver { digest: "sha256:bbb" });
        let kind = DiffKind::new("image-tag");
        let sig = UpdateSignal::new("image-tag", "ghcr.io/pleme-io/hanabi:latest");
        assert_eq!(
            kind.shadow(&sig, &env),
            ShadowOutcome::WouldApply {
                from: "sha256:aaa".into(),
                to: "sha256:bbb".into()
            }
        );
    }

    #[test]
    fn a_swarm_shadows_image_tags_end_to_end_same_core_different_domain() {
        let pins = pins();
        let env = ImageEnv::new(&pins, &MockResolver { digest: "sha256:bbb" });
        let images = vec!["ghcr.io/pleme-io/hanabi:latest".to_owned()];
        let source = ImageSignalSource::new(&images);
        assert_eq!(source.signals().len(), 1);
        assert_eq!(source.signals()[0].kind, "image-tag");

        let mut swarm = Swarm::new(
            Colony::new()
                .register(
                    Box::new(DiffKind::new("image-tag")),
                    PromotionPolicy::new(PromotionMode::Effect),
                )
                .frozen(true), // shadow-only
            MemPlanStore::new(),
        );
        let report = swarm.run_cycle_from(&source, &env, 1);
        assert_eq!(report.shadowed, 1, "the image bump is shadowed by the same core");
        assert_eq!(report.applied, 0);
        assert_eq!(swarm.pending_plan(1).pending[0].to, "sha256:bbb");
    }
}
