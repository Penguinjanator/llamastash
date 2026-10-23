---
date: 2026-10-23
topic: proxy-media-routes
---

# Proxy routes for image, TTS and STT models, plus declared routes on generic entries

## Goal

Make generic servers usable through the proxy for any model type, with no client changes:

1. The proxy forwards the common OpenAI image and audio routes, routed by `model` like the chat routes.
2. A generic entry can declare extra path prefixes it serves (native APIs such as ComfyUI `/prompt`, A1111 `/sdapi/v1`, whisper.cpp `/inference`).
3. A client that hardcodes a name the proxy does not know (`whisper-1`, `tts-1`) still reaches the one entry of that type.

Keep it simple. No new crates. No engine probing.

## Decisions

- **Curated list + declared prefixes, not a wildcard.** Nothing outside the built-in routes and the declared prefixes is forwarded, so engine admin and metrics paths (`/slots`, `/props`, `/metrics`, `/load`, ...) stay unreachable through the proxy unless a user declares them.
- **Built-in routes and modes** (all `POST`):

  | Route | Mode | Body |
  |---|---|---|
  | `/v1/images/generations` | `image` | JSON |
  | `/v1/images/edits` | `image` | multipart |
  | `/v1/images/variations` | `image` | multipart |
  | `/v1/audio/speech` | `tts` | JSON, binary or streamed response |
  | `/v1/audio/transcriptions` | `stt` | multipart, optional SSE response |
  | `/v1/audio/translations` | `stt` | multipart |

- **Modes reuse the existing `modes` field** of a generic entry. `LaunchMode` gains `image`, `tts`, `stt`. The existing `unsupported_endpoint` refusal covers the new routes unchanged: an entry that lists `modes` without the route's mode is refused before start; an entry with no `modes` (default: all) is forwarded and the engine answers. The new modes are routing-only: they change no launch argv. llama.cpp, vLLM and SGLang do not serve the new modes (`serves_mode` false), so a request naming one of their models gets `unsupported_endpoint`.
- **Multipart `model`.** For `multipart/form-data` bodies the proxy reads the `model` form field. A small hand-written parser (boundary from `Content-Type`, scan parts, `Content-Disposition: form-data; name="model"`), not a new crate. The body is still buffered under `proxy.max_body_size`.
- **Resolution order on the new routes:**
  1. `model` matches a row or a `proxy.aliases` key (existing `decide`).
  2. Otherwise, when `decide` answers `NotFound` or `ModelRequired`: candidates are catalog rows whose entry lists the route's mode **explicitly** in `modes` (an entry with empty `modes` is not a candidate). Exactly one candidate row → that row (auto-start when stopped).
  3. Otherwise, exactly one of those candidates is Ready → that one.
  4. Otherwise the original answer (`model_not_found` / `model_required`), with the message also saying to name the model or add a `proxy.aliases` entry.
  - `Ambiguous` from step 1 is returned as today, no fallback: the client named something two models match.
  - The fallback applies to the new media routes only. Chat, embedding and rerank routing is unchanged.
- **Rewrite on fallback.** When steps 2-3 pick the row, the forwarded body's `model` is set to that row's served name (the same edit `rewrite_model` does), for JSON and multipart. The client's name meant nothing to us, so this is safe and lets strict engines answer. A body with no `model` field stays unchanged. The response carries the existing `x-llamastash-served-by` header and `x-llamastash-fallback-reason: mode_match`, and the request-log row's `fallback` is `mode_match`.
- **Declared routes: `routes:` on a generic entry**, a list of path prefixes (`routes: [/prompt, /view, /history]`).
  - Prefix match on a segment boundary: `/prompt` matches `/prompt` and `/prompt/x`, not `/prompts`.
  - Any method; query string kept; body buffered under `proxy.max_body_size` and forwarded verbatim (no JSON parse, no rewrite); response streamed back.
  - Target: a Ready launch of that entry; else, when the entry has exactly one catalog row, auto-start it (no endpoint mode); else `400 ambiguous_model` listing the entry's rows.
  - Logged like other routes, with the matched **prefix** as the row's route (config-controlled, never the client's raw path).
  - Behind the same bearer key as everything else in LAN mode.
  - Matched after every built-in route.
