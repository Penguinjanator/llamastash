---
title: "feat: R15 batch 2 — request log and metrics surface"
type: feat
status: active
date: 2026-10-14
---

# feat: R15 batch 2 — request log and metrics surface

## Overview

Five open items from `TODO.md` § R15 → Batch 2, all in the proxy request log and the surfaces that render it (`llamastash requests`, the TUI Requests tab, `GET /metrics`, `status --json` `request_stats`):

1. An aggregate throughput figure beside `Avg tok/s`.
2. An `alias` note on a request that reached its model through `proxy.aliases`.
3. TUI Requests tab: narrower `Note`, wider `Request for`.
4. `/metrics?format=json` reads the request log once instead of three times.
5. `llamastash requests` caps `MODEL` at 24 cells like `REQUEST FOR`.

## Problem Frame

`Avg tok/s` is the mean per-stream speed. When one launch serves several tools at once it reads one slot, not the box: measured on a Z13 (`multi-agent-max`, 4 parallel generations, 214 s) the rows read `Avg tok/s 19.0` while the engine's own `llamacpp:*` counters read 50.3 tok/s aggregate. There is no figure that says what the launch is producing in total.

The other four are small fixes to the same surfaces: an alias request is not marked as one, the TUI gives `Note` width that `Request for` needs, a `/metrics` JSON scrape takes three separate log reads that can disagree, and the CLI caps one model-name column but not its neighbour.

## Requirements Trace

- R1. Summaries carry an aggregate throughput: Σ `completion_tokens` / busy wall, computed over the rows the 1000-row ring holds for the scope (not the 200-row TUI tail), together with the busy wall it averages over.
- R2. The throughput appears everywhere `Avg tok/s` does: TUI summary strip, `llamastash requests` summary block, `requests --json` / `requests_tail`, `status --json` `request_stats`, `/metrics?format=json`, and a gauge in the `/metrics` text format.
- R3. A row whose model came from a `proxy.aliases` entry carries that fact on the wire and shows `alias` in its note.
- R4. TUI `Request for` is 24 cells wide (matching the CLI cap); `Note`'s minimum width drops from 12 to 10, and `Note` still grows into spare width.
- R5. One `/metrics?format=json` request takes one `LogSnapshot` and every part of the body reads from it.
- R6. The padded `llamastash requests` table caps `MODEL` at 24 cells with `...`, like `REQUEST FOR`. Piped and `--json` output stay whole.
- R7. Docs, `CHANGELOG.md` and `TODO.md` updated in the same commits.

## Scope Boundaries

- No change to how `Avg tok/s`, `Tok/s session` or `tokens_per_second_last` are computed.
- No second "average concurrency" figure (TODO shape C): it needs the same busy-wall union as shape A and only approximates A's number.
- No `alias` note for a request the alias failed to route (dead or ambiguous target). Those rows already carry the proxy's error.
- No change to the Ollama `/api/show` path or the `/metrics` `resolve_launch` scope lookup beyond threading the snapshot; neither records a request row.
- The real-engine cross-check (rate from `llamacpp:tokens_predicted_total` vs our figure) is a later hardware check, not part of this change. Do not load real models on this host.

## Context & Research

### Relevant Code and Patterns

- `src/proxy/request_log.rs`
  - `RequestRow` (wire row, `#[serde(default)]`, every key always present), `RequestRow::note()`, `RequestRecord` setters (`set_fallback`, `set_auto_start`).
  - `RequestSummary` + `RequestSummary::cells()` — the one place the strip/CLI block text comes from.
  - `Totals` are since-daemon-start counters. The client figures (`hosts`, `connections`, `live_*`) are instead computed over ring rows through `ClientPair` and stamped onto a summary in both `RequestLog::tail` and `RequestLog::snapshot`. **The throughput follows the `ClientPair` pattern, not `Totals`**: it needs per-row intervals, which only the ring has.
  - `fmt_ms` formats a duration for a cell (`850ms`, `1.2s`, `2m05s`, `1h02m`).
  - `LogSnapshot` (rows, `all`, `models`, `launches`, `routes`) and its `in_flight` / `launch_path` helpers.
