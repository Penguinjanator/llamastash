---
title: "feat: model residency (per-preset idle TTL, unload to make room, preload at boot)"
type: feat
status: implemented
date: 2026-09-30
origin: R12 batch 1 in TODO.md
---

# feat: model residency

**Status:** implemented (U1–U4). Plan kept as the design record.

## Overview

Three changes to which models stay loaded, all in the same code: the idle
sweep ([`src/proxy/eviction.rs`](../../src/proxy/eviction.rs)), admission
([`src/launch/admission.rs`](../../src/launch/admission.rs)) and the launch
service.

1. **Per-preset idle TTL.** A preset overrides `proxy.idle_ttl_secs`; `0` means
   never unload.
2. **Unload idle models to make room.** When admission refuses a proxy
   auto-start, stop idle auto-started launches until the new one fits, instead
   of refusing.
3. **Preload at daemon boot.** Start listed models when the daemon starts.

## Problem

- One global TTL (`proxy.idle_ttl_secs`, default 1800) applies to every
  auto-started launch. A slow-loading model unloads as fast as a small one.
  Measured load times on 2026-09-30: llama-server Qwen3.8-27B 4.3 s (warm page
  cache), gufo Flash-Next 21.5 s, halogen Flash-Next 84 s (62 GiB read at
  0.8 GB/s).
- A request for a model that does not fit is refused by admission. The client
  then gets a family-MRU fallback or a `503`. Only the TTL sweep ever unloads,
  so on a 124 GiB host where Flash-Next alone takes about 86 GiB, the second
  model is refused even when the first has been idle for minutes.
- Nothing starts models on boot. The daemon clears `state.running` on every
  boot and does not restore launches ("A confirmed entry is demoted, not
  restored" in [`docs/architecture.md`](../architecture.md)), so an always-warm model (an embedder) waits for its
  first request.

## Scope

In: the three items above, config, CLI/`--json`, TUI where a preset is edited.

Out: evicting manual launches (they stay exempt everywhere), evicting a launch
with in-flight requests, cross-host scheduling.

## Design

**Per-preset TTL.** Add `idle_ttl_secs` to the preset shape, written through
`config::yaml_edit` like the other preset fields. `eviction::decide` already
takes `ttl`; resolve it per launch from the preset that launched it, falling
back to `proxy.idle_ttl_secs`. `0` returns `Skip`.

**Unload to make room.** On an admission refusal for a proxy auto-start:

- Candidates: `Ready`, `LaunchOrigin::AutoStart`, zero in-flight, TTL not `0`,
  not preloaded.
- Stop them least recently used first (MRU tracker already has the timestamps)
  until the projected demand fits, then admit and launch.
- If all candidates together would not free enough, stop none and refuse as
  today.
- Stop with the existing grace path and wait for the memory to show as free
  before re-running admission, so the new launch is not priced against memory
  that is still held.
- Umbrella backends free a model through their unload API, the same branch the
  sweep uses.

**Preload.** `daemon.preload: [<ref> | <launch file>]`, plus a preset-level
`preload: true`. After the daemon is up, launch each entry through admission
like any other launch, in list order, with origin `Manual` so the sweep never
unloads it. A refused preload logs and continues; it never blocks boot.

## Units

- [x] U1. Preset `idle_ttl_secs`: schema, `yaml_edit`, resolution in the sweep,
  `0` = never. Tests in `eviction.rs`.
- [x] U2. Make-room path on admission refusal: candidate filter, LRU order,
  all-or-nothing check, wait-for-free before re-admit. Integration test with
  the fake server.
- [x] U3. `daemon.preload` and preset `preload: true`: boot launch, refusal
  logged, excluded from eviction.
- [x] U4. Docs: `docs/usage.md` (config keys), `docs/architecture.md`
  (eviction, admission), `config.example.yaml`, `CHANGELOG.md`.

## Verification

- Unit tests per unit, `make test`, `make lint`.
- E2E on an isolated daemon (`LLAMASTASH_STATE_DIR`, non-default
  `--proxy-port`): load a large model by request, request a second one that
  does not fit, confirm the first unloads and the second serves; confirm a
  manual launch and a `ttl: 0` launch are never picked; restart the daemon
  with `daemon.preload` set and confirm the model comes up.

## Open questions

Both settled during implementation.

- **Does make-room also apply to `start` from the CLI/TUI?** No — proxy
  auto-start only, as proposed. The CLI already has `--force`, and a launch the
  operator typed is a decision they can see the refusal for and act on; silently
  stopping another model to satisfy a command line is a worse surprise than a
  refusal that names the numbers.
- **Does a preloaded model count as `Manual` in `status`?** Yes, and it needs no
  new origin label. Preload is durable user intent by definition, which is
  exactly what `LaunchOrigin::Manual` already means to the sweep, to the
  `<model>@<name>` naming rule, and to `status`. What distinguishes a preload is
  that it came from config, which the running row does not need to say.

## What shipped differently from the sketch

- **Candidate credit is the launch's own admission projection,** not a fresh
  estimate. The demand the gate priced each admitted launch at is stamped on its
  `state.running` row (`projected_demand_bytes`), so credit and refusal are the
  same figure in the same unit. Rows with no stamp (adopted, delegated) fall back
  to their catalog weight size, then the file size; a row nothing can sizes
  stays out of the candidate set rather than being credited for nothing.
- **The refusal's numbers travel in the IPC error `data`**
  (`demand_bytes` / `effective_free_bytes` / `reserved_bytes` beside the existing
  `cause: launch_refused`), so the proxy retries with the gate's own arithmetic
  instead of re-deriving it in a second place.
- **Make-room lives in `proxy::eviction`, not the launch service.** The MRU
  timestamps it orders by live behind `ProxyState`, and `daemon` must not depend
  on `proxy`; the auto-start path in `proxy::launch` owns the refusal and the one
  retry.
- **A preset TTL keeps the sweeper alive on its own.** With
  `proxy.idle_ttl_secs: 0` the sweep would otherwise never run and a preset's
  `idle_ttl_secs` would do nothing, so the sweeper is armed when either the
  global TTL or any preset pins one. A launch whose own override is `0` is
  skipped per row.
- **The TUI got no residency editor.** `Ctrl+P` captures a running launch's
  params, and residency is not part of a launch's params, so `idle_ttl_secs` /
  `preload` stay CLI + config surface (`presets save --idle-ttl` / `--preload`),
  like `default:`. Follow-up filed in `TODO.md`.
