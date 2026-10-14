---
title: "feat: R15 batch 3, live launch state and running-row enumeration"
type: feat
status: active
date: 2026-10-14
---

# feat: R15 batch 3, live launch state and running-row enumeration

## Overview

Two TODO items from `TODO.md` § R15 → Batch 3:

1. `/ui`'s `collect_running` has its own copy of the supervisor + catalog walk that `metrics::live_launches` already does. Make it consume `live_launches`.
2. External rows (`ctx.external`) are filled once at daemon boot and never refreshed, and the TUI and `status` table drop the port and leave external rows out of the `running` counter.

Keep both changes small. No new config keys, no new backend hooks, no new dependencies.

## Requirements Trace

- R1. `collect_running` in `src/proxy/ui.rs` builds its rows from `metrics::live_launches` (filtered to `ready`), not from its own registry walk.
- R2. An external server started after the daemon shows up in `status` / TUI within one resweep interval.
- R3. An external row whose process has exited drops off within one resweep interval.
- R4. The resweep is cheap: fixed interval (30 s), reuses `orphans::sweep`, no work beyond one process-table scan per tick.
- R5. The daemon's own children are never listed as external.
- R6. A row re-adopted at boot (the `from_adopted` rows that carry a launch `name`) keeps its name and port while its process lives.
- R7. The TUI shows the external row's port from `status` (the daemon already sends `port`); unknown port renders no `:0`.
- R8. The `status` padded table shows an external row's port in the `PORT` column when known, `-` otherwise.
- R9. The TUI Daemon pane `running N (...)` counts and lists external rows next to managed ones.

## Scope Boundaries

- No new way to find a port when the process argv has no `--port` / `-p` / `--port=` flag. That row keeps `port: null` and renders `-` / no port. (Engine default ports and reading listening sockets were considered and left out to keep this small.)
- `N ready` in the models row stays managed-only: the proxy cannot route to an external row and the resweep does not probe readiness.
- No config key for the resweep interval.
- No change to `stop_external`, the proxy routing, or `/v1/models`.
- Do not touch the `/metrics` `serve` / `parse_request` / `resolve_launch` / `json_body` functions: the R15 batch 2 branch rewrites them and this branch must merge cleanly with it.

## Context & Research

### Relevant Code and Patterns

- `src/proxy/metrics.rs` `LiveLaunch` + `live_launches` (sorted by `launch_order`, skips `is_infra_launch`, carries `ready`).
- `src/proxy/ui.rs` `collect_running` / `RunningEntry`. Its extra needs: the full `ModelId` (passed to `forward::Target::served_model_key`, used by `acquire_inflight_guard`) and the per-entry `serves_ui` verdict from `route::running_model_backend`.
- `src/daemon/mod.rs` step 6 (~line 343): boot sweep, then the adopted-row merge into `external_combined`, then `.with_external(...)`.
- `src/daemon/orphans.rs` `sweep`, `SweepInputs::new`, `ExternalProcess`, `pid_alive`, `extract_port`.
- `src/daemon/host_metrics.rs` `spawn(token, ...)`: pattern for a token-cancelled periodic task.
- `ctx.external` is `Arc<RwLock<Vec<ExternalProcess>>>`; readers: `ipc/status.rs`, `ipc/methods.rs` (`stop_external`), `daemon/launch_service.rs` (in-use ports).
- `src/tui/app.rs` `parse_external_row` hardcodes `port: 0` and `name: None` although `status` sends both. This is why the TUI shows `:0`; the scan already parses `--port` from argv.
- `src/cli/output.rs` status table: external rows hardcode `-` in `PORT`.
- `src/tui/info_pane.rs` `running_row` iterates `app.managed` only.
- Supervised children run under `setsid` but their parent pid is still the daemon's pid.

## Key Technical Decisions

- **Add `model_id: ModelId` to `LiveLaunch`, keep `path: String`.** `/ui` needs the full id for the in-flight guard. Keeping `path` avoids rewriting every `l.path == ...` comparison in `metrics.rs`, which would collide with batch 2. Test literals of `LiveLaunch` get the new field (a small test helper is fine if it removes repetition).
- **`live_launches` stays in `metrics.rs`.** Moving it would churn imports in files batch 2 edits. Update its doc comment: `/ui` now consumes it.
- **`/ui` chooser order becomes launch order (`L2` before `L10`)** because it inherits `live_launches`' sort. Acceptable and deterministic.
- **Resweep = `orphans::sweep` with an empty `recorded_running`**, every 30 s, as a token-cancelled task spawned next to the other daemon tasks after the context is built.
- **Own-children filter: skip a process whose parent pid is the daemon's pid** (`std::process::id()`). Covers every supervised child, including one mid-spawn that no supervisor has registered yet. One check, no supervisor locking.
- **Merge rule, as a pure function:** new list = fresh scan rows, plus each previous row whose pid is still alive with the same start time and not already in the fresh scan. When the fresh scan has the same pid as a named previous row, keep the previous (named) row only if it is the same process (start times match, or one side is unknown); otherwise the fresh row wins. Everything else from the previous list is dropped.
- **The tick does its process I/O off the async runtime.** Scan and merge run in `spawn_blocking`; the write lock is taken only to assign the result, minus any row `stop_external` removed while the tick ran.
- **Keep `port` on `ManagedRow` as `u16`**, with 0 meaning unknown for external rows; renderers that print an external row's port skip it when 0. Smallest change.

## Implementation Units

Units 1 and 2 share no files and do not depend on each other. They run in parallel, each in its own worktree on its own branch, and are merged before review.

- [x] **Unit 1: `/ui` consumes `live_launches`** (lane 1)