- `src/proxy/metrics.rs`
  - `serve` takes `live` once and threads `&[LiveLaunch]` through `parse_request`, `json_body` and the text render. That is the pattern for threading the snapshot.
  - Today: `parse_request` → `resolve_launch` takes snapshot #1 (only when a launch is named), `serve` takes snapshot #2, `json_body` calls `log.tail(...)` for read #3.
  - The text-format gauges for `request_tokens_per_second_avg` (around the `s.tokens_per_second_avg` use) are the pattern for the new gauge.
- `src/proxy/route.rs`
  - `resolve_reference` → `resolve_aliased_reference` is where an alias supplies the row. `resolve_client_reference` (callers: `route::decide`, `router.rs` `/api/show`, `metrics.rs` `resolve_launch`) returns `(Option<launch name>, CatalogRow)`. `<alias>@<launch>` goes through the same `resolve_reference` on the model half.
  - `RouteDecision::{ReadyAt, NotRunning, BackendUnavailable}` are the variants the router uses to `set_model` on the record (`src/proxy/router.rs`, right after `route::decide`).
- `src/proxy/forward.rs` `LoggedResponse::generating_since`: for a streamed response the speed estimate starts at the first SSE chunk; for a response that arrives whole it starts when the request was sent. So `ttfb_ms` ≈ `duration_ms` on a non-streamed row.
- `src/tui/tabs/requests.rs`: `COLUMNS` (`RequestFor` width 14, rank 80), `MIN_NOTE_W = 12`, `summary_lines` (wraps cells, never splits one).
- `src/cli/requests.rs`: `fit_padded_table` caps the `REQUEST FOR` column with `cap_cells(cell, REQUEST_FOR_MAX_W)`; `requests_human` prints `summary.cells(NONE)` as a kv block on a TTY.
- Tests: inline `#[cfg(test)] mod tests` in each file above; `tests/proxy_routing.rs` has alias routing tests (`proxy_state_with_aliases`, `proxy_alias_reaches_the_model_it_names`, `a_real_model_id_beats_an_alias_of_the_same_name`) and reads rows via `ctx.requests.tail(...)`; `tests/tui_e2e_render_test.rs` goldens `tests/golden/requests-view.txt` and `tests/golden/requests-view-wide.txt` (regenerate with `UPDATE_GOLDEN=1`).

### Institutional Learnings

- No `docs/solutions/` in this repo. `AGENTS.md` rules that apply: docs ship with code; CHANGELOG one-liners under `[Unreleased]`; `TODO.md` items struck as `- [x] ~~…~~ — done: …`; keybinding/glyph rules (no new glyphs needed here); clippy denies `shadow_unrelated`; two-space indent.

## Key Technical Decisions

- **Throughput window per row = the row's generation time, ending at the row's end.** For a finished row with `completion_tokens > 0`, `tokens_per_second > 0` and `duration_ms`: `end = started_at_ms + duration_ms`, `start = max(started_at_ms, end − completion_tokens / tokens_per_second)`. The TODO's `[started_at + ttfb, started_at + duration]` window is the same thing for a streamed row, but on a non-streamed row `ttfb ≈ duration`, so the window is near zero and one such request would read thousands of tok/s. The per-row speed (server-reported, or the proxy's estimate from its own clock) gives the generation time for both kinds of row with no extra field. Same effort as the TODO's window, and correct for non-streamed rows.
- **Busy wall = length of the union of those windows.** Sort by start, merge overlaps, sum. ≤1000 rows per scope, so a sort per scope is cheap.
- **Throughput = Σ completion_tokens of the counted rows / busy wall seconds.** `None` when no row qualifies or busy wall is 0. In-flight rows and rows without tokens or speed are skipped for both sums.
- **Computed over the ring rows the scope holds**, stamped onto the summary in `RequestLog::tail` and `RequestLog::snapshot` (all / per model / per launch), the same way `ClientPair` is. Extend that per-row accumulator (or add one beside it that both call sites share) so the two paths cannot compute it differently.
- **Wire fields on `RequestSummary`:** `tokens_per_second_aggregate: Option<f64>` and `busy_ms: Option<u64>`, both `null`/absent-tolerant like the other optional figures.
- **Cell:** label `Agg tok/s`, text `<tps:.1> over <fmt_ms(busy_ms)>` (e.g. `Agg tok/s 50.3 over 3m34s`), placed right after `Avg tok/s` (before `Tok/s session`). `none` placeholder when absent, like `Avg tok/s`.
- **Prometheus text:** one gauge `llamastash:request_tokens_per_second_aggregate{model}` emitted where `request_tokens_per_second_avg` is, with HELP text saying it covers the busy time of the rows the request log holds.
- **Alias flag:** `RequestRow.alias: bool` ("the client named a `proxy.aliases` entry; `requested_model` is the alias, `model` the model it points at"). Set only when `proxy.aliases` supplied the routed row, whole reference or the model half of `<alias>@<launch>`. A name that is a real model and also an alias (shadow case) is not an alias request. `resolve_client_reference` reports whether the alias table supplied the row; `decide` carries it on the routed variants; the router calls a new `RequestRecord::set_alias()` next to `set_model`. The other two `resolve_client_reference` callers ignore it.
- **Note order:** state words, then `alias`, then `auto-start`, `unloaded …`, `fallback (…)`, error. E.g. `alias, auto-start, fallback (launch_failed)`.
- **Single snapshot:** `serve` takes `state.ctx.requests.snapshot()` once, before `parse_request`, and passes `&LogSnapshot` to `parse_request` → `resolve_launch` and to `json_body`. Add `LogSnapshot::tail(&LogScope, limit) -> Tail` that answers exactly what `RequestLog::tail` answers for the same log state (summary incl. client figures, throughput, and `tokens_per_second_session` for a model scope; rows newest first). `json_body` uses it and no longer takes `&RequestLog`.
- **MODEL cap:** generalize the cap loop in `fit_padded_table` to both `MODEL` and `REQUEST FOR` with one constant (rename `REQUEST_FOR_MAX_W` to a name that covers both, value 24).
- **TUI widths:** `RequestFor` width 24 (update its comment: matches the CLI cap), `MIN_NOTE_W` 10 (`auto-start` fits). Ranks unchanged.

