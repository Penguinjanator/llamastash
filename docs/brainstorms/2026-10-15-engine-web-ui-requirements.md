---
date: 2026-10-15
topic: engine-web-ui
---

# Web UI for engines without one (`/ui`)

## Problem Frame

`/ui` reverse-proxies the running model's own web UI, which only llama.cpp serves. Every other launch (vLLM, SGLang, generic entries like gufo and Halogen) shows as a red, non-clickable row in the chooser, so a user running those engines has no browser chat through llamastash.

Checked 2026-10-15 against the latest releases:

- vLLM v0.31.0: no chat UI. It serves Swagger API docs at `/docs` only.
- SGLang v0.5.21: no chat UI. `/` returns `SGLang is running`. Its `realtime_webui` belongs to the diffusion app.

So no engine has a UI to route to. The fix is to ship llama.cpp's UI (`tools/ui`, MIT) inside llamastash and serve it for launches that have none.

What the llama.cpp UI (prebuilt `b11642`) needs from a server:

| Route | Need | Other engines |
|---|---|---|
| `GET ./props` | Required. On error the chat screen shows a server error. | None has it. llamastash answers it. |
| `POST ./v1/chat/completions` (SSE) | Required. Always sends `return_progress`, `sse_ping_interval`, `reasoning_format`. | vLLM accepts unknown fields (logs a warning). SGLang ignores them. gufo, Halogen: untested. |
| `delta.reasoning_content` | Only field read for thinking text. | SGLang sends it. vLLM sends `delta.reasoning`, so thinking is not shown. |
| `GET ./v1/models` | Model name for requests. | All have it. |
| `/slots`, `/v1/stream`, `/v1/chat/completions/control`, `/tools`, `timings` | Optional llama.cpp features. | Missing. Those features stay off. |

## Requirements

**Which launches get the bundled UI**
- R1. A Ready launch whose backend serves no web UI and that serves chat gets the bundled llama.cpp UI at `/ui/`. Its chooser row turns green and is clickable, and a lone such launch opens directly, like a lone llama.cpp launch today.
- R2. llama.cpp launches keep their engine's own UI, unchanged.
- R3. A launch that does not serve chat (embedding or rerank only) stays a red, non-clickable row.
- R4. No backend is named in the generic `/ui` code. The choice keys on the existing "serves a web UI" verdict, so a new UI-less backend gets the bundled UI without edits.

**What the bundled UI shows**
- R5. The UI loads without a server error and streams chat answers from the launch.
- R6. The context gauge shows the launch's resolved context size.
- R7. Image attach is offered only when the launch takes images (the same vision flag `list` and `integrations` use).
- R8. Requests carry a model name the engine accepts, including engines that check it (gufo's `--served-model-name`).

**Packaging**
- R9. The UI files are embedded in the llamastash binary. Opening `/ui` needs no network.
- R10. Nothing is unpacked or held in memory until the first request for the bundled UI. Daemon startup and idle memory are unchanged when `/ui` is not used.
- R11. The embedded UI version is pinned and recorded in the repo. A maintainer script or `make` target refreshes it to a given llama.cpp build, checking the published sha256.
- R12. The llama.cpp MIT license notice ships with the binary.

**Behavior kept as is**
- R13. Bundled-UI chat traffic stays out of the request log, like all `/ui` traffic today.
- R14. Cookie pin, chooser, `/ui/switch`, `?target=` and LAN Basic auth work the same for bundled-UI launches.

**Docs**
- R15. `docs/usage.md` § Web UI says which engines get the bundled UI and lists its limits: no thinking text on vLLM, llama.cpp-only features off, and one reload may show the previous UI version after switching between a llama.cpp model and a bundled-UI model (service worker cache).
- R16. `TODO.md` gets a follow-up for approach B (below), and the batch 5 item is struck.

## Success Criteria

- With one vLLM, SGLang, gufo or Halogen launch running, `http://127.0.0.1:11435/ui/` opens a working chat. A message streams an answer, and the context gauge matches `status` ctx. Verified live against each real engine, not a fake.
- With several launches running, the chooser shows those launches in green and opens them.
- llama.cpp launches behave exactly as before.
- Release tarball grows by about the size of the UI archive (3.1 MB today, on a 7.1 MB v0.7.0 tarball), and not more.

## Scope Boundaries

- No router mode (approach B). It is a follow-up in `TODO.md`.
- No rewriting of engine responses, so vLLM thinking text stays hidden in `/ui`. The `/v1` no-body-translation rule is unchanged.
- No service worker changes.
- No runtime download or update of the UI.
- No sidecar `llama-server` process.
- Lemonade is not checked for a UI of its own. Its chat rows get the bundled UI through R4 with no special case. Not tested.

## Key Decisions

- **Shim per launch now (A), router mode later (B).** A is the smallest change that makes every engine usable in `/ui`. B serves one bundled UI for all engines as a llama.cpp router (`/props` returns `role: router`), so the UI's own picker lists the catalog and chat goes through normal proxy routing. B would remove the chooser, `/ui/switch` and the two-UI-versions issue, but llamastash would have to follow upstream router API changes.
- **Embed instead of download on first use.** Works offline and the UI version only moves with llamastash releases. Costs about 3.1 MB per binary.
- **No sidecar process.** A `llama-server` sidecar would only serve static files. The proxy would still have to answer `/props` and route chat, and the sidecar needs llama.cpp installed. The proxy serves the files itself.
- **Accept the service worker issue.** The UI registers a service worker under `/ui/`. After switching between a llama.cpp model and a bundled-UI model, one reload can show the other UI version. Documented, and B removes it.
- **No vLLM reasoning rename.** Keeps the proxy out of response rewriting.

## Dependencies / Assumptions

- The prebuilt UI is published as `dist.tar.gz` plus `.sha256` at `https://huggingface.co/buckets/ggml-org/llama-ui/resolve/<b<N> | latest>/` (seen in llama.cpp `scripts/ui-assets.cmake`; `latest` was `b11642` on 2026-10-15).
- The UI stores chats in IndexedDB (Dexie, schema `version(1)` in `b11642`). If a future llama.cpp UI bumps that schema, an older UI on the same origin may fail to open the store. Unverified beyond `b11642`.
- gufo and Halogen are only on the Z13, so their live checks run there.

## Outstanding Questions

### Deferred to Planning

- [Affects R5][Technical] Which `/props` fields the UI reads without guarding against null, so the answer is the smallest one that does not crash.
- [Affects R8][Technical] Where the UI's model list and chat calls go: straight to the engine (its own `/v1/models` names what it accepts) or through proxy routing (applies `rewrite_model`, aliases).
- [Affects R5][Needs research] Whether gufo and Halogen accept the extra llama.cpp request fields or refuse them.
- [Affects R9, R11][Technical] Where the vendored archive lives (repo blob vs a pinned build-time fetch), given crates.io's 10 MB crate limit, offline docs.rs builds, and repo growth on each refresh. Whether to drop the ~220 KB of PWA splash PNGs.
- [Affects R12][Technical] How to carry the third-party licenses bundled in the UI's JS, not just llama.cpp's.

## Next Steps

-> /ce:plan for structured implementation planning