- **Config-load errors** (through `GenericConfig::validate`, same severity as the other generic entry refusals):
  - a prefix that does not start with `/`, is `/`, or contains `?`, `#` or `*`;
  - a prefix that equals, lies under, or contains a proxy-owned path: every route in the router's forwarded list, `/v1/models`, `/health`, `/metrics`, `/ui`, `/api`. Message names the built-in path and says to remove it. (So `/v1` alone is refused.) The proxy-owned list lives in `src/proxy/` and is passed into `validate`, so the generic module names no proxy internals beyond that list;
  - two entries whose prefixes are equal or one lies under the other (names both entries). One owner per path.
- **Out of scope, recorded in `TODO.md`:** OpenAI video jobs API (`/v1/videos` + `GET /v1/videos/{id}` needs a job-id → model map), `/v1/realtime` and WebSocket upgrade on declared routes (ComfyUI `/ws`), built-in backends claiming the new modes (vLLM serves Whisper on `/v1/audio/transcriptions`), routing by names an engine reports in its own `/v1/models`.

## Units

One lane, sequential. U1 and U2 share `src/proxy/router.rs` and the docs.

### U1. Media routes, modes, multipart, mode fallback

Files:
- `src/launch/mode.rs`: add `Image`, `Tts`, `Stt` (labels `image`, `tts`, `stt`); fix every exhaustive match the compiler flags. CLI `--mode` (`src/cli/cli_args.rs` `LaunchMode`) stays chat/embedding/rerank.
- `src/proxy/router.rs`: add the six routes to `GENERATION_ROUTES`; replace the inline path→mode match with one `fn endpoint_mode(path) -> Option<LaunchMode>`; mode fallback + rewrite wiring in `forward_request`.
- `src/proxy/route.rs`: `buffer_and_extract` takes the `Content-Type` and reads `model` from multipart; the fallback candidate pick (a helper next to `decide`).
- `src/proxy/forward.rs`: `with_model` (or a sibling) also rewrites the multipart `model` part; a way for the router to pass the fallback's served name as the request-model name.
- `src/util/multipart_body.rs` (new) + `src/util/mod.rs`: find a field's value range, replace it.
- `src/backend/mod.rs` + `src/backend/generic/mod.rs`: a trait method telling whether a row's entry lists a mode explicitly (default `false`; generic returns `modes.contains`). llama.cpp / vLLM / SGLang `serves_mode` false for the new modes.
- `src/backend/generic/config.rs`: `modes` accepts the new labels.
- Tests: unit tests for the multipart parser and rewrite (field first, last, missing, CRLF boundaries, binary file part), `endpoint_mode`, the candidate pick; one integration test in `tests/` that sends a multipart `/v1/audio/transcriptions` and a JSON `/v1/audio/speech` through the proxy to a fake server, covering: routed by `model`; unknown `model` routed by mode fallback with the body's `model` rewritten and the `mode_match` header; two `tts` entries + unknown name → `model_not_found`. Reuse the existing generic-entry test fixtures.

### U2. Declared routes on generic entries

Depends on U1.

Files:
- `src/backend/generic/config.rs`: `routes: Vec<String>` field; the validation rules above, taking the proxy-owned path list; unit tests for each refusal.
- `src/proxy/router.rs`: the proxy-owned path list (shared with `GENERATION_ROUTES`); a declared-route arm before `not_found`; the forwarder for it (buffer, pick target, forward verbatim, log under the prefix). Put the forwarder in a new `src/proxy/declared.rs` if it would push `router.rs` much further.
- Wherever generic config is validated at load, pass the list.
- Integration test: a declared prefix on a fake server forwards a `GET` with a query and a `POST` body verbatim, auto-starts a stopped entry, and leaves a request-log row with the prefix as route; an undeclared path still 404s.

### U3. Docs (same commits as the code)

- `docs/usage.md`: proxy route list, `modes` values, `routes` field row in the generic entry table, mode fallback + rewrite + `mode_match` header, config errors.
- `docs/architecture.md`: proxy scope (curated routes + declared prefixes).
- `AGENTS.md` "Proxy scope" bullet: one-line update, stay under 200 lines.
- `config.example.yaml`: a commented `modes`/`routes` example on a generic entry.
- `RECIPES.md`: whisper.cpp and FastFlowLM entries gain `modes: [stt]` and note the proxy route.
- `CHANGELOG.md` `[Unreleased]`: one line.
- `TODO.md`: strike "audio/tts/video/etc support on proxy" (Next) and "The proxy has no route for audio" (High priority) with a pointer to this plan; add the out-of-scope items above as new open entries under `## Next`.

## Verification

Setup: none (Rust only). Build dir is the worktree's own `target/`.