## Open Questions

### Resolved During Planning

- Shape A vs C: A. Both need the busy-wall union, so C is not simpler, and A is exact (user: "simpler, or the most accurate if similar effort").
- MODEL column: cap at 24 like REQUEST FOR (user).
- TUI widths: Request-for 14→24, Note min down, Note still takes spare width (user).
- Lanes: one implementer, sequential (user).

### Deferred to Implementation

- Exact name and shape of the shared ring-row accumulator, and whether `resolve_client_reference` returns a small struct or a third tuple field. Pick what reads best once in the code.
- Whether the golden fixtures' rows carry enough fields for the `Agg tok/s` cell to show a number or `—`. Either is fine; do not add fixture data just to show a number unless the goldens would otherwise not exercise the cell at all.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

```text
for row in ring rows of scope:
  if row finished and completion_tokens > 0 and tps > 0 and duration known:
    end   = started_at + duration
    start = max(started_at, end - completion_tokens / tps)
    windows.push((start, end)); tokens += completion_tokens
sort windows by start; merge overlapping; busy = Σ merged lengths
aggregate = busy > 0 ? tokens / busy_secs : None
```

Worked check: two rows overlapping fully, 100 tokens each over the same 10 s → busy 10 s, aggregate 20 tok/s (Avg tok/s reads 10). Two rows back to back, 100 tokens over 10 s each → busy 20 s, aggregate 10 tok/s.

## Implementation Units

- [x] **Unit 1: Alias flag and note**

**Goal:** A request routed through `proxy.aliases` is marked on its row and shows `alias` in the note.

**Requirements:** R3, R7

**Dependencies:** None

**Files:**
- Modify: `src/proxy/request_log.rs` (`RequestRow.alias`, `RequestRecord::set_alias`, `note()`)
- Modify: `src/proxy/route.rs` (`resolve_reference` / `resolve_client_reference` report alias use; `decide` carries it on `ReadyAt` / `NotRunning` / `BackendUnavailable`, including the umbrella route)
- Modify: `src/proxy/router.rs` (call `set_alias` beside `set_model`; ignore the flag in the `/api/show` caller)
- Modify: `src/proxy/metrics.rs` (ignore the flag in `resolve_launch`)
- Test: `src/proxy/request_log.rs` (inline), `tests/proxy_routing.rs`
- Docs: `docs/usage.md` (`NOTE` list, row JSON example gets `"alias": false`, a sentence on the key)

**Approach:**
- Follow the decisions above. The flag travels with the routing decision; nothing re-resolves the name to find out.
- Fallback rows keep the flag: the client still asked for the alias.

