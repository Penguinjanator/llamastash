---
title: "feat: proxy metrics surface (/metrics, /metrics/backend, launch-scoped request stats)"
type: feat
status: planned
date: 2026-10-11
origin: TODO.md batch 3 item "Standard metrics endpoint on the proxy" (three sub-items reconciled)
---

# feat: proxy metrics surface

**Status:** planned. Two implementation units in `src/proxy/` plus engine knobs.

## Overview

One observability surface on the existing proxy listener:

1. `GET /metrics` — the proxy's own numbers, from the request log. Prometheus text
   by default, JSON on request.
2. `GET /metrics/backend` — the running engines' own `/metrics`, fetched and merged,
   with each series labelled with the launch it came from.
3. Launch-scoped request stats: `requests --json`, `requests_tail`, and `/metrics`
   can be scoped to one launch, not just one model.

No new listener, no new port, no new config keys.

## Research (what the engines and neighbours actually expose)

Verified 2026-10-11:

| Engine | `/metrics` | Flag needed | Prefix | Notes |
| --- | --- | --- | --- | --- |
| llama.cpp (`tools/server/README.md`, master) | yes | `--metrics` (off by default; the route answers **501** without it) | `llamacpp:` | 15 series: `prompt_tokens_total`, `prompt_seconds_total`, `prompt_tokens_seconds`, `tokens_predicted_total`, `tokens_predicted_seconds_total`, `predicted_tokens_seconds`, `requests_processing`, `requests_deferred`, `n_tokens_max`, `n_decode_total`, `n_busy_slots_per_decode`, `spec_decode_*` |
| Halogen | yes, same names | same | `llamacpp:` | 0.16.2 serves the `llamacpp:*` names |
| vLLM (docs, V1) | yes | none | `vllm:` | gauges + counters + histograms (`time_to_first_token_seconds`, `e2e_request_latency_seconds`); every series carries a `model_name` label |
| SGLang (docs) | yes | `--enable-metrics` | `sglang:` | counters + histograms |
| Ollama | **no** | — | — | no `/metrics` on `main`: GitHub code search for the literal `"/metrics"` returns 0 hits (control string returns 5); open issues #3144, #17694 |
| LM Studio | **no first-party endpoint** (lower confidence, not a llamastash backend) | — | — | third-party proxies exist to fill the gap |

Two conclusions that shape the design:

- Every engine we sit in front of uses `<namespace>:<name>` and Prometheus text.
  The proxy follows the same convention instead of inventing one.
- Engines that need a flag to serve metrics answer 501 without it. So a scrape of a
  backend that has metrics switched off is an ordinary, expected outcome, not an error.

## Decisions

| # | Decision | Why |
| --- | --- | --- |
| D1 | Our metric names are `llamastash:*`. | Matches `llamacpp:` / `vllm:` / `sglang:` behind us. `:` is legal in a Prometheus name (`[a-zA-Z_:][a-zA-Z0-9_:]*`), reserved by convention for recording rules only. |
| D2 | No OpenTelemetry `gen_ai.*` aliases. | The OTel GenAI names (`gen_ai.client.token.usage`, `gen_ai.client.operation.duration`) are OTLP instrument names; a Prometheus text scrape sanitises them into `gen_ai_client_token_usage`, so an alias buys confusion, not compatibility. The mapping is documented instead of emitted. |
| D3 | Counters + average gauges. **No histograms.** | Decided by the requester. Averages come from the sums the request log already keeps, so nothing new is stored. Trade-off accepted: no `histogram_quantile()` on our own surface; per-request distributions stay in the JSON row view. |
| D4 | `/metrics` negotiates on `Accept` and on `?format=` (see the Accept contract). | One path for both audiences, and a Prometheus scrape must never accidentally get JSON. |
| D5 | Backend metrics are enabled by **knobs only**; default launched argv is unchanged. | Passing `--metrics` / `--enable-metrics` by default changes launched argv, and the overhead benchmarks compare our argv against raw `llama-server`. A knob is the existing opt-in mechanism, it reaches the CLI, the TUI editor and presets for free. |
| D6 | Prometheus labels stay bounded: `model`, `route`. `launch` only on the explicit `?launch=1` scrape. | Client address is unbounded (ephemeral ports) and is never a label. Launch ids are never reused and a long-lived daemon starts many, so per-launch series grow the TSDB forever; they are opt-in and documented. |
| D7 | Per-launch totals for the last 16 stopped launches are kept instead of dropped. | Today `forget_launch` deletes a stopped launch's totals, so a per-launch counter would vanish mid-scrape and read as a reset. Retaining 16 keeps them queryable and is a few hundred bytes. |
| D8 | `/metrics` never contacts a backend; `/metrics/backend` always re-scrapes. | A scrape must not fan out to N engines unless it asked to. Nothing is cached: stale metrics are worse than a slow scrape. |
| D9 | Nothing is persisted. All values are since daemon start. | The request log is in memory by design and dies with the daemon; `requests.jsonl` stays the durable record. Documented so nobody reads a counter reset as data loss. |

