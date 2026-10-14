---
title: "feat: prefill speed and cached prompt tokens in the request log"
type: feat
status: active
date: 2026-10-14
---

# feat: prefill speed and cached prompt tokens in the request log

## Overview

The request log shows generation speed (`Tok/s`, `Avg tok/s`, `Agg tok/s`) but nothing about prompt processing. llama-server already reports prefill time and cache reuse on most responses, and the proxy's usage tap already reads the `timings` object they come in, but it keeps only the token counts. This plan keeps the prefill figures, estimates them where the engine sends none, and shows them on every surface the generation figures are on: request rows, the summary, the CLI table, the TUI Requests tab, `status --json`, and `/metrics`.

## Problem Frame

Agent tools resend a long, mostly cached prompt on every turn. How fast the uncached part is processed, and how much of the prompt the cache covers, decide the wait before the first token. Today a user has to start llama-server with `--metrics` and read `/metrics/backend` to see either, and nothing is per request.

## Requirements Trace

- R1. Per row: prefill speed (engine-reported, or estimated and marked), the prompt tokens the engine computed, and the prompt tokens served from cache.
- R2. Summary: one prefill tok/s figure for the scope, plus cached tokens and a cache-hit ratio.
- R3. CLI `llamastash requests`: a `PP/S` column (padded and piped) and the summary lines.
- R4. TUI Requests tab: a `PP/s` column and the summary strip figures.
- R5. `/metrics` (text and JSON) and `status --json` `request_stats` carry the new figures.
- R6. Docs, CHANGELOG and tests ship with each unit.

## Scope Boundaries

- No `--metrics` added to llama.cpp launches by default (user decision). `/metrics/backend` keeps showing the engine's own prefill counters only when the user starts the engine with it.
- No prefill "aggregate" (union-of-windows) figure. The summary figure is token-weighted (see D5), which already answers "how fast does prefill run".
- No launch-level (`llamastash:launch_*`) variants of the new metric series; model level only.
- No per-row prefill time column; TTFB already shows the wait.
- No speculative-decoding (`draft_n` / `draft_n_accepted`) figures.

## Context & Research

What llama-server sends, checked in its source at upstream `62df1815b` (2026-10-14; the local CPU build is `1b70350c9`, one day older):

- `server_slot_stats::to_json()` (`tools/server/server-common.cpp`) builds `timings` = `{cache_n, prompt_n, prompt_ms, prompt_per_token_ms, prompt_per_second, predicted_n, predicted_ms, predicted_per_token_ms, predicted_per_second}` plus `draft_n`/`draft_n_accepted` when drafting. `prompt_n` is the prompt tokens actually computed; `cache_n` is the prompt tokens reused from the KV cache; `prompt_per_second` = `prompt_n * 1000 / prompt_ms`. `prompt_ms` is the engine's own processing time, without queue wait.
- `timings` is attached (when `stats.is_set()`) on: `/completion`, `/v1/completions`, `/v1/chat/completions` whole and the last chunk of a stream, and streamed `/v1/responses`. It is NOT attached on `/v1/messages` (whole or streamed) or on whole `/v1/responses`.
- Cached counts in `usage` on every route: `usage.prompt_tokens_details.cached_tokens` (OpenAI chat/completions), `usage.input_tokens_details.cached_tokens` (Responses), `usage.cache_read_input_tokens` (Anthropic). On the Anthropic route llama.cpp sends `input_tokens = n_prompt_tokens - n_prompt_tokens_cache`, so `input_tokens + cache_read_input_tokens` is the whole prompt, which is what the tap already computes.

Relevant code:

- `src/proxy/usage_tap.rs`: `Usage` (what the tap read), `Fields::scan` (reads `usage`, `timings`, `metrics` objects by key), `Fields::usage`, `Usage::estimate_speed`. Real llama.cpp bodies are already used as test fixtures there.
- `src/proxy/forward.rs`: `let sent = Instant::now()` before the upstream send (after routing and any auto-start); `LoggedResponse` (`generating_since`, `streamed`, `observe` marks the first data byte, `Drop` reads the tap and calls `estimate_speed` only for `RequestState::Done`, then `record.finish`).
- `src/proxy/request_log.rs`: `RequestRow` (`tokens_per_second`, `tokens_per_second_estimated`), `RequestCells` (`speed` with the `~` estimate mark), `RequestRow::cells`, `RequestSummary` + `RequestSummary::cells` (the label/value list both the CLI summary block and the TUI strip render), `Totals::add` / `Totals::summary` (since-daemon-start sums), `RequestRecord::finish`.
- `src/cli/requests.rs`: header vector and `cells_for` (padded and piped share the order), `fit_padded_table`.
- `src/tui/tabs/requests.rs`: `COLUMNS` (id, label, width, rank), `ColumnId`, the row renderer's match on `ColumnId`.
- `src/proxy/metrics.rs`: `model_series` (per-model counters such as `llamastash:prompt_tokens_total`), the gauge block that emits `llamastash:request_tokens_per_second_avg` / `_aggregate`, and the `metrics_series_names_match_the_docs` test that diffs emitted names against `docs/usage.md`.
- `tests/fixtures/fake_llama_server.rs`: a request body containing `__TEST_EMIT_TIMINGS__` makes the fake stream a final chunk with a llama.cpp-shaped `timings` (`cache_n`, `prompt_n`, `prompt_ms`, `prompt_per_second`, ...).

## Key Technical Decisions

- **D1. Row fields.** `RequestRow` gains `prefill_tokens: Option<u64>` (prompt tokens the engine computed, cache hits left out), `prefill_tokens_per_second: Option<f64>`, `prefill_estimated: bool`, and `cached_tokens: Option<u64>`. JSON keys use the same names. Follow the struct's existing serde attributes so rows written by an older daemon still load. `Usage` gains the same four.
- **D2. Engine-reported prefill.** From `timings`: `prefill_tokens = prompt_n`, `prefill_tokens_per_second = prompt_n * 1000 / prompt_ms` when both are present and greater than 0. Computing it from the two inputs keeps one formula for the reported and estimated cases; a test asserts it equals the fixture's `prompt_per_second`.
- **D3. Cached tokens.** First present of: `timings.cache_n`, `usage.prompt_tokens_details.cached_tokens`, `usage.input_tokens_details.cached_tokens`, `usage.cache_read_input_tokens`. The nested `*_details` objects need a nested read inside the `usage` object. Matching stays on field names, never on which backend answered.
- **D4. Estimate where the engine sends no prefill timing.** Only for `RequestState::Done` rows (same rule as `estimate_speed`), and only when `prompt_tokens` and `cached_tokens` are both known and `uncached = prompt_tokens - cached_tokens > 0`.
  - Streamed response: prefill window = first data byte minus `sent`.
  - Whole response: prefill window = (end minus `sent`) minus `completion_tokens / tokens_per_second`, and only when `tokens_per_second` was engine-reported (not estimated: an estimated whole-response speed already spans the whole wait, which leaves no prefill window). Skip when the window is not positive.
  - `prefill_tokens = uncached`, `prefill_tokens_per_second = uncached * 1000 / window_ms`, `prefill_estimated = true`.
  - Measured from `sent`, not from arrival, so routing and an auto-start's model load stay out. Queue wait inside the engine stays in, so the estimate reads low when requests wait for a slot. Say so in the docs.
  - Why the cached count is required: without it the proxy cannot tell computed tokens from cache hits. An agent prompt is mostly cache hits, so assuming none would overstate prefill speed many times over.
  - Where this lands in practice: llama.cpp `/v1/messages` streams (Claude Code) and whole `/v1/responses`. Engines that send neither `timings` nor a cached count show `-`.
  - Shape: a `Usage::estimate_prefill(window)` beside `estimate_speed`; `forward.rs` computes the window. `LoggedResponse` keeps `sent` and the first-data-byte instant it needs.
