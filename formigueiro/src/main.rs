//! # formigueiro — the update-swarm daemon
//!
//! Assembles the substrate into a runnable, **shadow-first** daemon:
//!
//! ```text
//!   FormigueiroConfig (shikumi)  ──build_colony──▶  Colony (kinds + policies + freeze)
//!   <flake>/flake.lock  ──FlakeLock::parse──▶  FlakeSignalSource + FlakeEnv(GitLsRemoteResolver)
//!            │
//!            ▼
//!   SwarmDaemon(Swarm, SystemClock, StdoutSink).tick(source, env)
//!            │  per cycle: shadow → temporal fold → outorga promotion
//!            ▼
//!   SwarmReport (emitted as typed JSON)  +  SwarmPlan (what would happen next)
//! ```
//!
//! One-shot by default (a single shadow cycle → the operator reads the plan);
//! `--watch` paces a convergence loop. Every mutation flows through `outorga`, and
//! the shipped config is shadow-first — nothing writes blind. Output is typed JSON
//! (serde), errors are typed (anyhow); no `format!()` of emitted text.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use formigueiro_config::FormigueiroConfig;
use formigueiro_core::{
    execute_applies, Colony, FlakeInputKind, MemPlanStore, NullExecutor, ReportSink, Swarm,
    SwarmDaemon, SwarmReport, SystemClock,
};
use formigueiro_flake::{
    FlakeEnv, FlakeLock, FlakeSignalSource, GitLsRemoteResolver, NixFlakeExecutor,
};
use shikumi::TieredConfig;

/// The fleet update-swarm daemon (shadow-first).
#[derive(Parser, Debug)]
#[command(name = "formigueiro", version, about)]
struct Args {
    /// The flake directory to converge (reads `<dir>/flake.lock`).
    #[arg(long, default_value = ".")]
    flake: PathBuf,
    /// Optional YAML config; defaults to the shipped shadow-first posture.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Keep running, ticking every interval; default is a single shadow cycle.
    #[arg(long)]
    watch: bool,
    /// Override the tick interval in seconds; defaults to the config's.
    #[arg(long)]
    interval: Option<u64>,
    /// Force the fleet freeze on (shadow-only regardless of config).
    #[arg(long)]
    freeze: bool,
    /// Opt in to the write path: execute promoted mutations (`nix flake update`).
    /// Default is the NullExecutor — even a promoted mutation does not write.
    #[arg(long)]
    apply: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut config = load_config(args.config.as_deref())?;
    if args.freeze {
        config.freeze = true;
    }
    let interval = args.interval.unwrap_or(config.tick_interval_secs).max(1);

    let mut daemon = SwarmDaemon::new(
        Swarm::new(build_colony(&config), MemPlanStore::new()),
        SystemClock,
        StdoutSink,
    );
    let resolver = GitLsRemoteResolver;

    loop {
        // Re-read the lock each cycle — it changes after an apply, so a fresh
        // observation is what converges the target.
        let lock = read_lock(&args.flake)?;
        let source = FlakeSignalSource::new(&lock);
        let env = FlakeEnv::new(&lock, &resolver);

        let report = daemon.tick(&source, &env); // StdoutSink emits the report
        let plan = daemon.swarm().pending_plan(report.at_epoch);
        emit_json(&plan)?; // the operator's "what would happen next"

        // Execute PROMOTED mutations only — structurally gated (an AppliedMutation
        // exists only for a TickOutcome::Applied). Default NullExecutor = shadow-
        // only, so promotion still does not write unless `--apply` is set.
        let applied = if args.apply {
            execute_applies(&report, &NixFlakeExecutor::new(&args.flake))
        } else {
            execute_applies(&report, &NullExecutor)
        };
        for result in &applied {
            emit_json(result)?;
        }

        if !args.watch {
            break;
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
    Ok(())
}

/// Load the config from `path`, or the shipped shadow-first posture if `None`.
fn load_config(path: Option<&Path>) -> Result<FormigueiroConfig> {
    let Some(path) = path else {
        return Ok(FormigueiroConfig::prescribed_default());
    };
    let yaml = std::fs::read_to_string(path).context("read config file")?;
    serde_yaml::from_str(&yaml).context("parse config YAML")
}

/// Build the swarm's [`Colony`] from config: register every enabled kind with its
/// promotion policy, and fold in the fleet freeze. Unknown kind names are skipped
/// (no `UpdateKind` impl yet) — their signals would surface as `UnknownKind`.
fn build_colony(config: &FormigueiroConfig) -> Colony {
    let mut colony = Colony::new();
    for kind in &config.kinds {
        if !kind.enable {
            continue;
        }
        match kind.name.as_str() {
            "flake-input" => {
                colony = colony.register(Box::new(FlakeInputKind), kind.promotion.to_policy());
            }
            _ => {} // no UpdateKind for this name yet — see the kind catalog roadmap
        }
    }
    colony.frozen(config.freeze)
}

/// Read + parse `<flake_dir>/flake.lock`.
fn read_lock(flake_dir: &Path) -> Result<FlakeLock> {
    let path = flake_dir.join("flake.lock");
    let json = std::fs::read_to_string(&path).context("read flake.lock")?;
    FlakeLock::parse(&json).context("parse flake.lock")
}

/// Emit a value as one line of typed JSON (serde is the renderer; no `format!()`).
fn emit_json<T: serde::Serialize>(value: &T) -> Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, value).context("serialize output")?;
    handle.write_all(b"\n").context("write output")?;
    Ok(())
}

/// The report sink: one typed-JSON line per cycle to stdout. Serialization of a
/// report never fails, and a broken stdout is not worth aborting a daemon over, so
/// the emission is best-effort.
struct StdoutSink;

impl ReportSink for StdoutSink {
    fn emit(&self, report: &SwarmReport) {
        let _ = emit_json(report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use formigueiro_config::UpdateKindConfig;
    use outorga::PromotionMode;

    #[test]
    fn default_config_is_the_shipped_shadow_first_posture() {
        let config = load_config(None).unwrap();
        assert!(config.enable);
        assert!(!config.freeze);
        let flake = config.kind("flake-input").expect("starter kind");
        assert_eq!(flake.promotion.mode, PromotionMode::ShadowConfirmEffect);
    }

    #[test]
    fn build_colony_registers_enabled_kinds_and_folds_freeze() {
        let config = FormigueiroConfig::prescribed_default();
        let colony = build_colony(&config);
        assert_eq!(colony.kind_names(), vec!["flake-input"]);
        assert!(!colony.is_frozen());

        // freeze folds in
        let mut frozen = config.clone();
        frozen.freeze = true;
        assert!(build_colony(&frozen).is_frozen());
    }

    #[test]
    fn a_disabled_kind_is_not_registered() {
        let mut config = FormigueiroConfig::bare(); // no kinds, disabled
        config.kinds.push(UpdateKindConfig {
            name: "flake-input".to_owned(),
            enable: false,
            promotion: formigueiro_config::PromotionConfig::prescribed(),
        });
        assert!(build_colony(&config).kind_names().is_empty());
    }

    #[test]
    fn an_unknown_kind_name_is_skipped_not_panicked() {
        let mut config = FormigueiroConfig::bare();
        config.kinds.push(UpdateKindConfig {
            name: "image-tag".to_owned(), // no UpdateKind impl yet
            enable: true,
            promotion: formigueiro_config::PromotionConfig::prescribed(),
        });
        assert!(build_colony(&config).kind_names().is_empty());
    }
}