## The Accept contract (this is what agents read — keep it exact)

`GET /metrics` answers in one of two formats. The rule is deterministic and has to
survive a naive `curl` and a real Prometheus scraper alike.

| Request | Response |
| --- | --- |
| no `Accept` header | Prometheus text |
| `Accept: */*` (what plain `curl` sends) | **Prometheus text** — a wildcard must never select JSON |
| `Accept: text/plain` or `text/plain; version=0.0.4; charset=utf-8` | Prometheus text |
| `Accept: application/openmetrics-text; version=1.0.0,text/plain` | Prometheus text (OpenMetrics is served as text format 0.0.4) |
| `Accept: application/json` | JSON |
| `Accept: application/json;q=0.9, text/plain;q=0.5` | JSON (higher q wins) |
| `Accept: application/json;q=0.9, text/plain;q=0.9` | Prometheus text (tie goes to text) |
| `Accept: application/xml` | `406` with the OpenAI-shaped error envelope |
| `?format=json` | JSON, regardless of `Accept` |
| `?format=prometheus` | Prometheus text, regardless of `Accept` |
| `?format=<anything else>` | `400` naming the two accepted values |

Rules:

- Only two candidates are matched: `application/json` and `text/plain`. A `;`
  parameter that is not `q` (e.g. `version=0.0.4`) does not affect the match.
- Missing `q` is 1.0. `*/*` matches either candidate at its own q but loses a tie
  against an explicit type.
- `?format=` beats `Accept`. `Accept` beats the default (text).
- Content types: `text/plain; version=0.0.4; charset=utf-8` and `application/json`.
- `HEAD /metrics` is not routed (404), same as `/health`.
- Both routes require the proxy API key when one is configured, with the existing
  `Bearer` challenge (not the `/ui` `Basic` one). `/metrics` is **not** added to
  `auth_exempt` in `src/proxy/router.rs`, so this is automatic.

The same table goes into `docs/usage.md` verbatim. It is the HTTP equivalent of the
`--json` agent contract, so say so there.

## `/metrics` series

Monotonic since daemon start (counters) or instantaneous (gauges). One label set per
metric name — never two label sets under one name.

Counters:

| Name | Labels | Meaning |
| --- | --- | --- |
| `llamastash:requests_total` | `model` | finished requests (a row counts when it finishes, not when it starts) |
| `llamastash:route_requests_total` | `route` | same, keyed by request path instead of model |
| `llamastash:request_errors_total` | `model` | rows `RequestRow::is_error()` |
| `llamastash:prompt_tokens_total` | `model` | |
| `llamastash:completion_tokens_total` | `model` | |
| `llamastash:request_duration_seconds_total` | `model` | sum of finished durations; mean = `rate(…_total) / rate(llamastash:requests_total)` |
| `llamastash:request_ttft_seconds_total` | `model` | sum of time-to-first-byte |
| `llamastash:auto_starts_total` | `model` | requests that spawned the launch |
| `llamastash:evictions_total` | `model` | launches unloaded to make room |

Gauges: `llamastash:requests_in_flight{model}`, `llamastash:request_duration_seconds_avg{model}`,
`llamastash:request_ttft_seconds_avg{model}`, `llamastash:request_tokens_per_second{model}`
(last reported), `llamastash:request_tokens_per_second_avg{model}`, `llamastash:models_loaded`,
`llamastash:models_discovered`, `llamastash:request_log_rows`, `llamastash:request_log_capacity`,
`llamastash:request_log_file_dropped_total`.

Only with `?launch=1` (documented cardinality warning): `llamastash:launch_info{launch,model,port}`
(gauge 1, live launches only), `llamastash:launch_requests_total{launch,model}`,
`llamastash:launch_request_errors_total{launch,model}`,
`llamastash:launch_prompt_tokens_total{launch,model}`,
`llamastash:launch_completion_tokens_total{launch,model}`,
`llamastash:launch_requests_in_flight{launch,model}`.