- **D5. Summary figure is token-weighted.** `Totals` sums `prefill_tokens` and `prefill_tokens / prefill_tokens_per_second` (seconds) over every finished row with a prefill speed, engine or estimated, whatever state it ended in. `RequestSummary.prefill_tokens_per_second = sum tokens / sum seconds`. Not the mean of per-row speeds: agent turns prefill a few hundred uncached tokens at a low per-token rate, a cold long prompt runs much faster, and a plain mean would mostly show the small turns.
- **D6. Cache figures.** `Totals` sums `cached_tokens` and the `prompt_tokens` of rows that carry a cached count. `RequestSummary` gains `cached_tokens: u64` and `cache_hit_ratio: Option<f64>` (cached over those prompt tokens; `None` when no row carried a cached count). Since-daemon-start like the other `Totals`.
- **D7. Cell text.** `RequestCells.prefill`: `-` (the `none` text) when absent, `~` prefix when estimated, one decimal below 1000 and a whole number from 1000 up, so `~12345` still fits 6 cells. `RequestSummary::cells` adds `Prefill tok/s` right after `Agg tok/s` (same number format, no `~`), and `Cached` right after `Tokens in` as `<fmt_count> (<pct>%)`, `none` when `cache_hit_ratio` is `None`.
- **D8. CLI.** `PP/S` header right after `TOK/S`, padded and piped (field order changes once more; say so in the CHANGELOG bullet). Name caps and the NOTE wrap are unchanged.
- **D9. TUI.** `PP/s` column right after `Tok/s`, width 6, rank 75: it drops after `Route` (100), `Client` (90) and `Request for` (80), before `Note` (70). The summary strip picks up the new cells from `RequestSummary::cells`.
- **D10. Metrics.** Per model, in `model_series` beside `llamastash:prompt_tokens_total`: counters `llamastash:prompt_tokens_cached_total`, `llamastash:prefill_tokens_total`, `llamastash:prefill_seconds_total` (so `rate(prefill_tokens_total) / rate(prefill_seconds_total)` works in Prometheus); and gauge `llamastash:request_prefill_tokens_per_second` beside `request_tokens_per_second_avg`, emitted only when the figure is `Some`. `/metrics?format=json` and `status --json` `request_stats` get the summary fields through `RequestSummary`.

## Implementation Units

Run sequentially; every unit touches `src/proxy/request_log.rs`.

- [ ] **Unit 1: capture prefill and cache figures per row**
  - Files: `src/proxy/usage_tap.rs`, `src/proxy/forward.rs`, `src/proxy/request_log.rs` (row fields, `finish`, `cells`), `tests/proxy_*.rs` (one integration test through the fake), `docs/usage.md` (row field list for `requests --json`), `docs/architecture.md` (only if it describes what the tap reads).
  - Tests (`usage_tap.rs` unit tests, real llama.cpp body shapes):
    - Whole chat body with `timings` (`prompt_n`, `prompt_ms`, `cache_n`) and `usage.prompt_tokens_details.cached_tokens`: prefill tokens, speed (equals `prompt_per_second`), cached from `cache_n`, not estimated.
    - Streamed chat: `timings` only on the last chunk; same fields read.
    - Anthropic whole body (`input_tokens`, `cache_read_input_tokens`, no `timings`): cached read, no prefill speed from the tap.
    - Responses body with `input_tokens_details.cached_tokens`: cached read.
    - `estimate_prefill`: sets speed and `prefill_estimated` from a window; never replaces an engine figure; no estimate without a cached count; no estimate when `uncached` is 0; no estimate for a zero window.
    - `forward.rs` window: a streamed response estimates from first byte minus send; a whole response with an engine speed estimates from end minus send minus generation time; a whole response whose speed was estimated gets no prefill estimate; a non-`Done` row gets no estimate.
    - Integration: a request through `fake_llama_server` with `__TEST_EMIT_TIMINGS__` logs a row with `prefill_tokens` 1, the speed from `prompt_ms` 25.704, and `cached_tokens` 40.
  - Commit: `feat(proxy): log prefill speed and cached prompt tokens per request`.

- [ ] **Unit 2: summary, CLI and TUI**
  - Files: `src/proxy/request_log.rs` (`Totals`, `RequestSummary`, `RequestSummary::cells`), `src/cli/requests.rs`, `src/tui/tabs/requests.rs`, `tests/golden/requests-view*.txt`, `docs/usage.md` (`requests` section: header, columns, summary lines, reprint the padded example from the renderer, which also fixes the missing 5-space indent on its `Requests` line; TUI Requests tab section; `status --json` `request_stats` key list), `CHANGELOG.md` (one bullet under `[Unreleased]`, bundled with the existing request-log bullet if one fits).
  - Tests:
    - Token-weighted figure: rows of 100 tokens at 100 tok/s (1 s) and 10,000 tokens at 5,000 tok/s (2 s) give 3,366.7, not the mean 2,550.
    - Estimated and engine rows both count; a row without a prefill speed counts toward neither sum.
    - Cache ratio: rows with cached counts give `cached / their prompt tokens`; a row without a cached count is left out of the denominator; no cached rows gives `None`.
    - Cell text: `-`, `~` mark, one decimal under 1000, whole number from 1000.
    - CLI: header order `... TOK/S PP/S TOTAL ...` in padded and piped; existing NOTE-wrap alignment test still passes.
    - TUI: column order and that `PP/s` drops after `Request for` and before `Note` as the pane narrows; goldens updated and eyeballed.
  - Commit: `feat(requests): show prefill speed and cache hits in the table and summary`.

