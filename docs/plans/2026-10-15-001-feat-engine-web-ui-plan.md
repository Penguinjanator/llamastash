---
title: "feat(proxy): bundled llama.cpp web UI for engines without one"
type: feat
status: active
date: 2026-10-15
origin: docs/brainstorms/2026-10-15-engine-web-ui-requirements.md
---

# Bundled llama.cpp web UI for engines without one

## Overview

`/ui` only works for llama.cpp launches, because only llama.cpp serves a web UI. vLLM v0.31.0 and SGLang v0.5.21 serve none (checked 2026-10-15), and neither do generic entries like gufo and Halogen. This plan embeds llama.cpp's prebuilt UI in the binary and serves it from the proxy for UI-less chat launches. The proxy answers the one llama.cpp-only route the UI needs (`/props`) and forwards `/v1/*` to the engine. A maintainer script refreshes the embedded copy, and a weekly workflow opens a PR when upstream moves.

Keep it simple. No new process, no runtime download, no response rewriting.

## Requirements Trace

From the origin doc (see origin: `docs/brainstorms/2026-10-15-engine-web-ui-requirements.md`):

- R1. A Ready, UI-less, chat-mode launch gets the bundled UI. Its chooser row is green, and a lone one opens directly.
- R2. llama.cpp launches keep their engine's own UI.
- R3. Embedding or rerank launches stay red and non-clickable.
- R4. No backend named in generic code. The choice keys on `Backend::serves_web_ui` plus the launch mode.
- R5. The UI loads without a server error and streams answers.
- R6. The context gauge shows the launch's resolved ctx.
- R7. Image attach only when the launch takes images.
- R8. Requests carry a model name the engine accepts.
- R9. UI files embedded in the binary; no network needed.
- R10. Nothing unpacked until the first bundled-UI request.
- R11. Pinned version recorded in the repo, plus a refresh script that checks sha256.
- R12. llama.cpp MIT notice ships with the binary.
- R13. Bundled-UI traffic stays out of the request log.
- R14. Cookie pin, chooser, `/ui/switch`, `?target=`, LAN Basic auth unchanged.
- R15. `docs/usage.md` § Web UI covers engines and limits.
- R16. `TODO.md`: router-mode follow-up added, batch 5 item struck.
- R17 (added in planning). A weekly workflow opens a refresh PR when upstream `latest` moves, using a PAT secret `LLAMA_UI_BUMP_TOKEN`, and does nothing when the secret is missing.

## Scope Boundaries

- No router mode (approach B). Follow-up in `TODO.md`.
- No rewriting of engine responses. vLLM thinking text stays hidden in `/ui`.
- No service worker changes.
- No sidecar process, no runtime download.
- No Lemonade-specific handling; its chat rows get the bundled UI through R4, untested.
- The weekly workflow opens a PR only. It never merges.

## Context & Research

### Relevant Code and Patterns

- `src/proxy/ui.rs`: `serve`, `resolve_target`, `collect_running`, `forward_ui`, `chooser_html`, `RunningEntry.serves_ui`. Unit tests at the bottom use `crate::proxy::test_support::{state_with, umbrella_id}`.
- `src/proxy/metrics.rs` `live_launches`: the one launch enumeration `collect_running` consumes.
- `src/proxy/route.rs` `running_model_backend`, `index_catalog_by_path`, `served_name_for_model`.
- `src/daemon/supervisor.rs`: `mode()` on the managed model; `actuals()` holds `resolved_ctx`.
- `src/ipc/status.rs` ~40-125: how `status` finds a launch's actuals (`running` snapshot keyed on launch id) to report `resolved_ctx`. Copy that lookup; don't invent a second one.
- `src/init/external/mod.rs:167`: vision comes from the catalog row's `multimodal.vision`. Generic entries set it from `vision:` (`src/backend/generic/mod.rs:144`).
- `src/init/benchmark.rs:39`: `include_str!("../../data/benchmark-snapshot.json")`. Same pattern for `include_bytes!` of the UI archive. `data/` ships in the crate; `assets/` and `scripts/` do not (`Cargo.toml` `exclude`).
- `tar = "0.4"` and `flate2` are already dependencies.
- `tests/proxy_ui.rs`: integration tests for the llama.cpp path (fake server). Must stay green.
- `scripts/vllm/README.md`, `scripts/sglang/README.md`: per-subdir script README convention.
- `.github/workflows/regenerate-benchmark-snapshot.yml`: existing scheduled data refresh; `actions/checkout@v7` is the pin used.
- `docs/runbooks/release-0.0.1-bootstrap.md` § Step 2 "Add secrets": where repo secrets are documented.