While coding: `cargo check --all-targets --features test-fixtures`, then narrow `cargo nextest run --features test-fixtures --lib <filter>` / `--test <name>`.

Before finishing:
- `make lint`
- `cargo nextest run --features test-fixtures`
- `cargo test --doc --features test-fixtures`

The pre-commit hook runs fmt + lint + doc on each commit.

Result (2026-10-23): `make lint`, `cargo nextest run --features test-fixtures`
(3396 tests) and `cargo test --doc --features test-fixtures` pass on the branch
tip. Beyond the suite, the real binary against a real third-party engine
(python's `http.server` as the generic entry) confirmed the shapes end to end:
`GET /inference` and `/inference?task=asr` answered from the engine with the
entry auto-started, `POST /v1/audio/transcriptions -F model=whisper-1` came
back `x-llamastash-served-by: httpbin` with `x-llamastash-fallback-reason:
mode_match`, an undeclared `/static` 404s, and `llamastash requests` files the
two under `/inference` and `audio/transcriptions`.

New tests: multipart field parse and rewrite (`src/util/multipart_body.rs`),
`endpoint_mode`, the media modes being the declared-only ones (`src/launch/mode.rs`),
the declared-route config refusals (`src/backend/generic/config.rs`), and in
`tests/generic_backend_test.rs`: media routes forwarding multipart and JSON with
auto-start, mode fallback with the rewrite and its header, a mode mismatch
refused before any launch, a model-less media request, and a declared prefix
reaching its engine (query kept, unparsable body forwarded, log filed under the
prefix, undeclared paths 404).

## Implementation notes

Shipped as specified. Where the build differs from the plan text:

- The mode gate uses `declares_mode`, not `serves_mode`, for `image` / `tts` / `stt`. `serves_mode` treats an entry with no `modes:` as an OpenAI server for everything, which let a plain chat entry take an upload it has no endpoint for.
- The fallback and the gate live in `forward_request` (`src/proxy/router.rs`), after `route::decide`, rather than inside `decide`. A media route whose name resolves to a row is answered or refused by that row; the mode pick only runs when the name resolves to nothing.
- Media routes are `POST`/`PUT` like the rest of the curated list, so the "no name at all" case is a body-less `POST`, which routes on the mode alone. A `GET` on those paths stays a 404.
- The rewrite is byte surgery on the `model` part (`src/util/multipart_body.rs`), no new dependency; part order, boundaries and file bytes are untouched, which the fake server proves by echoing the request bytes.
- The declared-path hook is `Backend::declared_path_prefix(path, server, request_path)` and reads the config entry rather than a live port: prefixes are config, and the row is already resolved before the hook is asked.
- A prefix nested inside another is refused only across entries. Nested prefixes inside one entry are that entry's longest-match business, which is how `/internal` and `/internal/deep` coexist on one row.
- A declared path buffers through `proxy.max_body_size` and, unlike a curated route, never refuses an unparsable body: a body the proxy cannot read names no model and is forwarded as sent.
- `/v1/models` publishes a declared media mode. `ModeHint` gained `Image` / `Tts` / `Stt`, so a row for an entry with `modes: [stt]` prints `stt` rather than `chat`; the three hints resolve to a chat launch and carry no Ollama capabilities, which is what keeps them routing-only.
- A declared path resolves dot segments before it matches anything (`src/util/http_path.rs`), including the `%2e` spellings a URL parser would decode, and forwards the resolved path rather than what the client wrote. A configured prefix containing a dot segment is refused at load. Without this, `/internal/../metrics` reached the metrics endpoint beside a declared `/internal`.
- A declared path rewrites nothing at all, not even the launch's own `rewrite_model` pin: `forward::Target::pin_request_model` is off there. The pin is the entry's contract on the OpenAI routes, not a licence over the engine's own API bodies.
- The declared-path lookup skips every row for a backend that declares no paths, so an unlisted path costs a config flag and a row scan instead of a backend resolution and an identity probe per row.
- Tracked in `TODO.md`: the chat routes are not mode-gated, video, `/v1/realtime`, a media engine's own model list, and built-in engines claiming a media mode.
- The `TODO.md` strike the plan called for (an unlisted path prefix item) is not in the file; nothing needed striking for U2.

## Definition of done

- All routes in the table forward through the proxy, multipart included.
- Mode fallback and rewrite behave as specified, observable in headers and the request log.
- Declared routes forward, auto-start, log, and every config refusal fires with a clear message.
- Docs and `TODO.md` updated in the same commits. Verification commands pass.