**Patterns to follow:** `set_fallback` / `fallback` field and its note fragment; `tests/proxy_routing.rs` alias tests and the row assertions around `requested_model`.

**Test scenarios:**
- Happy path: note of a done row with `alias: true` → `alias`; with `alias` + `auto_start` + `fallback = launch_failed` → `alias, auto-start, fallback (launch_failed)`; in-flight alias row → `in flight, alias`.
- Integration: request with an alias name through the proxy (`proxy_state_with_aliases`) → logged row has `alias == true`, `requested_model` = alias, `model` = target's served name.
- Integration: `<alias>@<launch>` request → `alias == true`.
- Edge case: a name that is both a real model id and an alias (shadow) → `alias == false`.
- Edge case: plain model name → `alias == false`.
- Wire: a row JSON without the `alias` key deserializes with `alias == false`.

**Verification:** alias rows show `alias` in `requests` and the TUI Note column; the new key appears in `requests --json` rows.

- [x] **Unit 2: Aggregate throughput**

**Goal:** Every summary carries `tokens_per_second_aggregate` and `busy_ms`, computed over the scope's ring rows, shown as `Agg tok/s … over …`.

**Requirements:** R1, R2, R7

**Dependencies:** None (Unit 1 touches `note()`/`RequestRow`, this touches `RequestSummary` and the stamping; do it after Unit 1 to keep commits apart)

**Files:**
- Modify: `src/proxy/request_log.rs` (`RequestSummary` fields + `cells()`; shared ring-row accumulator; stamping in `tail` and `snapshot`)
- Modify: `src/proxy/metrics.rs` (text-format gauge)
- Test: `src/proxy/request_log.rs` (inline), `src/proxy/metrics.rs` (inline), `src/cli/requests.rs` (inline, summary block)
- Regenerate: `tests/golden/requests-view.txt`, `tests/golden/requests-view-wide.txt` (only the summary strip should change in this unit)
- Docs: `docs/usage.md` (summary paragraph under `llamastash requests`, `request_stats` key list, terminal summary block description, `/metrics` gauge list)

**Approach:** per Key Technical Decisions and the sketch. Stamping must happen in both `tail` and `snapshot` through one shared piece of code.

**Patterns to follow:** `ClientPair` + `stamp`, and how `snapshot` builds `model_counts` / `launch_counts` in one pass; `live_cell` for an optional cell; the `request_tokens_per_second_avg` gauge.

**Test scenarios:**
- Happy path: two rows, same 10 s window, 100 tokens each, tps 10 → aggregate 20.0, busy 10 000 ms; `Avg tok/s` still 10.0.
- Happy path: two back-to-back rows → aggregate 10.0, busy 20 000 ms.
- Edge case: partial overlap (0–10 s and 5–15 s) → busy 15 000 ms.
- Edge case: non-streamed row (`ttfb_ms == duration_ms`, tps 25, 100 tokens, duration 5 000) → window 4 s, aggregate 25.0 (no blow-up).
- Edge case: `completion_tokens / tps` longer than `duration` → window clamped to start at `started_at`.
- Edge case: in-flight rows, rows with no tokens, no speed, or zero tokens → ignored; scope with none of the above → both fields `None`, cell shows `—`.
- Scope: rows of two models and two launches → `tail(Model)` / `tail(Launch)` / `tail(All)` each count only their rows; `snapshot()` per-model / per-launch / all summaries equal the matching `tail(...)` summaries.
- Ring: more rows than the `limit` passed to `tail` → aggregate uses all ring rows in scope, not just `limit` (e.g. `tail(All, 0)` still has the figure).
- Cells: `cells()` order puts `Agg tok/s` right after `Avg tok/s`; text format `50.3 over 3m34s`.
- Metrics text: a model with qualifying rows emits the new gauge with the expected value; a model without emits none.
- Wire: a summary JSON without the two keys deserializes with both `None`.

**Verification:** `requests`, `requests --json`, `status --json` `request_stats`, `/metrics` and `/metrics?format=json` all carry the figure for the same scope with the same value.

- [x] **Unit 3: One log snapshot per `/metrics` JSON scrape**

**Goal:** `serve` reads the log once; launch resolution and the JSON body use that snapshot.

**Requirements:** R5, R7

