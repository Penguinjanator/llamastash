---
title: "feat: init patchers wire effort, Linux CUDA prebuilt, Codex CLI entry"
type: feat
status: implemented
date: 2026-09-30
origin: R12 batch 3 in TODO.md; live tests 2026-09-30
---

# feat: `init` patchers and installer

**Status:** implemented. Mini plan. Three independent units in `src/init/`.

## Overview

1. **Patchers wire effort controls** for reasoning models, so a client's effort
   picker works against llamastash.
2. **Linux + NVIDIA installs the upstream CUDA prebuilt** instead of Vulkan.
3. **Codex CLI entry** in the `init` picker.

## Rule for every patcher change

Validate the client's behavior at its source first (default branch, cite the
commit), and test it live where possible. The client notes below are a
starting point, not a spec.

## 1. Effort controls in patchers

**Problem.** No patcher in `src/init/external/tools/` writes a reasoning field,
so client effort controls do nothing. pi: `/thinking low` fails with
`Unknown thinking level "low". Available levels: off.`

**What the engines do** (measured 2026-09-30, temp 0, same prompt, one launch
each, per-request `reasoning_effort` on `/v1/chat/completions`):

| Engine | low | xhigh | none |
|---|---|---|---|
| llama.cpp master `f872b5911`, Qwen3.8-27B | 46 | 106 | thinking off |
| gufo `fd1710b`, Flash-Next | 61 | 106 | 27 |
| halogen `sha256:414872ef`, Flash-Next | 63 | 112 | 28 |

The llamastash proxy forwards the field unchanged. So this is client config
only; no server change.

**Levels come from the chat template.** Qwen3.8's template accepts `xhigh`
(default), `medium` and `low`, turns `high` into `xhigh`, and raises
`Unexpected reasoning effort <x>. Supported types are xhigh (default), medium,
and low.` on anything else. `none` turns thinking off before that check. Keep
one per-template level table (from the chat template or a per-arch list) that
every patcher reads, and write fields only for rows with `has_reasoning_hint`.

**Per client:**

- **pi** (tested live, 0.99.2): write `"reasoning": true` and, for Qwen3.8,
  `"thinkingLevelMap": {"off": "none", "minimal": null, "low": "low",
  "medium": "medium", "high": null, "xhigh": "xhigh"}`. pi hides a level mapped
  to `null` and shows `xhigh`/`max` only when mapped
  (`getSupportedThinkingLevels`, `packages/ai/src/models.ts`). Without
  `reasoning: true` no effort is sent; with it and no map, `xhigh` goes out as
  `high` and `off` sends nothing.
- **opencode** (source only): `reasoning: true` plus explicit `variants`, e.g.
  `{"low": {"reasoningEffort": "low"}, ...}`. It builds no variants itself for
  ids containing `qwen` (`packages/opencode/src/provider/transform.ts`
  `variants()`). `@ai-sdk/openai-compatible` sends `reasoningEffort` as
  `reasoning_effort` (`openai-compatible-chat-language-model.ts:315`). Also
  write `limit.context`: the patcher writes none today and config models
  default to `0` (`provider.ts` ~1609).
- **Zed** (source only): a `reasoning_effort` default on an `available_models`
  entry turns the picker on, but its list is fixed to minimal ... max
  (`OPENAI_COMPATIBLE_SELECTABLE`, `language_model_core.rs:817`), so `minimal`
  and `max` hit the template error. Verify the error text reaches the user
  readably (not a bare 500); a proxy clamp was dropped and is reconsidered only
  if it does not.
- **Continue** (source only): no `reasoning_effort` found on its OpenAI chat
  path. Confirm before deciding there is nothing to write.
- **Aider** (source only): `--reasoning-effort` needs the model to list it in
  `accepts_settings` (`aider/models.py:150`), so the patcher would write a
  model-settings entry.
- **Claude Code**: covered by
  [the Anthropic effort mapping plan](2026-09-30-002-feat-anthropic-effort-mapping-plan.md).

## 2. Linux + NVIDIA CUDA prebuilt

**Problem.** `pick_asset_suffix` sends `OsFamily::Linux` + `GpuInfo::Nvidia`
to `ubuntu-vulkan-x64.tar.gz` (`src/init/install/gh_releases.rs:84-86`).
llama.cpp now ships Linux CUDA builds, so this is routing only.

**Assets** on `b11276` (read 2026-09-30): `ubuntu-cuda-12.8-x64`,
`ubuntu-cuda-13.4-x64`, `ubuntu-cuda-13.4-arm64`, each with a
`cudart-llama-...` companion bundle carrying the CUDA runtime, so no CUDA
toolkit is needed. Absent at `b10700`, present by `b11000`.

**Design.** A CUDA branch for Linux + NVIDIA; pick the CUDA minor from the
installed driver; a CUDA-vs-Vulkan prompt in the wizard; fetch the matching
`cudart` bundle.

**Open.** Is the existing `--version` smoke (`src/init/smoke.rs`) enough to
fall back to Vulkan when the CUDA build will not load? This box is AMD, so the
branch needs an NVIDIA host (or the UAT matrix) to test.

## 3. Codex CLI entry

