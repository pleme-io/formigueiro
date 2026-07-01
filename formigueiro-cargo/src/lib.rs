//! # formigueiro-cargo — the cargo-dep Environment
//!
//! The **third update domain** — the same [`formigueiro_core`] algebra with a third
//! environment (crate dependency versions), confirming the pattern is a template,
//! not a coincidence. It supplies only the two seams, exactly as the flake and image
//! crates do:
//!
//! - **current** — the pinned version the swarm tracks for a crate (the caller's
//!   pin set; pure).
//! - **latest** — the crate's latest published version, via a [`CrateResolver`].
//!
//! The kind is [`formigueiro_core::DiffKind::new`]`("cargo-dep")`. [`CrateResolver`]
//! is the Environment seam (mockable); the real [`CargoSearchResolver`] runs
//! `cargo search` with a **typed argv (no shell)**. Errors are typed; nothing panics
//! or `format!()`s a composed message.

use std::collections::BTreeMap;
use std::process::Command;

use formigueiro_core::{BlockReason, SignalSource, UpdateEnv, UpdateSignal};

/// Resolve the latest published version for a crate. The **Environment seam**: the
/// real impl runs `cargo search` (typed argv, no shell); tests mock it.
pub trait CrateResolver {
    /// The latest published version of `krate`.
    ///
    /// # Errors
    /// A [`BlockReason`] when the crate can't be resolved.
    fn resolve(&self, krate: &str) -> Result<String, BlockReason>;
}

/// The production [`CrateResolver`]: `cargo search <krate> --limit 1`, argv-typed,
/// no shell. Parses the version from the first line (`krate = "x.y.z"  # ...`).
#[derive(Clone, Copy, Debug, Default)]
pub struct CargoSearchResolver;

impl CrateResolver for CargoSearchResolver {
    fn resolve(&self, krate: &str) -> Result<String, BlockReason> {
        let output = Command::new("cargo")
            .args(["search", krate, "--limit", "1"])
            .output()
            .map_err(|_| BlockReason::Unreachable)?;
        if !output.status.success() {
            return Err(BlockReason::Unreachable);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_version(&stdout).ok_or(BlockReason::Unreachable)
    }
}

/// Extract the version from `cargo search` output: the text between the first pair
/// of quotes on the first line (`krate = "x.y.z"  # description`).
fn parse_version(search_output: &str) -> Option<String> {
    let line = search_output.lines().next()?;
    let mut quoted = line.split('"');
    quoted.next(); // before the opening quote
    quoted.next().map(str::to_owned) // between the quotes = the version
}

/// The `cargo-dep` [`UpdateEnv`]: current = the pinned version from `pins`, latest =
/// the [`CrateResolver`]'s published version. A signal's `subject` is the crate name.
pub struct CargoEnv<'a, R: CrateResolver> {
    pins: &'a BTreeMap<String, String>,
    resolver: &'a R,
}

impl<'a, R: CrateResolver> CargoEnv<'a, R> {
    /// Compose an env from a pin set + a resolver.
    #[must_use]
    pub fn new(pins: &'a BTreeMap<String, String>, resolver: &'a R) -> Self {
        Self { pins, resolver }
    }
}

impl<R: CrateResolver> UpdateEnv for CargoEnv<'_, R> {
    fn current(&self, sig: &UpdateSignal) -> Option<String> {
        self.pins.get(&sig.subject).cloned()
    }
    fn latest(&self, sig: &UpdateSignal) -> Result<String, BlockReason> {
        self.resolver.resolve(&sig.subject)
    }
}

/// A [`SignalSource`] over the tracked crates: emits a `cargo-dep` signal per crate.
pub struct CargoSignalSource<'a> {
    crates: &'a [String],
}

impl<'a> CargoSignalSource<'a> {
    /// A signal source over `crates`.
    #[must_use]
    pub fn new(crates: &'a [String]) -> Self {
        Self { crates }
    }
}

impl SignalSource for CargoSignalSource<'_> {
    fn signals(&self) -> Vec<UpdateSignal> {
        self.crates
            .iter()
            .map(|krate| UpdateSignal::new("cargo-dep", krate))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use formigueiro_core::{DiffKind, ShadowOutcome, UpdateKind};

    struct MockResolver {
        version: &'static str,
    }
    impl CrateResolver for MockResolver {
        fn resolve(&self, _krate: &str) -> Result<String, BlockReason> {
            Ok(self.version.to_owned())
        }
    }

    fn pins() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("serde".to_owned(), "1.0.210".to_owned());
        m
    }

    #[test]
    fn parse_version_reads_the_cargo_search_line() {
        let out = "serde = \"1.0.219\"    # A generic serialization framework\nserde_json = \"1\"";
        assert_eq!(parse_version(out).as_deref(), Some("1.0.219"));
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn cargo_env_reports_pinned_current_and_resolved_latest() {
        let pins = pins();
        let env = CargoEnv::new(&pins, &MockResolver { version: "1.0.219" });
        let sig = UpdateSignal::new("cargo-dep", "serde");
        assert_eq!(env.current(&sig), Some("1.0.210".to_owned()));
        assert_eq!(env.latest(&sig), Ok("1.0.219".to_owned()));
    }

    #[test]
    fn the_generic_diffkind_shadows_a_crate_version_bump() {
        let pins = pins();
        let env = CargoEnv::new(&pins, &MockResolver { version: "1.0.219" });
        let sig = UpdateSignal::new("cargo-dep", "serde");
        assert_eq!(
            DiffKind::new("cargo-dep").shadow(&sig, &env),
            ShadowOutcome::WouldApply {
                from: "1.0.210".into(),
                to: "1.0.219".into()
            }
        );
    }

    #[test]
    fn signal_source_emits_a_cargo_dep_signal_per_crate() {
        let crates = vec!["serde".to_owned(), "tokio".to_owned()];
        let sigs = CargoSignalSource::new(&crates).signals();
        assert_eq!(sigs.len(), 2);
        assert!(sigs.iter().all(|s| s.kind == "cargo-dep"));
    }
}
