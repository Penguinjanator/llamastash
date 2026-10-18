---
title: "refactor: remove the managed-multiplexer plumbing"
type: refactor
status: in-progress
date: 2026-10-18
origin: "TODO.md Next Batch 1, the entry \"The managed-multiplexer plumbing has no producer left\"; decision in [`2026-10-17-002-refactor-drop-lemonade-backend-plan.md`](2026-10-17-002-refactor-drop-lemonade-backend-plan.md)"
---

# refactor: remove the managed-multiplexer plumbing

## Goal

The Lemonade backend was the only producer of the managed-multiplexer shape (one
supervised umbrella process, per-model load and unload delegated to its API). It
was removed in PR #115. Nothing can build a delegated launch, an umbrella launch
id, an infra launch, or the proxy's engine-unavailable 503 any more. Delete that
code, the wire fields that only it filled, and the docs that describe it.

Every backend left (llama.cpp, vLLM, SGLang, generic) runs one supervised process
per model. After this change that is the only launch shape, and the code says so
without an enum that has one variant.

## Decisions

- **D1. No replacement hooks.** Delete the shape outright. No backwards-compat
  path (AGENTS.md: none until the first release). If an engine that needs an
  umbrella shows up later, it gets designed then.
- **D2. `Lifecycle` goes entirely.** Remove the `Lifecycle` enum, its `label()`,
  and `Backend::lifecycle()` from the trait, the `for_each_backend!` delegation
  and every backend impl. Callers that compared against
  `Lifecycle::ProcessPerModel` (`daemon/launch_service.rs` near line 1248,
  `daemon/orphans.rs` near line 234) drop the comparison: read what the check
  guarded and keep the behaviour process backends already get today.
- **D3. `LaunchPlan` collapses into `ProcessLaunchSpec`.** With
  `DelegateToManager` gone the enum has one variant. `Backend::prepare_launch`
  returns `ProcessLaunchSpec` directly; delete `LaunchPlan`, `ManagerLaunchSpec`,
  `ManagerModelRef`, the `unreachable!`/`panic!` arms, and test helpers that only
  unwrap the enum (`spec_of` in `backend/llama_cpp/mod.rs`). Keep
  `Backend::start` / `Backend::stop` as trait hooks: the generic backend
  overrides `start`.
- **D4. `backends` JSON loses `lifecycle` and `umbrella`.** `status --json`
  rows become `{id, installed, accelerators, server_labels, enabled?, binary?}`.
  Remove `Backend::status_extra` (no backend overrides it; it existed to carry
  `umbrella`) and the `umbrella` rendering in `cli/output.rs` (human and TSV),
  plus the tests that fed synthetic `managed_multiplexer` / `umbrella` rows.
  `backend_row` in `ipc/status.rs` loses its lifecycle argument.
- **D5. Proxy keeps one forward path.** Remove `decide_umbrella_route`, the
  multiplexer branch in `decide_route`, `RouteDecision::BackendUnavailable` and
  its arms in `proxy/router.rs`, `upstream_path_prefix` on
  `RouteDecision::Forward` and `ForwardTarget`, `ProxyState::umbrella_client` and
  `build_umbrella_client` (always use `http_client`), and the
  `is_infra_launch` skips in `proxy/route.rs`, `proxy/router.rs`,
  `proxy/metrics.rs` and `proxy/ui.rs`. Keep `RouteDecision::Forward::model_path`
  as is; only fix its doc comment.
- **D6. Eviction loses its umbrella branches.** Remove the two
  `umbrella_owner(..)` branches in `proxy/eviction.rs`, `umbrella_idle_rows` and
  `delegated_rows`. Where a helper or gate is shared with process rows (for
  example the row-level exemptions read off the snapshot row), keep it and
  rewrite only the comment that justifies it by delegated rows. Delete only what
  becomes unused.