**Goal:** One registry walk for live launches.

**Requirements:** R1

**Dependencies:** none

**Files:**
- Modify: `src/proxy/metrics.rs` (`LiveLaunch` gains `model_id`; `live_launches` fills it; doc comment; test literals)
- Modify: `src/proxy/metrics_backend.rs` (only if its tests build `LiveLaunch` literals)
- Modify: `src/proxy/ui.rs` (`collect_running` maps `live_launches` → `RunningEntry`, filtering `ready`)
- Test: `src/proxy/ui.rs` (inline), `src/proxy/metrics.rs` (inline)

**Approach:**
- `collect_running` calls `live_launches`, keeps `ready` entries, and computes `serves_ui` per entry as it does today. The `name` it shows is `LiveLaunch::model` (same `served_name_for_model` result).
- Delete the duplicated walk. Do not touch `serve`, `parse_request`, `resolve_launch`, or `json_body`.

**Patterns to follow:** existing `live_launches` callers in `metrics_backend.rs`.

**Test scenarios:**
- Happy path: two Ready launches → `collect_running` returns both, in launch order, with the right ports and names.
- Edge case: a launch that is not Ready (Starting) is excluded from `/ui` but still present in `live_launches` with `ready: false`.
- Edge case: an infrastructure (umbrella) launch is excluded from both.
- Existing `/ui` chooser / forward tests keep passing unchanged.

**Verification:** `src/proxy/ui.rs` no longer calls `supervisors.snapshot()` or `catalog.shared_view()`.

- [x] **Unit 2: periodic external resweep and external rows in TUI / status** (lane 2)

**Goal:** External rows stay current, show their port, and count as running.

**Requirements:** R2–R9

**Dependencies:** none

**Files:**
- Modify: `src/daemon/orphans.rs` (own-children filter; pure merge function; resweep loop + interval constant)
- Modify: `src/daemon/mod.rs` (spawn the resweep task with the daemon's cancel token)
- Modify: `src/ipc/methods.rs` (fix the `stop_external` doc comment that says the snapshot is rebuilt on `status`)
- Modify: `src/tui/app.rs` (`parse_external_row` reads `port`; `name` stays `None` because the TUI never addresses an external row by name)
- Modify: `src/tui/info_pane.rs` (`running_row` counts and lists external rows)
- Modify: wherever the TUI prints an external row's port (find it; skip the port when 0)
- Modify: `src/cli/output.rs` (status table `PORT` for external rows)
- Modify: `docs/architecture.md` (orphan sweep section: periodic resweep, 30 s, own-children filter), `docs/usage.md` (status external rows, if it describes them), `CHANGELOG.md` (one line under `[Unreleased]`), `TODO.md` (tick and strike **both** batch 3 items; Unit 1's lane does not edit `TODO.md`)
- Test: `src/daemon/orphans.rs` (inline), `src/tui/app.rs` (inline), `src/tui/info_pane.rs` (inline), `src/cli/output.rs` (inline)

**Approach:**
- The boot sweep stays as it is. After the context is built, spawn a loop: wait 30 s, run `sweep` with an empty recorded list, merge into `ctx.external` under one write lock, repeat until the token is cancelled.
- The own-children filter goes inside `sweep` (it applies at boot too, where it is a no-op).
- `running_row`: count = managed + external; external entries render as their label plus `:port` only when the port is known.

**Test scenarios:**
- Merge, happy path: previous `[A(scan)]`, fresh `[B]` → `[B]` (A exited, B is new).
- Merge: previous named row N (pid alive) not in fresh → N kept with name and port.
- Merge: previous named row N whose pid is dead → dropped.
- Merge: fresh has the same pid as named N → one row, N's version (name kept).
- Filter: a process whose parent pid is the daemon pid is not reported as external (test through the pure filter or a helper; do not spawn real engines).
- TUI parse: `status` external row with `port: 41102, name: "coder"` → `ManagedRow` with port 41102 and name `coder`; `port: null` → port 0.
- Info pane: one managed + one external → `running 2 (...)` listing both; external with unknown port lists no `:0`; only external → `running 1`, not `running 0`.
- Status table: external with port 8080 → `PORT` shows `8080`; without → `-`.

**Verification:** start a `llama-server` (or the test fixture server) by hand after the daemon is up under an isolated `LLAMASTASH_STATE_DIR`; within ~30 s `status` lists it with its port; kill it; within ~30 s it is gone. TUI `--render` shows it in `running N` with no `:0`.

## Verification commands

Each lane in its own worktree. A new worktree has no `target/`, so the first build is cold.

- `cargo check --features test-fixtures`
- Lane 1: `cargo nextest run --features test-fixtures --lib proxy::`
- Lane 2: `cargo nextest run --features test-fixtures --lib orphans` and `--lib tui::` and `--lib cli::output`
- The pre-commit hook runs `cargo fmt`, `make lint` and `make doc` on commit. Do not run them by hand first.
- Lane 2 E2E: as in Unit 2 Verification, with `LLAMASTASH_STATE_DIR`, `LLAMASTASH_CONFIG_DIR`, `LLAMASTASH_CACHE_DIR` set to a temp dir and a non-default `--proxy-port`. Never touch the user's running daemon.

## Risks

| Risk | Mitigation |
|---|---|
| A supervised child is listed as external during spawn | Parent-pid filter, not supervisor registry lookup |
| Resweep drops a re-adopted row and its in-use port | Merge keeps named rows while their pid is alive |
| Merge conflict with the R15 batch 2 branch in `metrics.rs` | Unit 1 stays out of the functions batch 2 rewrites |
