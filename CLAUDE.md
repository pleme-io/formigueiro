# formigueiro — the fleet update anthill (workspace)

The continuous, shadow-first, promotion-gated **update swarm**. Canonical
doctrine: [`theory/FORMIGUEIRO.md`](https://github.com/pleme-io/theory/blob/main/FORMIGUEIRO.md).
Operator face: `pleme-io/docs/formigueiro.md`. Skill: `formigueiro`.

## Crates

| Crate | Role | Deps |
|---|---|---|
| **`outorga`** | The **generic** progressive-authority promotion FSM (`Shadow → ShadowConfirmEffect → Effect`, two-key freeze). Pure, traited, exhaustively tested. A k8s-free lift of breathe's promotion lifecycle — **breathe is the intended 2nd consumer** (three-site rule); extract to its own repo when it adopts. | serde only |
| **`formigueiro-core`** | The pure algebra of the swarm: `UpdateSignal`, the `UpdateKind` trait + its `shadow` (compute-what-would-change, no write), the mockable `UpdateEnv` boundary (observation seam) + the `SignalSource` trait (ingestion seam — where signals come from; `Swarm::run_cycle_from` polls it), the `Formiga::tick` that composes a `ShadowOutcome` with an `outorga` promotion decision into one typed `TickOutcome`, and the `Colony` orchestrator (a CATALOG-REFLECTION registry of kinds + their policies + the fleet freeze; dispatch-only, `Send+Sync`), and the `PlanStore` (the temporal keystone — folds each shadow into per-target state so a `ShadowConfirmEffect` window accrues across ticks; `TargetState` IS the `outorga::Observation`; `MemPlanStore` for M0, CRD/Postgres the durable destination). `Colony::tick_with_store` is the full temporal loop (shadow → fold → decide). The `Swarm<S: PlanStore>` is the stateful daemon object (owns a Colony + a store; `run_cycle` ticks a batch → a typed `SwarmReport` rollup with `is_quiescent()` = the fleet-currency predicate a Viggy `(defpromessa)` proves; `pending_plan(now)` → a `SwarmPlan` derived from the store alone (`PlanStore::targets()`, no re-observe) = the operator's live "what would the swarm do right now" view, `ready_to_apply()` vs `held()`). Pure (no I/O/clock/task), traited, exhaustively tested. The complete M0 shadow-only algebra. | outorga only |
| **`formigueiro-flake`** | The **`flake-input` Environment** — the first *real* `UpdateEnv`. A typed parse of `flake.lock` (`FlakeLock` → current locked rev) + a mockable `RefResolver` (upstream head via `git ls-remote`, **typed argv, no shell**) composed into `FlakeEnv`. The `RefResolver` trait IS the TYPED-SPEC triplet's Environment seam — real network never touches a unit test. Typed errors (`FlakeError` + `Display`, **no `format!()`**), no panic/unwrap. `FlakeSignalSource` is the ingestion half (emits a `flake-input` signal per lock input; pairs with `FlakeEnv`). `NixFlakeExecutor` is the write half (the real `ApplyExecutor` — `nix flake update <input>`, typed argv, no shell; only reached for a promoted mutation). Proven: a swarm shadows a real `blackmatter` bump end-to-end (frozen → held, `pending_plan` surfaces the concrete `to` rev). | formigueiro-core, serde_json |
| **`formigueiro`** (bin) | The runnable **daemon** — assembles everything: `FormigueiroConfig` → `build_colony` (kinds + policies + freeze), `<flake>/flake.lock` → `FlakeSignalSource` + `FlakeEnv(GitLsRemoteResolver)`, driven by `SwarmDaemon(Swarm, SystemClock, StdoutSink)`. One-shot by default (a shadow cycle → the operator reads the plan); `--watch` paces a loop; `--freeze` forces shadow-only. clap args, typed JSON output (no `format!`), anyhow-typed errors. **Runs end-to-end** (real `git ls-remote`, shadow-only). `publish=false` (nix/image-deployed, not crates.io). | all four + clap, anyhow |
| **`formigueiro-config`** | The `shikumi::TieredConfig` surface: update kinds, per-kind promotion policy (→ `outorga`), the fleet `freeze` switch, NATS ingestion, samba pacing. `prescribed_default()` is **shadow-first** (no kind ships in blind `Effect`). | outorga, shikumi |

## Standing rules

- **Shadow-first is non-negotiable — and now enforced by construction.** The write
  seam (`ApplyExecutor`) takes an `AppliedMutation`, which is constructible *only*
  from a `TickOutcome::Applied` (`AppliedMutation::from_outcome`). A shadowed /
  up-to-date / blocked outcome yields `None`, so a write without a promotion
  decision is **unrepresentable**, not merely discouraged. The default executor is
  `NullExecutor` (refuses every write); the daemon writes only under an explicit
  `--apply` (and even then, freeze + `outorga` gate *which* mutations apply).
  `prescribed_default` never ships a kind in `Effect`.
- **outorga stays generic.** It has zero formigueiro dependency and zero k8s coupling,
  so breathe (and any future promotion consumer) stands on the same tested FSM. Do not
  leak update-swarm concepts into it.
- **Config is shikumi.** Every operator knob is a `TieredConfig` (`bare` floor +
  `prescribed_default` shipped). The HM/NixOS/Darwin module trio + `ConfigStore` +
  hot-reload compose around `formigueiro-config` (pending-shikumi follow-up).
- **No `format!()` of emitted syntax; `#[derive(TataraDomain)]` for authoring surfaces;
  every recurring impl → the macro farm.** (Fleet standing rules.)

## Roadmap (see theory/FORMIGUEIRO.md §VIII)

M0 flake-update-as-formiga **shadow-only** ✅ (the `formigueiro` daemon runs end-to-end:
real `flake.lock` → `git ls-remote` → shadow → typed report/plan, frozen) → M1 wire
`outorga` promotion + `freeze` ✅ (the `SwarmDaemon` + `outorga` two-key decision) →
M2 `PlanStore` (in-mem ✅; CRD/Postgres next — the `PlanStore` trait is the seam) →
M2b NATS ingestion (the `SignalSource` seam; `FlakeSignalSource` is the first source)
→ M3 widen the `UpdateKind` catalog (image-tag, chart-version, …) → M4 lisp runtime
morphology (`(defupdatekind …)`). The full pure algebra + the runnable shadow-first
daemon are green + tested; remaining work is durable persistence, event ingestion,
and the kind catalog — each a bolt-on to the tested core.

> Repo-ification: created locally; the GitHub repo lands via the `pleme-io-github-posture`
> IaC flow (not `gh repo create`), then AUTO-RELEASE publishes the crates.
