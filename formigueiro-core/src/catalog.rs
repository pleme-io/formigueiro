//! # catalog — the swarm's self-describing kind registry (CATALOG REFLECTION)
//!
//! The set of update kinds the swarm supports is declared here as typed data, so
//! the substrate describes itself: tooling (a `formigueiro kinds` listing, generated
//! docs, an M-phase planner) iterates [`KIND_CATALOG`] mechanically rather than
//! grepping for `UpdateKind` impls. Adding a kind *requires* a catalog entry — the
//! reflection tests fail otherwise — so code and its self-description cannot drift.
//!
//! Each domain crate (`formigueiro-flake`, `-image`, `-cargo`) supplies the kind's
//! [`crate::UpdateEnv`]; this catalog is the metadata + the mechanical promise (via
//! the shadow matrix) that every listed kind is a working [`crate::DiffKind`].

use serde::Serialize;

/// The maturity of an update kind — a mechanical readiness signal for tooling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Maturity {
    /// Shipped with a tested environment.
    Working,
    /// Declared, environment not yet shipped.
    Draft,
}

/// A catalog entry describing one update kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KindEntry {
    /// The kind's stable name (the dispatch key + [`crate::DiffKind`] name).
    pub name: &'static str,
    /// The crate that supplies this kind's environment.
    pub domain: &'static str,
    /// Mechanical readiness.
    pub maturity: Maturity,
    /// One-line purpose.
    pub purpose: &'static str,
}

/// The typed catalog of every update kind the swarm supports — its self-description.
/// Iterated by tooling; guarded by the reflection tests (unique names, shadow
/// matrix, maturity partition).
pub const KIND_CATALOG: &[KindEntry] = &[
    KindEntry {
        name: "flake-input",
        domain: "formigueiro-flake",
        maturity: Maturity::Working,
        purpose: "bump a nix flake input to its upstream head",
    },
    KindEntry {
        name: "image-tag",
        domain: "formigueiro-image",
        maturity: Maturity::Working,
        purpose: "bump a container image tag to its upstream digest",
    },
    KindEntry {
        name: "cargo-dep",
        domain: "formigueiro-cargo",
        maturity: Maturity::Working,
        purpose: "bump a crate dependency to its latest published version",
    },
];

/// Look up a catalog entry by kind name.
#[must_use]
pub fn catalog_entry(name: &str) -> Option<&'static KindEntry> {
    KIND_CATALOG.iter().find(|e| e.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockReason, DiffKind, ShadowOutcome, UpdateEnv, UpdateKind, UpdateSignal};
    use std::collections::BTreeSet;

    /// An env where every subject has a pending bump.
    struct BumpEnv;
    impl UpdateEnv for BumpEnv {
        fn current(&self, _s: &UpdateSignal) -> Option<String> {
            Some("old".into())
        }
        fn latest(&self, _s: &UpdateSignal) -> Result<String, BlockReason> {
            Ok("new".into())
        }
    }

    #[test]
    fn every_kind_name_is_unique() {
        let names: BTreeSet<_> = KIND_CATALOG.iter().map(|e| e.name).collect();
        assert_eq!(
            names.len(),
            KIND_CATALOG.len(),
            "duplicate kind name in the catalog"
        );
    }

    #[test]
    fn the_catalog_is_non_degenerate_and_covers_the_shipped_domains() {
        assert!(KIND_CATALOG.len() >= 3, "catalog earns reflection at >=3 kinds");
        for name in ["flake-input", "image-tag", "cargo-dep"] {
            let entry = catalog_entry(name).expect("shipped kind must be catalogued");
            assert_eq!(entry.maturity, Maturity::Working);
        }
    }

    #[test]
    fn every_catalogued_kind_shadows_a_bump() {
        // The MASS-SYNTHESIS matrix: exercise every listed variant. A new catalog
        // entry whose DiffKind doesn't shadow fails here — one run reports all.
        let mut failures = Vec::new();
        for entry in KIND_CATALOG {
            let kind = DiffKind::new(entry.name);
            let sig = UpdateSignal::new(entry.name, "x");
            if !matches!(kind.shadow(&sig, &BumpEnv), ShadowOutcome::WouldApply { .. }) {
                failures.push(entry.name);
            }
        }
        assert!(failures.is_empty(), "kinds failed the shadow matrix: {failures:?}");
    }

    #[test]
    fn maturity_histogram_partitions_the_catalog() {
        let working = KIND_CATALOG
            .iter()
            .filter(|e| e.maturity == Maturity::Working)
            .count();
        let draft = KIND_CATALOG
            .iter()
            .filter(|e| e.maturity == Maturity::Draft)
            .count();
        assert_eq!(working + draft, KIND_CATALOG.len(), "maturity must partition");
    }

    #[test]
    fn catalog_serializes_to_typed_json() {
        let json = serde_json::to_string(&KIND_CATALOG[0]).unwrap();
        assert!(json.contains("\"name\":\"flake-input\"") && json.contains("\"maturity\":\"working\""));
    }
}
