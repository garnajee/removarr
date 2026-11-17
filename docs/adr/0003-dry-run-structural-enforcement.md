# ADR-0003: Dry-run enforced structurally

Status: accepted (wave 1). Details: `docs/architecture.md` section 6.

## Context
MASTER §6 and CONTEXTE §3.11 and §9.5 require that dry-run runs the real workflow (scan, plan,
validations) but can never reach a mutation adapter. Dry-run is on by default.

## Decision
- `dry_run` and `auto_mode` are read once at startup. There is no runtime toggle.
- `removarr_config::LiveMode` is a token with a private constructor. It is obtained only through
  `Config::live_mode()`, which returns `None` in dry-run. A test-only constructor exists behind the
  `test-support` feature, which is enabled only through dev-dependencies.
- Every live mutating adapter constructor (`LiveFsMutator`, `QbitController`, `ArrCommander`,
  `JellyfinCommander`) takes `&LiveMode`. Read adapters do not.
- `app::wiring` builds `workflows::actions::Mutators`. The recording set can always be built.
  The live set needs `LiveMode`, so in a dry-run process real mutators are never constructed.
- Plans carry `PlanMode`. A dry-run plan can never be executed live.
- Amended at CP1: any plan can be **simulated** with the recording set in any process
  (`ExecutePlan { simulate: true }`). This gives CONTEXTE §6.2 "simulation complète" in live
  processes too. Simulation never changes the plan status, and its intents are ignored by
  recovery. Live execution requires a live plan, approval, a live process, and the runner's
  assertion `mutators.mode == plan.mode == Live`.

## Consequences
- Turning dry-run off requires a restart and a new plan. Simulating does not.
- Dry-run journals and plans are persisted in the DB (app data), never in roots.
- Guard tests and gate greps verify the property (architecture section 6.8).
