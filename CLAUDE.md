# formigueiro — the fleet update anthill (workspace)

The continuous, shadow-first, promotion-gated **update swarm**. Canonical
doctrine: [`theory/FORMIGUEIRO.md`](https://github.com/pleme-io/theory/blob/main/FORMIGUEIRO.md).
Operator face: `pleme-io/docs/formigueiro.md`. Skill: `formigueiro`.

## Crates

| Crate | Role | Deps |
|---|---|---|
| **`outorga`** | The **generic** progressive-authority promotion FSM (`Shadow → ShadowConfirmEffect → Effect`, two-key freeze). Pure, traited, exhaustively tested. A k8s-free lift of breathe's promotion lifecycle — **breathe is the intended 2nd consumer** (three-site rule); extract to its own repo when it adopts. | serde only |
| **`formigueiro-config`** | The `shikumi::TieredConfig` surface: update kinds, per-kind promotion policy (→ `outorga`), the fleet `freeze` switch, NATS ingestion, samba pacing. `prescribed_default()` is **shadow-first** (no kind ships in blind `Effect`). | outorga, shikumi |

## Standing rules

- **Shadow-first is non-negotiable.** Every apply flows through `outorga` — a blind
  write (mutation without a `PromotionDecision`) is not expressible. `prescribed_default`
  never ships a kind in `Effect`.
- **outorga stays generic.** It has zero formigueiro dependency and zero k8s coupling,
  so breathe (and any future promotion consumer) stands on the same tested FSM. Do not
  leak update-swarm concepts into it.
- **Config is shikumi.** Every operator knob is a `TieredConfig` (`bare` floor +
  `prescribed_default` shipped). The HM/NixOS/Darwin module trio + `ConfigStore` +
  hot-reload compose around `formigueiro-config` (pending-shikumi follow-up).
- **No `format!()` of emitted syntax; `#[derive(TataraDomain)]` for authoring surfaces;
  every recurring impl → the macro farm.** (Fleet standing rules.)

## Roadmap (see theory/FORMIGUEIRO.md §VIII)

M0 flake-update-as-formiga **shadow-only** → M1 wire `outorga` promotion + `freeze` →
M2 `PlanStore` (CRD/Postgres) → M3 widen the `UpdateKind` catalog → M4 lisp runtime
morphology. This repo currently ships the **M1 promotion algebra** (`outorga`) and its
**config surface** (`formigueiro-config`), both green + tested, ahead of wiring.

> Repo-ification: created locally; the GitHub repo lands via the `pleme-io-github-posture`
> IaC flow (not `gh repo create`), then AUTO-RELEASE publishes the crates.