### What the llama.cpp UI needs (read from llama.cpp `tools/ui` at `3e4dfa8f3`, prebuilt `b11642`)

- `GET ./props`: required. Unguarded reads: `default_generation_settings.n_ctx`, `default_generation_settings.params`, `model_path` (basename used as the single-model name). The rest is read with `?.`: `modalities.{vision,audio,video}`, `role` (`"router"` means router mode, anything else is single-model), `build_info`, `model_alias`, `ui_settings`.
- `GET ./v1/models`: model list in single-model mode, sent back as `model` in chat requests.
- `POST ./v1/chat/completions`: SSE. Always sends `return_progress`, `sse_ping_interval`, `reasoning_format`. vLLM accepts unknown fields (`extra="allow"`), SGLang ignores them (pydantic default).
- Optional, may 404: `./slots`, `./v1/stream`, `./v1/streams/lookup`, `./v1/chat/completions/control`, `/tools`, `/cors-proxy`.
- Archive: `https://huggingface.co/buckets/ggml-org/llama-ui/resolve/<bNNNN|latest>/dist.tar.gz` plus `dist.tar.gz.sha256`. `latest` was `b11642` on 2026-10-15 (3.1 MB; 10 MB unpacked; `build.json` inside holds `{"version":"b11642"}`). Not every build number is published (`b11643` returned 404).

### Facts checked during planning

- Repo Actions setting `can_approve_pull_request_reviews: false`, and events made with `GITHUB_TOKEN` do not start other workflows. Hence the PAT secret.
- Current crate contents total 7.2 MB uncompressed; the 3.1 MB archive keeps the `.crate` under the 10 MiB crates.io limit (verify, see U1).

## Key Technical Decisions

- **Three UI sources per launch:** the engine's own UI (backend `serves_web_ui`), the bundled UI (UI-less backend and chat mode), or none. A small enum on `RunningEntry` replaces the `serves_ui` bool. Keeps R4: only the trait call and the launch mode decide.
- **Bundled routing by path:** `/props` is answered by the proxy; `/v1/...` is forwarded unchanged to the launch's port with the same `forward::forward_to_upstream` call `forward_ui` uses today (no request log, R13); everything else is an embedded file (`/` serves `index.html`) or 404. Forwarding `/v1/models` straight to the engine gives the UI the name the engine accepts (R8), including gufo's `--served-model-name`.
- **Fake `/props` is minimal:** `role: "model"`, `model_path` = the launch's served name, `default_generation_settings: {n_ctx, params: {}}`, `modalities: {vision, audio: false, video: false}`, `total_slots: 1`, `build_info` naming llamastash and its version. `n_ctx` is the launch's `resolved_ctx` when its backend reports one (llama.cpp, vLLM and SGLang implement `fetch_actuals`), else the launch's requested ctx (`params.ctx`, what `status` shows as `ctx`; for a generic entry it comes from the `ctx: true` knob), else `0`. The generic backend reports no actuals, so generic launches always take the second value.
- **Embedded archive, unpacked once:** `include_bytes!` the `.tar.gz`; on the first bundled-UI request unpack it into an in-memory map (path to bytes) held in a `OnceLock`. About 10 MB of RAM after first use, zero before (R10). Lookups are map keys, never filesystem paths, so there is no path traversal surface.
- **Version record:** `data/llama-ui/VERSION` holds the build (`b11642`) and the archive's sha256. Written only by the refresh script.
- **NOTICE ships in release tarballs:** add a llama.cpp UI entry to `NOTICE` (MIT, the ggml authors, and that the bundle includes npm packages under their own licenses, listed in llama.cpp `tools/ui/package.json`), fix NOTICE's line that says the binary embeds none of its sources, and copy `NOTICE` into the release tarball beside `LICENSE` (R12).
- **Weekly refresh PR:** Monday 04:00 UTC cron plus `workflow_dispatch`. Runs the same script with `latest`; opens a PR from `chore/llama-ui-<build>` with the PAT. Exits early when the secret is unset, nothing changed, or that branch already exists on the remote.