llama.cpp serves `POST /v1/responses` and `/v1/responses/input_tokens`
(`tools/server/server.cpp:264,283-284`, master, read 2026-09-30) and the proxy
already byte-pipes both (`src/proxy/router.rs:122-123`). Codex CLI dropped
`wire_api = "chat"` in mid-2026
([discussion #7782](https://github.com/openai/codex/discussions/7782)).
Confirm the routes answer on the llama.cpp build the installer fetches, then
add a Codex patcher beside the others. Codex stays out of the picker until
then.

## Units

- [x] U1. Per-template effort level table (Qwen3.8 first) read from the chat
  template or a per-arch list; `PatchModel` carries it for
  `has_reasoning_hint` rows.
- [x] U2. pi patcher writes `reasoning` + `thinkingLevelMap`. Live test with pi.
- [x] U3. opencode patcher writes `reasoning`, `variants` and `limit.context`.
  Validate at source, live test if possible.
- [x] U4. Zed, Continue, Aider: validate at source, write what applies, record
  what does not.
- [x] U5. Linux + NVIDIA CUDA routing, driver-to-CUDA-minor choice, `cudart`
  bundle, wizard prompt.
- [x] U6. Codex: confirm `/v1/responses` on the fetched build, add the patcher
  and picker entry.
- [x] U7. Docs: `docs/usage.md` (`init` targets), `INSTALL.md` (CUDA),
  `CHANGELOG.md`.

## Outcome

- **Levels:** parsed from the chat template's `reasoning_effort not in (...)`
  check, plus its `|default(...)`, in `src/init/external/effort.rs`. Not per
  arch: Qwen3.8-27B reports `qwen35`, shared with Qwen3.5/3.6, and
  Flash-Next reports `qwen4exp`. `none` is offered when the template reads
  `enable_thinking` (llama.cpp master `f872b5911`, `server-common.cpp`, maps
  `none` to `enable_thinking = false`).
- **pi** (`main` `17f3dccbe`, 0.99.2): as planned. `off` maps to `none` when
  the template can turn thinking off, else `null`.
- **opencode** (`dev` `e9f8a210b`, 1.18.33): as planned; `limit.output` is
  required by the config schema, written as `min(32000, context / 2)`.
  `context: 0` also turns compaction off (`session/overflow.ts`).
- **Zed** (`main` `8e7fbcc13`, 1.22.0): writes the template's default as
  `reasoning_effort`. Its list stays minimal ... max.
- **Continue** (`main` `5522c6f44`, v2.0.0): nothing to write; effort is sent
  only for `o*` / `gpt-5+` over the Responses API, fixed to `medium`.
- **Aider** (`main` `5dc9490bb`, 0.86.0): nothing written. `/reasoning-effort`
  is not gated; a model-settings entry for the flag would replace Aider's
  name-based defaults for that model.
- **CUDA:** open question answered: `--version` is not enough. With
  `GGML_BACKEND_DL=ON` the CUDA backend is a plugin that is skipped when it
  cannot load. Run on this AMD host against the real `b11302` CUDA 13.4 build
  plus `cudart-` bundle: libraries resolve through `$ORIGIN`, `--version`
  exits 0, `--list-devices` lists `(none)`. `init` checks for a CUDA device
  and falls back to Vulkan. Not yet run on an NVIDIA host (TODO.md).
- **Codex** (`main` `60947e234`, 0.159.2; local 0.150.1): written as a
  `--profile llamastash` file (`$CODEX_HOME/llamastash.config.toml`) so
  `config.toml` is untouched; no TOML merge needed. Checked with codex
  0.150.1: the profile layer, the `auth` command and `wire_api =
  "responses"` work, and `reasoning.effort` is sent only when
  `model_reasoning_effort` is set.

## E2E (2026-10-01, this AMD host)

Isolated daemon (scratch `HOME` + `LLAMASTASH_*` dirs, proxy `:41535`),
llama-server master `f872b5911` (build 11310), favorite
`unsloth/Qwen3.8-27B-GGUF` `UD-Q6_K`, `llamastash integrations
pi,opencode,zed,codex,aider`:

- Files match the unit tests, levels read from the real GGUF.
- pi 0.99.1, capture listener: `--thinking off/low/medium/xhigh` send
  `none/low/medium/xhigh`; the hidden `high` and `minimal` clamp to
  `xhigh` and `low`. Through the proxy: `low` and `off` answer.
- opencode 1.18.33: `--variant low/none/xhigh` send those values; `--variant
  low` answers through the proxy.
- codex 0.150.1 (behind 0.159.2): `--profile llamastash` answers through
  `/v1/responses`, with and without `-c model_reasoning_effort=low`.
- A rejected level (`minimal`, as Zed can send) returns HTTP 500 with an
  OpenAI error body whose message ends `Unexpected reasoning effort minimal.
  Supported types are xhigh (default), medium, and low.` Zed itself was not
  driven (GUI), so how it renders that is not checked.

## Follow-up: proxy URL in tool configs

Found during E2E: the integrations wrote `config.proxy.effective_port()`
(config, else `11435`), so a proxy that moved past a busy port or a daemon
started with `--proxy-port` got configs pointing at the wrong port. With
another daemon on `11435`, that is a different daemon's proxy. Now the
order is: the daemon's `status.proxy.listen` when `status` is `listening`
(wildcard host mapped to loopback), then config, then the default
(`src/init/external/proxy_url.rs`). Checked live: daemon moved to `11436`
and to `41536` (`--proxy-port 41535` held by another listener); pi,
OpenCode, Zed and Codex configs got the moved port.

## Verification

- `make test`, `make lint`; patcher golden tests per client.
- `init` against scratch config dirs, then run each client that is installed
  against an isolated daemon and switch effort levels.
- CUDA: the release-gate UAT or an NVIDIA host; state the host when recording.