Rendering rules: `# HELP` and `# TYPE` once per name, then the samples; metric names
sorted, samples within a name sorted by label values (deterministic output for tests
and diffs); label values escaped (`\` `"` newline); values finite, integers printed
without a decimal point; a metric with no samples is omitted entirely (no empty
`# TYPE` block).

Omit nothing that has samples, never emit `NaN`, and never hold the request-log mutex
while building the body: take a snapshot into owned data first.

## `/metrics` JSON shape

```json
{
  "generated_at_ms": 1760123456789,
  "scope": { "model": "gemma-4…", "launch": null },
  "summary": { …one RequestSummary, keys identical to requests --json… },
  "models": [ { "model": "gemma-4…", "path": "/models/…", "launches": 1, …RequestSummary } ],
  "launches": [ { "launch": "L3", "model": "gemma-4…", "port": 45001, "live": true, …RequestSummary } ],
  "requests": [ …RequestRow, exactly the array requests --json returns… ],
  "backends": [ …only from /metrics/backend, never here… ]
}
```

Because `summary` and `requests` are the objects `llamastash requests --json`
already returns, a client that parses one parses the other.

## `/metrics/backend`

- Targets: every running, Ready launch (same enumeration `src/proxy/ui.rs` uses for
  its chooser: `launch_id`, `port`, display name).
- For each target, in parallel: `GET http://127.0.0.1:<port>/metrics`, 2 s timeout,
  pooled `state.http_client`, body capped at 4 MiB (over-cap = keep the head, mark
  `truncated`). One target failing never fails the response.
- Merge into one text document: `# HELP` / `# TYPE` kept once per metric name (first
  wins); every sample line gets `launch="L3"` and `model="<name>"` appended to its
  label set (or a new label set when it had none); engine labels such as vLLM's
  `model_name` are left alone; unparsable lines are skipped and counted.
- Then our own gauges for that scrape, labelled `model,launch`:
  `llamastash:backend_metrics_available` (1/0 — a 501 from llama.cpp without
  `--metrics` lands here), `llamastash:backend_metrics_series`,
  `llamastash:backend_metrics_scrape_duration_seconds`,
  `llamastash:backend_metrics_skipped_total`.
- `?model=<ref>` restricts the fan-out to one model.
- JSON form (`Accept: application/json` or `?format=json`) returns
  `{"backends":[{"launch","model","url","status","ok","series","duration_ms","truncated","error"}]}`
  with no metric text in it.
- Constants are module-level `const`s (`SCRAPE_TIMEOUT`, `MAX_TARGET_BYTES`,
  `RETIRED_LAUNCH_HISTORY = 16`), not config keys.

## Launch scoping (why one model's numbers merge today)

Today every row carries `launch_id`, and `status --json` shows a per-launch
`request_stats` block (`RequestLog::launch_summary`), but `requests_tail` only takes
`model_path`, so `llamastash requests <ref>` merges every launch of that model —
naming a launch resolves to its model. Fix that as part of this work:

- `src/proxy/request_log.rs`: replace the `tail(model_path, limit)` scope with a
  `LogScope` enum — `All`, `Model { path }`, `Launch { id }` — where a launch scope
  takes its summary from `by_launch` and filters rows on `row.launch_id`.
- `Totals` keeps a display label (the row's `model`), so per-model series and per
  launch series print the same name.
- `forget_launch` moves the totals to a bounded retired list (D7) instead of
  deleting; `launch_summary` reads live then retired.
- `requests_tail` IPC gains an optional `launch_id` param (`src/ipc/methods.rs`),
  and the wire doc in `docs/architecture.md` § IPC surface updates with it.
- `llamastash requests <ref>`: when the reference names a *launch* (a bare `L3` or a
  port) the scope is that launch; a model reference keeps the model scope. Update the
  `RequestsArgs` doc comment in `src/cli/cli_args.rs` and `docs/usage.md`.
- `/metrics` and `/metrics/backend` take `?model=` and `?launch=`. A reference that
  matches nothing is a `404` naming the value. Matching order: launch id (`L3`),
  port, exact catalog path, then case-insensitive display-name substring.

## Engine knobs (D5)

- `src/backend/llama_cpp/knobs.rs`: `KnobDef { id: "metrics", flag: None, kind:
  KnobKind::Bool, emit: Emit::BareFlagWhenTrue, group: Group::Advanced, label:
  "Metrics", help: "serve Prometheus metrics at /metrics", ring: Ring::None,
  fallback: LayerLabel::ServerDefault, volatile: false, auto: None, aliases: &[] }`.
  `BareFlagWhenTrue` emits `--metrics` when true and nothing when false (the shape
  llama.cpp wants; there is no `--no-metrics`), so nothing in `compose.rs` changes.
  Place it in the declaration order that keeps `Advanced` grouped as the file already
  does.
- `src/backend/sglang/knobs.rs`: same shape with `id: "enable-metrics"`, help
  `"serve Prometheus metrics at /metrics"`. Copy `enable-unified-memory`, which is
  already `Bool` + `BareFlagWhenTrue`.
- vLLM: no knob; V1 serves `/metrics` on its own. Do not add a vLLM knob.
- Lemonade and the generic backend: untouched. A generic entry's argv is the user's.
- Verify against real binaries where one is reachable and state the version:
  `llama-server --help | grep -i metric`, `python -m sglang.launch_server --help` or
  `sglang --help` if installed. If no binary is reachable, say so in the result
  instead of asserting the flag from this file.
- Adding a knob can move the TUI knob-editor render; if a golden snapshot changes,
  regenerate it the way `make build` / the `snapshot` target does and commit the
  new golden.

## Units

| Unit | Files | Depends on | Lane |
| --- | --- | --- | --- |
| U1 launch scoping + retired totals in the log | `src/proxy/request_log.rs`, `src/ipc/methods.rs`, `src/cli/requests.rs`, `src/cli/cli_args.rs` + inline tests | — | impl-1 |
| U2 engine knobs | `src/backend/llama_cpp/knobs.rs`, `src/backend/sglang/knobs.rs` + their tests | — | impl-1 |
| U3 `/metrics` | new `src/proxy/metrics.rs`, `src/proxy/mod.rs`, `src/proxy/router.rs` | U1 | impl-2 |
| U4 `/metrics/backend` | new `src/proxy/metrics_backend.rs`, `src/proxy/mod.rs`, `src/proxy/router.rs`, `tests/fixtures/fake_llama_server.rs` | U1, U3 | impl-2 |
| U5 docs + changelog + TODO strikes | `docs/usage.md`, `docs/architecture.md`, `CHANGELOG.md`, `TODO.md`, this plan's checkboxes | all | split: the `requests`/knob lines with impl-1, the metrics lines with impl-2 |

| U5 docs + changelog + TODO strikes | `docs/usage.md`, `docs/architecture.md`, `CHANGELOG.md`, `TODO.md`, this plan's checkboxes | all | split: the `requests`/knob lines with impl-1, the metrics lines with impl-2 |
| U6 clients and parallel sessions in the request summary | `src/proxy/request_log.rs`, `src/proxy/metrics.rs`, `src/cli/requests.rs`, `src/tui/tabs/requests.rs`, `docs/usage.md`, `CHANGELOG.md` + their tests | U1, U3 | impl-3 |

U1 and U2 touch no common file. U3 and U4 share `router.rs` and `mod.rs`, and both
need U1's log API, so impl-2 runs after impl-1 lands. U6 shares `metrics.rs` and
`request_log.rs` with U3/U4, so it runs after them.

## Clients, connections and live traffic (U6, added after round 1)

An agent runner looking at a launch needs to know whether anything is running right now
and how many things are behind it. Four figures, all derived from the rows the scope
retains (the last 1000), so the numbers always match the table on screen and nothing grows
with daemon age:

- **hosts** — distinct client IPs the scope served. On a loopback-only setup every local
  tool is `127.0.0.1` (or `::1`), so this reads 1 until you bind a LAN; it counts machines.
- **connections** — distinct client peers on `ip:port`, which is what a tool's live
  connection looks like to the proxy. This is the figure that counts tools locally.
- **live hosts** and **live connections** — the same two counts over the requests in flight
  right now.

Display rule, shared by the TUI strip and the CLI summary block through
`RequestSummary::cells`:

- drop the live pair entirely when both are zero, so an idle launch's strip is unchanged;
- when live hosts equals live connections, show one field, `Live N`;
- when they differ, show `Live N/M` (hosts/connections);
- `Hosts` and `Connections` always show.

`requests --json` gains `hosts`, `connections`, `live_hosts` and `live_connections` in
`summary`. The four are `Option`s: absent means an older daemon never computed them, which
is not the same as zero. `/metrics` gains `llamastash:hosts_in_flight` and keeps
`requests_in_flight` as the connections figure, with the same label rules including
`model="none"` and the `?launch=1` opt-in.

`Tok/s last` is dropped from the strip and from the CLI summary block; `tokens_per_second_last`
stays in the JSON and in the rows.

Scoping rule, same in all three surfaces (TUI strip, CLI, `/metrics`): a **launch** scope is
strict and a **model** scope is the whole-model aggregate.

- A launch scope means only that launch's rows and only that launch's counters. A running
  model row in the TUI is one launch, so the Requests tab takes that launch's id, and never
  another launch's numbers. `requests L3` already does this through `LogScope::Launch`, and
  `?launch=<ref>` on the metrics route must do the same: no model-filed totals merged into a
  launch summary, and `status --json` per-launch `request_stats` stops folding in requests
  filed under the model.
- A model scope (`requests <model>`, the model row in the TUI, `?model=<ref>`) keeps
  everything filed under that model since daemon start, including rows of launches that have
  since stopped and the retained totals for the last 16 retired launches. A dashboard scrape
  and `llamastash requests <model>` agree line for line.
- Where requests are filed under the model with no distinct launch id (a managed
  multiplexer), a launch view is the model aggregate by necessity, and `docs/usage.md` says
  so.

## Verification

From the worktree root. A fresh worktree has no `target/`, so the first cargo run
compiles dependencies; let it finish.

```bash
cargo nextest run --features test-fixtures --lib request_log
cargo nextest run --features test-fixtures --lib metrics
cargo nextest run --features test-fixtures --test knob_parity_test
cargo nextest run --features test-fixtures --test proxy_metrics     # new file, U4
make lint
make test                                                          # once, at the end
```

E2E with an isolated daemon. **Never touch the default state dir** — a second daemon
on the user's own state dir kills his models. Every shell call needs the exports
again:

```bash
cd "$WORK"
export LLAMASTASH_STATE_DIR="$PWD/.e2e/state" LLAMASTASH_CONFIG_DIR="$PWD/.e2e/config" \
       LLAMASTASH_CACHE_DIR="$PWD/.e2e/cache"
mkdir -p "$LLAMASTASH_STATE_DIR" "$LLAMASTASH_CONFIG_DIR" "$LLAMASTASH_CACHE_DIR"
cp ~/.config/llamastash/config.yaml "$LLAMASTASH_CONFIG_DIR/"   # real catalog, isolated state
cargo build --bin llamastash
./target/debug/llamastash --proxy-port 11900 daemon start
./target/debug/llamastash status --json | jq '.proxy.status'
curl -s        localhost:11900/metrics | head -20
curl -s -H 'Accept: application/json' localhost:11900/metrics | jq '{summary, n: (.requests|length)}'
curl -s 'localhost:11900/metrics?format=json&limit=5' | jq '.requests|length'
curl -s        localhost:11900/metrics/backend | head -20
./target/debug/llamastash daemon stop
```

Then, only if the host has room (`free -h` shows the model fitting with slack, and
nothing else is on the GPU): start one small GGUF with `metrics: true`, send one
completion through the proxy, and confirm `llamastash:requests_total` moved,
`llamastash:requests_in_flight` returned to 0, and `llamastash:backend_metrics_available`
is 1 with `llamacpp:` series labelled `launch="L1"`. Skipping this is acceptable if
the host is busy — say so in the result rather than swapping models in.

## Definition of done

- `/metrics` serves the series above in text format and the JSON shape above, per
  the Accept table, key-auth on, nothing new bound to a port.
- `/metrics/backend` merges the live launches' metrics with a `launch` label and
  reports per-target status without letting one target break the response.
- `llamastash requests`, `requests_tail` and `/metrics` can be scoped to one launch,
  and a stopped launch's totals survive for the last 16.
- llama.cpp and SGLang have an opt-in metrics knob; default argv is byte-identical to
  before (`cargo nextest run --features test-fixtures --test knob_parity_test` and the
  argv-diff test stay green).
- `docs/usage.md` carries the Accept table, the series list, the curl examples, and
  the `requests` launch scope; `docs/architecture.md` carries the two new routes and
  the since-daemon-start caveat; `CHANGELOG.md` has one short line; the TODO.md item
  and its three sub-items are struck.
- `make test` and the E2E block are green, and their output is quoted in the result.