## Open Questions

### Resolved During Planning

- Where the archive lives: `data/llama-ui/dist.tar.gz` (in the crate, not `assets/`).
- Third-party licenses: one NOTICE entry pointing at llama.cpp's `tools/ui/package.json`; no per-package license dump.
- How the PR is opened: PAT secret `LLAMA_UI_BUMP_TOKEN` (user decision).

### Deferred to Implementation

- Whether `params: {}` is enough for the UI's parameter sync, or a few keys are needed. Confirm in the E2E browser check.
- Whether gufo and Halogen accept the extra llama.cpp request fields. Needs the Z13; recorded as a TODO for the user's UAT.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

```text
/ui/... request
  -> resolve_target (cookie -> lone -> chooser), unchanged rules,
     "selectable" = ui source is Engine or Bundled
  -> Forward(entry):
       Engine  -> today's forward_ui (strip /ui, forward everything)
       Bundled -> strip /ui, then:
                    /props        -> fake props JSON (ctx, vision, name)
                    /v1/...       -> forward_to_upstream(entry.port), no log
                    other path    -> embedded file ("/" -> index.html) | 404
```

## Implementation Units

All four run in one lane, in order. None can run in parallel (U2 needs U1's archive; U3 documents U2; U4 calls U1's script).

- [x] **Unit 1: Vendored UI archive, refresh script, NOTICE**

**Goal:** The pinned UI archive is in the repo, a script refreshes it, and the license notice ships.

**Requirements:** R9, R11, R12

**Dependencies:** None

**Files:**
- Create: `scripts/llama-ui/update.sh`
- Create: `scripts/llama-ui/README.md`
- Create: `data/llama-ui/dist.tar.gz` (written by running the script with `b11642`)
- Create: `data/llama-ui/VERSION` (written by the script)
- Modify: `NOTICE`
- Modify: `.github/workflows/release.yml` (the `cp LICENSE README.md CHANGELOG.md` line adds `NOTICE`)
- Also commit: `docs/brainstorms/2026-10-15-engine-web-ui-requirements.md` and this plan (both untracked in the worktree)

**Approach:**
- `update.sh <bNNNN|latest>`: download the `.sha256` then the archive from the HF bucket URL above; refuse unless the `.sha256` body starts with 64 hex chars and the archive matches it; read the real build from `build.json` inside the archive (so `latest` records a concrete `bNNNN`); write `data/llama-ui/dist.tar.gz` and `VERSION` (build line, sha256 line); print the build. Same sha as the current `VERSION`: change nothing and say so. Download into a `mktemp -d` dir and extract only `build.json` from the archive (never unpack the whole tree into `/tmp`).
- Bash with `set -euo pipefail`, `curl`, `sha256sum` (or `shasum -a 256` on macOS), `tar`. Short header comment saying what it does and how to run it.
- `scripts/llama-ui/README.md`: what the script does, how to run it by hand, that the weekly workflow (U4) runs it, and the manual check to do before merging a bump (open `/ui` on one UI-less engine and send a message).

**Patterns to follow:** `scripts/vllm/README.md`; `scripts/regenerate-benchmark-snapshot.py` for a data refresh.

