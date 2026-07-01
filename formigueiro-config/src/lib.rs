//! # formigueiro-config — the shikumi-typed config surface for the update swarm
//!
//! Every operator-facing formigueiro knob lives here as a [`shikumi::TieredConfig`]
//! struct, so the swarm is configured the one fleet-standard way: `bare()` is the
//! zero-opinion floor (disabled, empty), `prescribed_default()` is the shipped,
//! **shadow-first** posture (enabled, but every kind starts under
//! [`outorga::PromotionMode::ShadowConfirmEffect`] — never a blind `Effect`).
//!
//! The promotion knobs are typed by [`outorga`] (the shared promotion FSM), so a
//! `PromotionConfig` maps directly to an [`outorga::PromotionPolicy`] — config and
//! runtime speak one vocabulary.
//!
//! Layering (per the ★★ CONFIGURATION MANAGEMENT rule): this crate is the typed
//! schema + tier logic; the HM / NixOS / Darwin module trio and the
//! `ConfigStore` discovery + hot-reload wrapper compose *around* it (M-phase — a
//! `pending-shikumi` follow-up, tracked in `theory/FORMIGUEIRO.md`).

use outorga::{PromotionMode, PromotionPolicy};
use serde::{Deserialize, Serialize};

/// The default swarm tick interval (seconds).
pub const DEFAULT_TICK_INTERVAL_SECS: u64 = 300;
/// The default samba admission quota, as a percent of the observed upstream limit.
pub const DEFAULT_QUOTA_PCT: f64 = 5.0;
/// The default JetStream subject the swarm ingests update events from.
pub const DEFAULT_JETSTREAM_SUBJECT: &str = "formigueiro.events";
/// The default NATS endpoint (the fleet's in-cluster NATS).
pub const DEFAULT_NATS_URL: &str = "nats://pleme-nats.nats.svc:4222";

/// Top-level formigueiro configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FormigueiroConfig {
    /// Master enable. `false` ⇒ the swarm does nothing (bare floor).
    pub enable: bool,
    /// The fleet master switch. `true` ⇒ **every** kind is re-shadowed instantly,
    /// overriding each kind's own promotion mode ([`outorga`] two-key rule).
    pub freeze: bool,
    /// How often the swarm ticks its convergence loop.
    pub tick_interval_secs: u64,
    /// Where update events come from.
    pub ingest: IngestConfig,
    /// How mutations are paced (samba).
    pub pacing: PacingConfig,
    /// The update-kind catalog. Empty ⇒ nothing to converge.
    pub kinds: Vec<UpdateKindConfig>,
}

impl FormigueiroConfig {
    /// Look up a kind's config by name.
    #[must_use]
    pub fn kind(&self, name: &str) -> Option<&UpdateKindConfig> {
        self.kinds.iter().find(|k| k.name == name)
    }

    /// The effective promotion policy for a kind — with the fleet `freeze`
    /// already folded in is *not* done here (freeze is a runtime input to
    /// [`outorga::PromotionPolicy::decide`]); this returns the kind's declared
    /// policy. Unknown kinds get the safe default (`ShadowConfirmEffect`).
    #[must_use]
    pub fn policy_for(&self, kind: &str) -> PromotionPolicy {
        self.kind(kind)
            .map_or_else(PromotionPolicy::default, |k| k.promotion.to_policy())
    }
}

/// Event-ingestion (NATS JetStream) configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IngestConfig {
    /// NATS endpoint.
    pub nats_url: String,
    /// JetStream subject carrying update events.
    pub jetstream_subject: String,
}

/// samba pacing configuration for swarm mutations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PacingConfig {
    /// Admission quota, percent of the observed upstream rate limit.
    pub quota_pct: f64,
    /// Token-bucket burst.
    pub burst: u32,
}

/// One update kind (e.g. `flake-input`, `image-tag`) and its promotion policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateKindConfig {
    /// The kind's stable name.
    pub name: String,
    /// Whether this kind is active.
    pub enable: bool,
    /// The kind's promotion policy.
    pub promotion: PromotionConfig,
}

/// The typed, serializable projection of an [`outorga::PromotionPolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromotionConfig {
    /// The promotion lifecycle mode.
    pub mode: PromotionMode,
    /// The clean-observation window before auto-promotion.
    pub confirm_after_secs: u64,
}

impl PromotionConfig {
    /// Map to the runtime [`outorga::PromotionPolicy`].
    #[must_use]
    pub fn to_policy(self) -> PromotionPolicy {
        PromotionPolicy {
            mode: self.mode,
            confirm_after_secs: self.confirm_after_secs,
        }
    }