- **D7. Daemon and IPC.** Remove: `Backend::umbrella_launch_id`,
  `umbrella_openai_prefix`, `supervise_at_boot` (trait, macro delegation, and the
  boot call in `daemon/mod.rs` step 8a); `is_infra_launch`, `umbrella_owner`,
  `is_managed_multiplexer` in `backend/mod.rs`; `multiplexer_refuses_name`, its
  call and its test in `daemon/launch_service.rs`; the umbrella-owner branch of
  `backend_for_launch`; the `delegated` map and `set_delegated_state` /
  `delegated_state` / `remove_delegated` / `clear_delegated` in
  `daemon/registry.rs` (update the callers; `ManagedState` stays);
  `RunningModel::delegated_backend_id` in `daemon/state_store.rs`;
  `delegated_target` and the umbrella log lookup in `ipc/methods.rs` (`logs_tail`
  tails the launch's own log); the umbrella-row key lookup in `ipc/status.rs`;
  the idle infra skip in `daemon/idle.rs`; the supervisor's "cannot supervise a
  managed umbrella" port error in `daemon/supervisor.rs` if nothing else returns
  it. Keep `IdentityError::BackendUnavailable` in `backend/mod.rs`: it is a
  different error (an explicit backend that is not available) and still live.
- **D8. TUI and CLI.** Remove the infra-row drop and the delegated tab gating in
  `tui/app.rs`, the shared `*` marker in `tui/right_pane.rs` and its legend line
  in `tui/help_overlay.rs`, the multiplexer arm in `tui/launch_picker.rs`, the
  multiplexer poll case in `tui/events.rs` (plus its test helper near line 5292)
  and `tui/tabs/requests.rs`, and the multiplexer check in `cli/start.rs`. If a
  golden render under `tests/golden/` changes because the legend line is gone,
  update that golden file to the new output.
- **D9. Words that stay.** "managed" also means "a launch the daemon
  supervises" (`ManagedRow`, `ManagedState`, `ManagedModel`, "no managed
  launches"): keep all of those. "delegated" also means a knob handed to
  `--fit` (`is_fit_delegated`): keep. "umbrella" in README.md, FEATURES.md and
  the Lemonade recipe in `docs/usage.md` (near line 1261) describes `lemond`'s
  own process run as a generic entry: keep. Historical records are not edited:
  `docs/plans/*` other than this file, `docs/brainstorms/*`, `docs/spikes/*`,
  `docs/testing/*`, `docs/reviews/*`, `docs/benchmarks/*`, released
  `CHANGELOG.md` sections, and ticked `[x]` entries in `TODO.md`.

## Unit

One unit, one lane. The trait methods being removed are called across the whole
tree, so the change does not split into parallel units that each compile.

### U1. Remove the plumbing, update docs

Files (edit; delete code only, no new modules):

- `src/backend/mod.rs` (module docs lines 25-35, `Lifecycle`, `LaunchPlan`,
  `ManagerLaunchSpec`, `ManagerModelRef`, trait methods and their docs, macro
  delegations, `is_infra_launch`, `umbrella_owner`, `is_managed_multiplexer`,
  the tests `lifecycle_labels_are_stable`, `delegate_to_manager_carries_umbrella_and_model`
  and the lifecycle asserts in the other tests; reword comments that cite a
  managed multiplexer as an example, e.g. near lines 323, 639, 756, 890, 931,
  963, 1008)
- `src/backend/{llama_cpp,vllm,sglang,generic}/mod.rs`, `src/backend/identity.rs`
- `src/daemon/{mod,registry,launch_service,state_store,supervisor,idle,orphans,restart}.rs`
- `src/ipc/{methods,status}.rs`
- `src/proxy/{route,router,forward,state,eviction,metrics,ui}.rs`
- `src/cli/{output,start,stop,daemon}.rs`
- `src/tui/{app,events,right_pane,help_overlay,launch_picker,info_pane}.rs`,
  `src/tui/tabs/{requests,settings}.rs`
- `src/test_support.rs` (comment near line 146)
- `tests/vllm_backend_test.rs` (comment near line 303), any `tests/*.rs` or
  `tests/golden/*` that break
- Docs: `AGENTS.md` (the "Adding a backend" item 1: drop "for a managed
  multiplexer ... `supervise_at_boot`"), `docs/architecture.md` (lines near 161,
  167, 274, 311, 395, 520, 561, 568, and the Backend neutrality contract table),
  `docs/usage.md` (near line 495, and any `backends` JSON shape that lists
  `lifecycle` or `umbrella`), `docs/troubleshooting.md` (near line 117),
  `CHANGELOG.md` (one line under `[Unreleased]` `### Removed`: the
  `lifecycle` field of `status --json` backend rows is gone; every backend runs
  one process per model), `TODO.md` (strike the entry at line 503 with
  `- [x] ~~...~~` and a one-line "removed 2026-10-18" note; strike the two open
  entries that only exist for the multiplexer, near lines 659 and 670, the same
  way), this plan (tick the checklist below)

Find every site with:

```bash
rg -n 'DelegateToManager|ManagedMultiplexer|managed_multiplexer|managed-multiplexer|managed multiplexer|[Uu]mbrella|is_infra_launch|supervise_at_boot|ManagerLaunchSpec|ManagerModelRef|LaunchPlan|Lifecycle\b|\.lifecycle\(\)|multiplexer|delegated_|_delegated|status_extra|upstream_path_prefix|BackendUnavailable' src tests AGENTS.md docs/usage.md docs/architecture.md docs/troubleshooting.md
```

Checklist:

- [x] Code: D2-D8 applied, no dead code or unused imports left
- [x] Docs: AGENTS.md, architecture.md, usage.md, troubleshooting.md, CHANGELOG.md
- [x] TODO.md entries struck
- [x] Verification below passes

## Verification

Setup: the worktree starts with no `target/`; the first build is cold (several
minutes). Builds are capped at 8 jobs by `~/.cargo/config.toml`.

```bash
cargo check --all-targets --features test-fixtures,uat
cargo check --all-targets
cargo nextest run --features test-fixtures --lib
cargo nextest run --features test-fixtures      # full suite, once, at the end
cargo test --doc --features test-fixtures
```

The search command above returns only lines D9 says to keep, plus
`IdentityError::BackendUnavailable`.

E2E (AGENTS.md requires it for a wire change). Isolated daemon, never the user's
real one; export the env in the same shell line as every command:

```bash
E=$PWD/target/e2e-mux; mkdir -p $E/{state,config,cache,hf}
export LLAMASTASH_STATE_DIR=$E/state LLAMASTASH_CONFIG_DIR=$E/config LLAMASTASH_CACHE_DIR=$E/cache HF_HOME=$E/hf
cargo build --features test-fixtures --bins
target/debug/llamastash daemon start --proxy-port 18437
target/debug/llamastash status --json | jq '.backends'   # no lifecycle / umbrella keys
target/debug/llamastash status                          # backends block renders
target/debug/llamastash --render --render-size 160x45   # TUI renders, help legend has no umbrella line
target/debug/llamastash daemon stop
```

Write a minimal `$E/config/config.yaml` first with
`daemon: {port_range: {start: 47300, end: 47350}}`,
`disable_default_cache_paths: {huggingface: true, ollama: true, lm_studio: true}`
and `model_paths: []`, so the isolated daemon scans nothing and binds no default
port.

## Definition of done

U1 checklist ticked, verification green, one commit on
`refactor/drop-managed-multiplexer` with the docs in the same commit, no AI
attribution in the commit.
