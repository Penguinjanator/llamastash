---
title: "feat: map Anthropic effort fields for llama.cpp on /v1/messages"
type: feat
status: completed
date: 2026-09-30
origin: R12 batch 2 in TODO.md; live tests 2026-09-30
---

# feat: map Anthropic effort fields for llama.cpp on `/v1/messages`

**Status:** completed 2026-10-01. Mini plan.

## Overview

Make Claude Code's effort control work on a llama.cpp model. The proxy's
llama.cpp backend rewrites one field in a `/v1/messages` body before
forwarding: `output_config.effort` becomes
`chat_template_kwargs.reasoning_effort`. Every other byte stays the same. Drop
the hook when llama.cpp handles the field itself.

## Problem

Claude Code sends `thinking: {type: "adaptive"}` and `output_config.effort` for
a model id it does not recognize
([gateway docs](https://code.claude.com/docs/en/llm-gateway-protocol#how-the-connection-method-changes-client-behavior)).
llama.cpp ignores both, so `/effort` does nothing on a local model.

Measured 2026-09-30, llama.cpp master `f872b5911` (build 11310), Qwen3.8-27B
Q6_K, temp 0, same prompt, output tokens on `/v1/messages`:

| Field sent | Output tokens |
|---|---|
| nothing (server `--reasoning-effort medium`) | 46 |
| `output_config.effort: xhigh` | 46 (ignored) |
| `thinking.type: adaptive` | 46 (ignored) |
| top-level `reasoning_effort: xhigh` | 46 (ignored) |
| `chat_template_kwargs.reasoning_effort: xhigh` | 106 |

Upstream: [llama.cpp #20479](https://github.com/ggml-org/llama.cpp/pull/20479)
maps the OAI, OpenRouter and Claude effort fields. Open since 2026-03-13,
review required, no activity since 2026-06-26. Its OAI half landed separately
in `7e4c0a96` (per-request `reasoning_effort` on `/v1/chat/completions`).

Other engines, same test:

- halogen `sha256:414872ef` honors `output_config.effort` itself (63 vs 112
  tokens). Needs nothing.
- gufo `fd1710b` answers `400 request field '<x>' is not supported on this
  endpoint` for `thinking`, `output_config`, `reasoning_effort` and
  `chat_template_kwargs`. Claude Code always sends `thinking`, so it likely
  fails outright on gufo. Not tested with real Claude Code.

## Scope

In: llama.cpp backend, `/v1/messages` only, `output_config.effort`.

Out: a proxy-side clamp for unsupported effort values (dropped 2026-09-30; the
template error names the valid values), request params in presets (dropped
2026-09-30), any gufo workaround.

## Design

- A backend trait hook, default no-op, that may rewrite a request body for a
  given endpoint. Only `src/backend/llamacpp/` implements it (backend no-leak
  rule).
- The proxy calls it in `forward.rs` next to `with_model`, using the same
  in-place `RawValue` rewrite so untouched fields stay byte-identical.
- Mapping: `output_config.effort` → `chat_template_kwargs.reasoning_effort`.
  A client-set `chat_template_kwargs.reasoning_effort` wins. Leave
  `output_config` in the body (llama.cpp ignores it). Added in review:
  `count_tokens` maps too (same body, so its count has to match the request
  that follows), and `backend.llamacpp.map_anthropic_effort: false` turns the
  whole mapping off, because a per-request kwarg otherwise overrides an effort
  the launch itself was given.
- `thinking.type: disabled`: check what llama.cpp does with it today before
  mapping it to `reasoning_effort: "none"`.
- Effort values the template does not accept (e.g. `max` on Qwen3.8) pass
  through and get the template's own error, which names the valid values.

## Units

- [x] U1. Trait hook with a no-op default; call site in `forward.rs`; test that
  a body with no effort field is forwarded byte-identical.
- [x] U2. llama.cpp implementation of the mapping, including the "client kwarg
  wins" rule; tests on the rewritten bytes.
- [x] U3. Check `thinking.type: disabled` on current llama.cpp; map it or
  document why not. **Not mapped**, rationale below.
- [ ] U4. File the gufo `/v1/messages` 400 upstream (with the four field names
  and the Claude Code docs link). **Drafted 2026-10-01, not filed** — text in
  the appendix, tracked in `TODO.md`; filing waits for a go-ahead.
- [x] U5. Docs: `docs/architecture.md` (proxy body rewrites), `docs/usage.md`
  (Claude Code effort), `CHANGELOG.md`.

### U3: why `thinking.type: disabled` is not mapped

llama.cpp's own `/v1/messages` translation (`tools/server/server-chat.cpp:599`,
`f872b5911`) reads `thinking` already: `type: enabled` becomes a token budget,
`adaptive` and `disabled` are ignored. Turning `disabled` off here would mean
emitting `chat_template_kwargs.enable_thinking: false` (the only per-request
disable the engine honors, `server-common.cpp:1363`), which forks the engine's
thinking rules into the proxy and can contradict the effort field on the same
request. It is also not what Claude Code sends on this path: a model id it does
not recognize gets `{"type":"adaptive","display":"omitted"}` (captured from
2.1.286 below), never `disabled`. Revisit if a client starts sending it.

## Verification

Done 2026-10-01.

- `cargo test --features test-fixtures --no-fail-fast` green (47 binaries),
  `cargo clippy --all-targets --features test-fixtures -- -D warnings` clean.
- Live, isolated daemon (`LLAMASTASH_STATE_DIR` + `--proxy-port 11535`),
  llama.cpp `f872b5911` (build 11310), Qwen3.8-27B-UD-Q6_K on the llamacpp
  backend (`--ctx 16384 --mtp off`), temp 0, one prompt, `max_tokens` 600.
  Re-run with [`scripts/effort-live-check.sh`](../../scripts/effort-live-check.sh)
  (`<proxy-origin> <model> [direct-upstream-origin]`). `map_anthropic_effort`
  left on, which is the default:

| Sent | HTTP | Output tokens | Thinking chars |
|---|---|---|---|
| nothing | 200 | 600 | 1633 |
| `output_config.effort: low` | 200 | 521 | 1174 |
| `output_config.effort: xhigh` | 200 | 600 | 1502 |
| `output_config.effort: max` | 500 | — | — |
| `output_config.effort: xhigh` + `chat_template_kwargs.reasoning_effort: low` | 200 | 521 | 1174 |
| direct `chat_template_kwargs.reasoning_effort: low` | 200 | 521 | 1174 |
| direct `chat_template_kwargs.reasoning_effort: xhigh` | 200 | 600 | 1502 |

  The mapped rows match the direct `chat_template_kwargs` rows exactly, so the
  rewrite is indistinguishable from sending the kwarg. `max` comes back as
  llama.cpp's own 500 carrying the template's exception text
  (`Unexpected reasoning effort max. Supported types are xhigh (default),
  medium, and low.`), which is the pass-through behavior the scope row asked
  for. `/v1/messages/count_tokens` answers 200 with the field present.
- Same daemon with `map_anthropic_effort: false`: the `low`, `xhigh` and `max`
  rows all return the no-effort numbers (600 / 1633), so the launch and engine
  defaults stand, while a client that sends the kwarg directly still gets its
  own value (521 / 1174).
- The check needs a model whose template defines `reasoning_effort`. On
  Qwen3.5-4B (no such branch in its template) every row answers 600 / 1640
  regardless, which exercises the plumbing and nothing else.
- Cheapest model that does have the branch: `LFM2.5-2.6B-Q3.8-TBrilliance-NEO-IQ4_XS.gguf`
  from `DavidAU/LFM2.5-2.6B-Qwen3.8-Turbo-...-GGUF` (1.5 GB, ready in seconds,
  unlike the 27B's ~3 min). Its template names its efforts `low` /
  `medium-low` / `medium` / `high` / `ultra` and raises on `xhigh`, so the
  script takes the row values from `EFFORT_LOW` / `EFFORT_HIGH` / `EFFORT_BAD`.
  With `EFFORT_HIGH=ultra EFFORT_BAD=nonsense`: nothing 501 / 885, `low`
  416 / 877, `ultra` 598 / 1136, `low` + client kwarg `low` 416 / 877,
  `nonsense` and `xhigh` a 500 with the template's raise text, and the direct
  kwarg rows land on 416 / 877 and 600 / 1356.
- What the real client sends, captured from Claude Code 2.1.286 against a
  logging listener on `/v1/messages?beta=true` for an unrecognized model id:
  `thinking: {"type":"adaptive","display":"omitted"}` plus
  `output_config: {"effort":"xhigh"}`, no `chat_template_kwargs`. The proxy
  reaches the same token counts with that exact shape, query string included.
- A full `claude` session against the proxy stops earlier, on the known
  `System message must be at the beginning` template error already documented
  in `docs/usage.md` for Qwen GGUFs plus tool calling. Unrelated to this change
  and unchanged by it.

## Open questions

- Remove the hook when #20479 merges, or keep it for older llama.cpp builds a
  user may still run? No backwards-compat before the first release suggests
  remove. **Decided: remove.** No pre-1.0 compat shim, and
  `scripts/effort-live-check.sh` says when the mapping is redundant (the
  `proxy-effort-*` rows move without it).

## Review round (2026-10-01)

Nine findings on the first cut, all addressed.

1. No way to turn the mapping off, and it silently overrode a launch-time
   effort. Added `backend.llamacpp.map_anthropic_effort` (default `true`), read
   by the llama.cpp impl from `MethodContext`; the override is now documented.
2. A rejected effort fails the request instead of being dropped. Not a bug, but
   undocumented: `max` is rejected by Qwen3.8's template, and a client that
   always sends an effort now gets an error where it used to get an answer.
   Written into `usage.md` with the template error text and llama.cpp's own
   status for it.
3. `count_tokens` was out of scope yet silently gained the mapping, and U4 was
   ticked although nothing was filed. `count_tokens` is now in scope, stated in
   the hook doc and tested; U4 is unticked and both the gufo issue and the
   upstream watch are in `TODO.md`.
4. `effort-live-check.sh` died before its own usage check when
   `~/.config/llamastash/config.yaml` had no `api_key` line (a failing command
   substitution under `set -e`), and its defaults hardcoded this box's proxy
   port and model. Args are required now and the key lookup cannot abort it.
5. No test proved the rewrite reaches upstream, so a mis-recorded
   `resolved_backend` passed everything. The fake upstream now echoes the
   request bytes and `tests/proxy_routing.rs` asserts both the mapped and the
   untouched case.
6. The `forward.rs` test asserted llama.cpp's byte-level output across three
   modules. Slimmed to plumbing (byte-identical forward for every backend,
   request-model pin), with the mapping asserted in `effort.rs` and in the
   integration test.
7. Duplicate JSON keys were mishandled: a second `chat_template_kwargs` loses
   in llama.cpp's parser, and the same shape applied to `output_config`. Both
   bail when a key repeats, and a non-string `effort` bails instead of failing
   to compile.
8. Four doc and PR-body claims were wrong: gufo described as having no effort
   concept (it 400s on the fields), the template said to ignore a non-string
   `reasoning_effort` (it raises), `xhigh` attributed to a client default (it
   is this box's `effortLevel` setting), and the drop-on-rejected-value
   behaviour left unstated.
9. Five hand-rolled `Backends::all().find(|b| b.id() == ...)` lookups were left
   behind (three in `launch_service.rs`, one in `supervisor.rs`, one in
   `orphans.rs`) and the proxy re-resolved the backend per request.
   `ManagedModel` now stores the resolved `Backends` next to `resolved_backend`
   (launch and the readiness probe use it) and the proxy takes it from the model
   it already found. The double-parse worry did not hold, because
   `REQUEST_MODEL_KEY` is only ever set by a config-declared server, which
   rewrites nothing; the two rewrites now sit in one helper with that note.


## Appendix: gufo upstream issue (drafted 2026-10-01, not filed)

Target: [gufo-org/gufo](https://github.com/gufo-org/gufo). Facts measured at
gufo `fd1710b` (2026-09-30) on this box, not re-measured since.

**Title:** `/v1/messages` 400s on the Anthropic fields Claude Code sends

**Body:**

> Running gufo behind LlamaStash's generic backend, `POST /v1/messages` returns
> `400 request field '<x>' is not supported on this endpoint` for each of:
>
> - `thinking`
> - `output_config`
> - `reasoning_effort`
> - `chat_template_kwargs`
>
> `thinking` is the blocking one. Claude Code sends it on every inference
> request, and for a model id it does not recognize it sends
> `{"type": "adaptive", "display": "omitted"}`, with effort in
> `output_config.effort`
> ([gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol)).
> The result is that no Claude Code request can land on gufo, and a client
> cannot work around it: the fields arrive whether or not the feature is used.
>
> llama.cpp and Halogen both ignore body fields they do not implement, so the
> same request reaches them and answers.
>
> What we would like, either is enough:
>
> 1. Ignore unknown body fields on the OpenAI and Anthropic endpoints. This is
>    the option that survives the next client release, since clients like Claude
>    Code add capabilities as new body fields paired with a beta header.
> 2. Keep the strict check but accept `thinking`
>    (`{"type": "enabled" | "adaptive" | "disabled", "budget_tokens"?: int}`)
>    and `output_config` (`{"effort"?: "low" | "medium" | "xhigh", ...}`), and
>    document which `/v1/messages` fields are read.
>
> Repro (a real Claude Code 2.1.286 body, trimmed):
>
> ```bash
> curl -sS -X POST http://127.0.0.1:<port>/v1/messages \
>   -H 'content-type: application/json' \
>   -d '{"model":"<served-model-name>","max_tokens":256,"stream":false,
>        "thinking":{"type":"adaptive","display":"omitted"},
>        "output_config":{"effort":"xhigh"},
>        "messages":[{"role":"user","content":"hi"}]}'
> ```

Filing it needs a go-ahead; nothing was posted to any tracker from this plan.