- [ ] **Unit 3: metrics**
  - Files: `src/proxy/metrics.rs`, `docs/usage.md` (Metrics series list, so `metrics_series_names_match_the_docs` passes).
  - Tests: the three counters and the gauge appear for a model with prefill rows; the gauge is absent when no row has a prefill speed; JSON form carries the summary fields.
  - Commit: `feat(proxy): export prefill and cache counters on /metrics`.

- [ ] **Unit 4: E2E and doc sweep**
  - Run the E2E below, then check `README.md`, `docs/architecture.md`, `docs/usage.md`, `CHANGELOG.md` for any statement the new fields make wrong. Tick this plan's checkboxes.
  - Commit only if the sweep changed something: `docs: ...`.

## Verification (commands)

While coding (narrowest first):
- `cargo check --features test-fixtures`
- `cargo nextest run --features test-fixtures --lib usage_tap`
- `cargo nextest run --features test-fixtures --lib request_log`
- `cargo nextest run --features test-fixtures --lib forward`
- `cargo nextest run --features test-fixtures --lib metrics`
- `cargo nextest run --features test-fixtures --lib requests` (CLI and TUI requests unit tests)
- `cargo nextest run --features test-fixtures --test <the integration file you add the fake-engine test to>`
- Goldens: `UPDATE_GOLDEN=1 cargo nextest run --features test-fixtures --test tui_e2e_render_test`, then without `UPDATE_GOLDEN`, then `git diff tests/golden/`.

Before the last commit (once, in the foreground, not detached): `cargo nextest run --features test-fixtures --no-fail-fast` and `cargo test --doc --features test-fixtures`. If a test fails on the 3 s `runtime.json` deadline, run its file alone and then the full suite once more before calling it a regression.

E2E (isolated; never the user's daemon on port 11435, never the daemon on port 11599, nothing under `target/r15-e2e`; never load a real model). In every shell call that talks to this daemon, export fresh temp `LLAMASTASH_STATE_DIR`, `LLAMASTASH_CONFIG_DIR`, `LLAMASTASH_CACHE_DIR`, and `LLAMASTASH_LLAMA_SERVER=$HOME/Workspace/llms/llama.cpp/build-cpu/bin/llama-server` (the daemon refuses to start without a llama-server path):
- `cargo build --bin llamastash`; `target/debug/llamastash daemon start --proxy-port 0`; proxy address from `status --json | jq -r .proxy.listen`.
- One `POST /v1/chat/completions` for a model that does not exist; `requests --json | jq '.summary, .requests[0]'` shows the new keys (`null` / `false` / `0` as designed); `requests | cat` shows `PP/S` after `TOK/S`; the padded table under `script -qc '<cmd>' /dev/null`; `curl -s "<proxy>/metrics?format=json" | jq .summary`.
- `target/debug/llamastash --render --render-size 160x45` and `100x30` for the TUI.
- `daemon stop`.

## Definition of Done

- Units 1-3 committed separately on `feat/r15-batch2`, docs and tests in the same commit as the code they describe.
- Full nextest suite and doctests pass; pre-commit hook passes.
- E2E done and summarized; plan checkboxes ticked.

## Sources & References

- llama.cpp upstream `62df1815b`: `tools/server/server-common.cpp` (`server_slot_stats::to_json`), `tools/server/server-task.cpp` (where `timings` and the cached-token fields are attached per route).
- Related plan: `docs/plans/2026-10-14-001-feat-r15-request-log-metrics-plan.md` (the aggregate tok/s and estimate conventions this follows).