**Test scenarios:**
- Test expectation: none -- data file and maintainer script. Covered by verification below.

**Verification:**
- `scripts/llama-ui/update.sh b11642` writes both files; `VERSION` says `b11642`; running it again reports no change and leaves `git status` clean.
- `cargo package --no-verify --allow-dirty` produces a `.crate` under 10 MiB, and `cargo package --list --allow-dirty` includes `data/llama-ui/dist.tar.gz`.

- [x] **Unit 2: Serve the bundled UI for UI-less chat launches**

**Goal:** `/ui` opens a working chat for a UI-less chat launch, with llama.cpp launches unchanged.

**Requirements:** R1-R10, R13, R14

**Dependencies:** Unit 1

**Files:**
- Create: `src/proxy/ui_bundle.rs` (embedded archive, lazy unpack, file lookup with content type, fake `/props` body)
- Modify: `src/proxy/ui.rs`
- Modify: `src/proxy/mod.rs` (module declaration)
- Test: `src/proxy/ui_bundle.rs` (inline tests), `src/proxy/ui.rs` (inline tests)

**Approach:**
- Replace `RunningEntry.serves_ui: bool` with a three-value UI source (engine / bundled / none). Compute it in `collect_running` from the backend's `serves_web_ui()` (as today) and the launch's mode: UI-less and chat means bundled. Keep the mapping a tiny pure function so it is unit-testable without a daemon.
- `resolve_target` and `chooser_html` treat engine and bundled the same ("selectable"). Chooser copy stays: green opens, red has no web UI.
- `forward_ui` branches: engine source is today's path untouched. Bundled source routes by the stripped path as in the design sketch. The `/props` answer needs the launch's resolved ctx (same lookup as `src/ipc/status.rs`) and the catalog row's vision flag; read them only when answering `/props`, for that one launch.
- `ui_bundle.rs`: `include_bytes!` of `data/llama-ui/dist.tar.gz`; `OnceLock` map built on first use with `flate2` + `tar`, keys normalized without the leading `./`; content type by extension (`html`, `js`, `css`, `svg`, `png`, `ico`, `json`, `webmanifest`, default `application/octet-stream`); query string ignored for lookup.
- Update the module doc comment at the top of `ui.rs` (it says the UI is always the backend's own).
- No backend names anywhere in this code (R4).

**Patterns to follow:** existing `forward_ui`, `html_response`, `empty_body`; tests at the bottom of `ui.rs` (`entry_ui`, `resolve_target_*`, `chooser_rows_are_green_with_a_ui_and_red_without`).

**Test scenarios:**
- Happy path: UI source mapping: UI-serving backend in any mode -> engine; UI-less backend in chat mode -> bundled; UI-less in embedding or rerank mode -> none.
- Happy path: `resolve_target` with one bundled entry and no cookie forwards to it; a cookie pinned to a bundled entry forwards to it.
- Edge case: one none-source entry alone still shows the chooser (existing rule), and a cookie pinned to it falls through to the chooser.
- Happy path: `chooser_html` renders a bundled entry as a green link to `/ui/?target=<id>` and a none entry as a red, non-link row.
- Happy path: bundled path routing: `/props` and `/props?x=1` -> fake props; `/v1/models`, `/v1/chat/completions` -> forward; `/` -> `index.html`; `/slots`, `/tools`, `/nope.js` -> 404.
- Happy path: embedded lookup: `/` returns `index.html` with `text/html`; the archive's `_app/immutable/bundle.*.js` (found by scanning keys, not a hardcoded hash) returns a JavaScript content type and non-empty bytes.
- Happy path: fake props JSON: `n_ctx` equals the ctx passed in; `modalities.vision` follows the flag; `role` is `"model"`; `model_path` equals the served name; `default_generation_settings.params` is an object.
- Integration: existing `tests/proxy_ui.rs` (llama.cpp path) stays green unchanged.

**Verification:**
- Unit tests above pass; `tests/proxy_ui.rs` passes.
- E2E against a real engine, isolated daemon (see Verification below): a generic entry running real `llama-server --no-ui` on a 1B GGUF opens the bundled UI at `/ui/`, `/ui/props` reports the same ctx as `status --json` `ctx` for that launch (4096), a streamed `/ui/v1/chat/completions` returns tokens, `/ui/slots` returns 404. A llama.cpp launch beside it still serves its own UI when pinned.

- [x] **Unit 3: Docs, CHANGELOG, TODO**

**Goal:** Docs match the new behavior; open work is indexed.

**Requirements:** R15, R16

**Dependencies:** Unit 2

**Files:**
- Modify: `docs/usage.md` § Web UI (`/ui`)
- Modify: `docs/architecture.md` (backend-neutrality table row for `/ui` and any `/ui` description)
- Modify: `AGENTS.md` (the "Proxy scope" bullet: one-line edit, no growth)
- Modify: `CHANGELOG.md` (`[Unreleased]` one-liner)
- Modify: `TODO.md` (R15 batch 5)
- Modify: `docs/plans/2026-10-15-001-feat-engine-web-ui-plan.md` (tick units)

**Approach:**
- `docs/usage.md`: engines without their own UI get an embedded copy of llama.cpp's UI (version from `data/llama-ui/VERSION`), chat launches only. Limits: no thinking text on vLLM (it sends `delta.reasoning`, the UI reads `delta.reasoning_content`); llama.cpp-only features (slots view, stream resume, server tools) are off; after switching between a llama.cpp model and a bundled-UI model one reload can show the other UI version (service worker cache). Update the chooser sentence (green = opens, red = not a chat launch / no UI). Say which engines were tested live and with which version.
- `TODO.md`: strike the batch 5 item with a pointer to this plan; add one line for router mode (approach B, origin doc § Key Decisions); add one line for live UAT of the bundled UI on vLLM, SGLang, gufo and Halogen (the lane can only test generic `llama-server --no-ui` on the XPS), including whether gufo and Halogen accept the extra request fields.
- Follow the writing rules in `AGENTS.md`: plain facts, short.

**Test scenarios:**
- Test expectation: none -- docs only.

**Verification:**
- Every doc statement about `/ui` matches U2's behavior; no doc still says only llama.cpp launches open.

- [ ] **Unit 4: Weekly refresh workflow**

**Goal:** A PR shows up when upstream publishes a newer UI.

**Requirements:** R17, R11

**Dependencies:** Unit 1

**Files:**
- Create: `.github/workflows/update-llama-ui.yml`
- Modify: `docs/runbooks/release-0.0.1-bootstrap.md` (§ Step 2: add `LLAMA_UI_BUMP_TOKEN`)
- Modify: `scripts/llama-ui/README.md` (mention the workflow and the secret)

**Approach:**
- Triggers: `schedule` (Monday 04:00 UTC) and `workflow_dispatch`. `permissions: contents: read`; the PAT does the writes.
- Steps: exit 0 with a `::notice::` when `LLAMA_UI_BUMP_TOKEN` is empty; `actions/checkout@v7` with that token; run `scripts/llama-ui/update.sh latest`; exit 0 when `data/llama-ui` is unchanged; exit 0 when `chore/llama-ui-<build>` already exists on the remote; else commit as `github-actions[bot]` with `chore(proxy): bump bundled llama.cpp UI to <build>`, push the branch, `gh pr create` (with `GH_TOKEN` set to the PAT) and a body naming the build, sha256, and the manual `/ui` check from `scripts/llama-ui/README.md`.
- Runbook entry: fine-grained PAT on `llamastash/llamastash` with Contents and Pull requests read/write; the workflow is a no-op until it exists.

**Test scenarios:**
- Test expectation: none -- CI workflow; cannot run locally.

**Verification:**
- YAML parses (`python -c 'import yaml,sys; yaml.safe_load(open(sys.argv[1]))' .github/workflows/update-llama-ui.yml` or `actionlint` if installed).
- The script call and branch/PR logic read correctly against the script's actual output.

## Verification

Setup (fresh worktree): no extra setup; cargo builds into the worktree's `target/`.

While coding:
- `cargo check`
- `cargo nextest run --features test-fixtures --lib proxy::ui`
- `cargo nextest run --features test-fixtures --test proxy_ui`

Before the last commit: the pre-commit hook runs `cargo fmt`, `make lint` and `make doc`. Do not run them by hand; read the hook output only if it fails.

E2E (required, user-visible change), isolated so the user's daemon is never touched. Export the env vars in every shell call, because they do not persist between calls:
- `LLAMASTASH_STATE_DIR`, `LLAMASTASH_CONFIG_DIR`, `LLAMASTASH_CACHE_DIR` under the worktree's `target/e2e-ui/`; daemon on a non-default proxy port (e.g. `--proxy-port 11545`).
- Test generic engine (the user asked for one): real `llama-server` with its UI turned off, so llamastash treats it as a UI-less engine. Config: `model_paths: [/mnt/work/b4r2/models]` and one `backend.generic.servers` entry: `name: noui`, `model: Llama-3.2-1B-Instruct-Q4_K_M`, `binary: /mnt/work/Workspace/llms/llama.cpp/build-cuda/bin/llama-server` (b11658), `args: [-m, "{model}", --host, "{host}", --port, "{port}", --no-ui]`, `knobs: [{flag: -c, ctx: true, default: "4096"}]`, `ready: /health`, `modes: [chat]`. Check `docs/usage.md` § Generic backend for field spelling.
- `cargo build --bin llamastash`; start the daemon from `target/debug/llamastash`; `start Llama-3.2-1B-Instruct-Q4_K_M --server generic-noui`; then `curl` `/ui/` (bundled `index.html`), `/ui/props` (`n_ctx` matches `status --json` `ctx`, 4096), a streamed `POST /ui/v1/chat/completions` (tokens arrive), `/ui/slots` (404).
- Leave the isolated config at `target/e2e-ui/` and say in the result how to start it, so the orchestrator can reuse it for a browser check.
- Also start the same model on llama.cpp (`--name e2e`), confirm `/ui/switch` lists both green, and `/ui/?target=<llama.cpp launch>` then `/ui/props` returns llama-server's own props (has `build_info` from llama.cpp).
- Stop the isolated daemon (`daemon stop`) and every launch it started when done.

## System-Wide Impact

- **Interaction graph:** only `/ui/*` handling changes. `/v1/*`, `/api/*`, `/metrics` untouched.
- **Unchanged invariants:** llama.cpp `/ui` path, cookie and chooser rules, LAN Basic auth, no request-log entries for `/ui`, the `/v1` no-body-translation rule.
- **State lifecycle:** the unpacked map lives for the daemon's life once built (about 10 MB). Nothing persisted.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| A future UI bump reads a `/props` field we don't fake, and the chat screen errors | Each bump arrives as a PR; README asks for a manual `/ui` check before merging |
| gufo or Halogen reject the extra llama.cpp request fields | TODO for Z13 UAT; documented as untested |
| Release binary grows about 3.1 MB | Accepted (user decision); `.crate` size checked in U1 |
| PAT expiry silently stops refresh PRs | Workflow notice in the run log; token rotation already tracked in the runbook's rotation section |

## Sources & References

- Origin: `docs/brainstorms/2026-10-15-engine-web-ui-requirements.md`
- Prior `/ui` plan: `docs/plans/2026-06-15-001-feat-proxy-ui-surface-plan.md`
- llama.cpp UI source: `tools/ui/` at `3e4dfa8f3`; asset provisioning: `scripts/ui-assets.cmake`
- vLLM `vllm/entrypoints/generate/base/protocol.py` (`DeltaMessage.reasoning`) at v0.31.0; SGLang `python/sglang/srt/entrypoints/openai/protocol.py` at v0.5.21