**Dependencies:** Unit 2 (the snapshot's summaries must already carry the throughput so `LogSnapshot::tail` matches `RequestLog::tail`)

**Files:**
- Modify: `src/proxy/request_log.rs` (`LogSnapshot::tail`)
- Modify: `src/proxy/metrics.rs` (`serve`, `parse_request`, `resolve_launch`, `json_body` and its callers/tests)
- Modify: `src/proxy/metrics_backend.rs` (`serve` also calls `metrics::parse_request`; it takes one snapshot and passes it)
- Test: `src/proxy/request_log.rs` (inline), `src/proxy/metrics.rs` (inline)
- Docs: none expected (no visible change). Strike the TODO item.

**Approach:** take the snapshot before `parse_request`; pass `&LogSnapshot` the way `&[LiveLaunch]` is passed. `parse_request` takes `&LogSnapshot` instead of calling `snapshot()` itself, so `/metrics/backend` now takes one snapshot per scrape even without `?launch=`. That is one copy of at most 1000 rows, the same `/metrics` already pays per scrape; accepted. `LogSnapshot::tail` builds the same `Tail` from snapshot data (scope summary lookup + `tokens_per_second_session` for a model scope + rows newest first, limited). `json_body` only uses its `&RequestLog` for the tail, so drop that parameter.

**Patterns to follow:** `live` threading in `serve`; `RequestLog::tail` for the semantics.

**Test scenarios:**
- Parity: for one log state with rows across two models and launches (one retired), `snapshot.tail(scope, n) == log.tail(scope, n)` for `All`, each `Model`, each `Launch`, and a scope nothing has reached (default summary, no rows), with `n` = 0, 2 and larger than the ring.
- Integration: existing `json_body` / `serve` tests keep passing unchanged in output; a `?launch=<id>&format=json` scrape still resolves a retired launch from the snapshot.

**Verification:** outside test code, `src/proxy/metrics.rs` and `src/proxy/metrics_backend.rs` each call `requests.snapshot()` once per request path, and `metrics.rs` has no `RequestLog::tail` call.

- [x] **Unit 4: Column widths — TUI and CLI**

**Goal:** TUI `Request for` 24 cells and `Note` min 10; CLI padded table caps `MODEL` at 24 like `REQUEST FOR`.

**Requirements:** R4, R6, R7

**Dependencies:** Units 1–2 (so the golden fixtures are regenerated once more with all visible changes in place)

**Files:**
- Modify: `src/tui/tabs/requests.rs` (`COLUMNS` `RequestFor` width + comment, `MIN_NOTE_W`)
- Modify: `src/cli/requests.rs` (`fit_padded_table` cap loop over both columns, renamed constant, doc comments)
- Test: `src/tui/tabs/requests.rs` (inline), `src/cli/requests.rs` (inline)
- Regenerate: `tests/golden/requests-view.txt`, `tests/golden/requests-view-wide.txt`
- Docs: `docs/usage.md` (`REQUEST FOR` bullet: `MODEL` and `REQUEST FOR` both cap at 24 cells on the padded table; table example if the widths shown change)

**Approach:** constants only on the TUI side; on the CLI side one loop over the two header names.

**Patterns to follow:** existing `REQUEST FOR` cap test(s) in `src/cli/requests.rs`; column-fit tests in `src/tui/tabs/requests.rs`.

**Test scenarios:**
- CLI: a 40-char model name → `MODEL` and `REQUEST FOR` cells both end in `...` and measure 24 cells on the padded table; piped output (colors off) keeps both whole; `one_model` view (no `MODEL` column) still caps `REQUEST FOR`.
- TUI: at a width where every column fits, `Request for` renders 24 cells and a 24-char name is not cut; at a narrow width the existing drop order still holds (Route, Client, Request for); `Note` never renders under 10 cells when shown.
- Goldens regenerated and the diff reviewed: only the summary strip (Unit 2), Note text (Unit 1, if fixture rows are aliased) and column widths change.

**Verification:** `--render` of the Requests tab shows the wider `Request for`; `requests` on a TTY caps `MODEL`.

- [x] **Unit 5: Docs, CHANGELOG, TODO**

**Goal:** Everything user-visible is documented and the TODO items are struck.

**Requirements:** R7

**Dependencies:** Units 1–4 (can be folded into each unit's commit instead; either is fine as long as each commit leaves docs matching code)

**Files:**
- Modify: `docs/usage.md` (anything not already done in Units 1–4)
- Modify: `CHANGELOG.md` (`[Unreleased]` → `### Added` / `### Changed` one-liners: aggregate throughput; alias note; wider `Request for` / MODEL cap)
- Modify: `TODO.md` (strike the five R15 Batch 2 items as `- [x] ~~…~~ — done: <one line>`)
- Check: `docs/architecture.md` for any `status` / `request_stats` shape listing that names summary keys (none found during planning; confirm)

**Test expectation:** none -- docs only.

**Verification:** `rg -n 'tokens_per_second_aggregate|busy_ms|"alias"' docs/usage.md` finds the new keys; `TODO.md` R15 Batch 2 has no unchecked items.

## System-Wide Impact

- **Interaction graph:** `RequestSummary` is serialized by `requests_tail` (IPC), `status` `request_stats`, the request-log file sink (rows only), and `/metrics?format=json`. New keys flow to all of them through serde.
- **API surface parity:** `--json` is the agent contract. New keys are additive and default on read, so an older client reading a newer daemon (and the reverse) still parses.
- **Unchanged invariants:** `Avg tok/s` / `Tok/s session` / `tokens_per_second_last` values; `Totals` since-start counting; piped (`\t`) output of `requests`; `/metrics` text output apart from the one new gauge; routing behavior (the alias flag is recorded, never consulted).

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| `tail` and `snapshot` compute throughput differently | One shared accumulator; parity test in Unit 3 compares the two |
| Golden fixtures regenerated over an unintended change | Review the golden diff in each unit; only the listed areas may change |
| Cold build in a fresh worktree is slow | Expected; `target/` sits in the worktree on `/mnt/work` |

## Verification (commands)

Setup: none beyond the toolchain; the first build in the worktree is cold.

While coding (narrowest first):
- `cargo check --features test-fixtures`
- `cargo nextest run --features test-fixtures --lib request_log`
- `cargo nextest run --features test-fixtures --lib metrics`
- `cargo nextest run --features test-fixtures --lib requests` (CLI + TUI requests unit tests)
- `cargo nextest run --features test-fixtures --test proxy_routing`
- Goldens: `UPDATE_GOLDEN=1 cargo nextest run --features test-fixtures --test tui_e2e_render_test requests_view`, then the same without `UPDATE_GOLDEN`, then `git diff tests/golden/` and check the diff is what the unit expects.

Before the last commit (once):
- `cargo nextest run --features test-fixtures`
- `cargo test --doc --features test-fixtures`
- `make lint` and `make doc` run in the pre-commit hook; read their output only on failure.

E2E (isolated; never the user's daemon, never load a real model):
- In one shell invocation: export `LLAMASTASH_STATE_DIR`, `LLAMASTASH_CONFIG_DIR`, `LLAMASTASH_CACHE_DIR` to fresh temp dirs; `cargo build --bin llamastash`; `target/debug/llamastash daemon start --proxy-port 0`; read the proxy address from `target/debug/llamastash status --json | jq -r .proxy.listen`; send one `POST /v1/chat/completions` for a model that does not exist; `target/debug/llamastash requests --json | jq '.summary, .requests[0]'` shows `tokens_per_second_aggregate`, `busy_ms` and the row's `alias` key; `curl -s "<proxy>/metrics?format=json" | jq .` parses; `target/debug/llamastash daemon stop`. Env does not persist between shell calls, so re-export in every call that talks to this daemon.
- `cargo run -- --render --render-size 160x45` to eyeball the TUI after the golden updates.

## Definition of Done

- All five R15 Batch 2 items implemented as above, each unit committed separately on `feat/r15-batch2` with conventional-commit messages scoped `proxy` / `tui` / `cli` / `docs`.
- Unit tests and integration tests listed above exist and pass; full nextest suite and doctests pass; pre-commit hook passes.
- `docs/usage.md`, `CHANGELOG.md`, `TODO.md` updated; plan checkboxes ticked.
- E2E run done and its output summarized in the result.

## Sources & References

- `TODO.md` § R15 → Batch 2 (measurements and shape A/C descriptions)
- Related plan: `docs/plans/2026-10-05-001-feat-r14-batches-plan.md`
- Code: `src/proxy/request_log.rs`, `src/proxy/metrics.rs`, `src/proxy/route.rs`, `src/proxy/router.rs`, `src/proxy/forward.rs`, `src/tui/tabs/requests.rs`, `src/cli/requests.rs`