    /// The shipped, safe default: shadow-first auto-promotion.
    #[must_use]
    pub fn prescribed() -> Self {
        Self {
            mode: PromotionMode::ShadowConfirmEffect,
            confirm_after_secs: outorga::DEFAULT_CONFIRM_AFTER_SECS,
        }
    }
}

// ── shikumi::TieredConfig — bare (zero-opinion) + prescribed (shipped) ─────────

impl shikumi::TieredConfig for FormigueiroConfig {
    fn bare() -> Self {
        Self {
            enable: false,
            freeze: false,
            tick_interval_secs: 0,
            ingest: IngestConfig::bare(),
            pacing: PacingConfig::bare(),
            kinds: Vec::new(),
        }
    }

    fn prescribed_default() -> Self {
        // Enabled fleet-wide by default — but SHADOW-FIRST: the one starter kind
        // (`flake-input`) auto-promotes only after a clean window, and `freeze`
        // is a one-flip fleet kill switch. This is the safe posture that retires
        // the 2026-06-02 blind auto-writer.
        Self {
            enable: true,
            freeze: false,
            tick_interval_secs: DEFAULT_TICK_INTERVAL_SECS,
            ingest: IngestConfig::prescribed_default(),
            pacing: PacingConfig::prescribed_default(),
            kinds: vec![UpdateKindConfig {
                name: "flake-input".to_owned(),
                enable: true,
                promotion: PromotionConfig::prescribed(),
            }],
        }
    }
}

impl shikumi::TieredConfig for IngestConfig {
    fn bare() -> Self {
        Self {
            nats_url: String::new(),
            jetstream_subject: String::new(),
        }
    }

    fn prescribed_default() -> Self {
        Self {
            nats_url: DEFAULT_NATS_URL.to_owned(),
            jetstream_subject: DEFAULT_JETSTREAM_SUBJECT.to_owned(),
        }
    }
}

impl shikumi::TieredConfig for PacingConfig {
    fn bare() -> Self {
        Self {
            quota_pct: 0.0,
            burst: 0,
        }
    }

    fn prescribed_default() -> Self {
        Self {
            quota_pct: DEFAULT_QUOTA_PCT,
            burst: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shikumi::TieredConfig;

    #[test]
    fn bare_is_a_zero_opinion_inert_floor() {
        let b = FormigueiroConfig::bare();
        assert!(!b.enable, "bare must be disabled");
        assert!(!b.freeze);
        assert_eq!(b.tick_interval_secs, 0);
        assert!(b.kinds.is_empty());
        assert!(b.ingest.nats_url.is_empty());
        assert_eq!(b.pacing.quota_pct, 0.0);
    }

    #[test]
    fn prescribed_is_enabled_but_shadow_first() {
        let p = FormigueiroConfig::prescribed_default();
        assert!(p.enable, "prescribed ships enabled");
        assert!(!p.freeze, "not frozen by default");
        assert_eq!(p.tick_interval_secs, DEFAULT_TICK_INTERVAL_SECS);
        // the load-bearing safety property: NO kind ships in blind Effect mode
        for k in &p.kinds {
            assert_ne!(
                k.promotion.mode,
                PromotionMode::Effect,
                "kind {} must not ship in blind Effect",
                k.name
            );
        }
        // the starter kind exists and auto-promotes safely
        let flake = p.kind("flake-input").expect("flake-input starter kind");
        assert!(flake.enable);
        assert_eq!(flake.promotion.mode, PromotionMode::ShadowConfirmEffect);
    }

    #[test]
    fn policy_for_maps_config_to_runtime_and_defaults_safely() {
        let p = FormigueiroConfig::prescribed_default();
        let flake = p.policy_for("flake-input");
        assert_eq!(flake.mode, PromotionMode::ShadowConfirmEffect);
        assert_eq!(flake.confirm_after_secs, outorga::DEFAULT_CONFIRM_AFTER_SECS);
        // unknown kind → the safe default, never Effect
        let unknown = p.policy_for("does-not-exist");
        assert_eq!(unknown.mode, PromotionMode::ShadowConfirmEffect);
    }

    #[test]
    fn round_trips_through_yaml() {
        let p = FormigueiroConfig::prescribed_default();
        let yaml = serde_yaml::to_string(&p).unwrap();
        let back: FormigueiroConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn rejects_unknown_fields() {
        // deny_unknown_fields guards typos in operator config
        let bad = r#"{"mode":"shadow","confirm_after_secs":10,"typo":true}"#;
        assert!(serde_json::from_str::<PromotionConfig>(bad).is_err());
    }

    #[test]
    fn promotion_mode_serializes_camel_case() {
        let c = PromotionConfig::prescribed();
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("shadowConfirmEffect"), "got {j}");
    }
}
