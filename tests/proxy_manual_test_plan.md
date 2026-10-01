# Manual test plan — Proxy router

Broad-scope manual verification of the proxy feature. Run after every
substantive proxy change; sign-off goes into the PR description as:

> Proxy manual plan green at `<commit-sha>`.

Companion to [`tests/proxy_real_client_smoke.md`](proxy_real_client_smoke.md),
which is the tight pre-release OpenCode + Pi smoke. This plan adds the
Ollama-discovery surface, family-MRU fallback verification, real-client
integration matrix, and explicit "won't work OOB" boundaries.

Repo-relative file references throughout. All shell commands assume the
cwd is the repository root unless noted.

## Scope

What this plan covers that integration tests don't:

- Auto-start UX latency on a dormant model under a real client
- Byte-pure SSE forwarding under a live streaming consumer
- Family-MRU fallback headers visible to a real client (`launch_failed`
  vs `family_mismatch`)
- Ollama-discovery surface (`/api/tags`, `/api/version`, `/api/ps`,
  `/api/show`) recognised by Ollama-shape clients
- Cross-endpoint digest stability (regression coverage for the
  path-vs-header divergence fixed in `3afae7a`)
- `--proxy-port` override surviving the default detached re-exec

## Preflight

Reuse the preflight + cleanup from
[`tests/proxy_real_client_smoke.md`](proxy_real_client_smoke.md)
§Preflight (steps 1-6). At the end you should have:

- Daemon running, proxy listening on `127.0.0.1:11434`
- `<MODEL_R>` Ready, `<MODEL_D>` dormant
- Both names recorded for the runs below

Additional preflight for this plan — sanity-check the four Ollama
discovery endpoints come up at all:

```bash
for ep in /api/version /api/tags /api/ps; do
  echo "== GET $ep ==";
  curl -sS "http://127.0.0.1:11434$ep" | jq .
done
curl -sS http://127.0.0.1:11434/api/show \
  -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL_R\"}" | jq .
```

If any returns a non-200 or unparseable body, abort and file before
proceeding.

---

## Test 1 — Ollama discovery probes (curl, no agent)

Pure wire-shape verification. Fast (≈30s) and catches schema regressions
before any client is involved.

```bash
# 1a — version is non-empty + single field
curl -sS http://127.0.0.1:11434/api/version \
  | jq -e 'keys == ["version"] and .version != ""'

# 1b — tags lists every discovered model and is alphabetically sorted
TAGS=$(curl -sS http://127.0.0.1:11434/api/tags)
echo "$TAGS" | jq -e '.models | length > 0'
echo "$TAGS" | jq -e '.models | map(.name) | . == sort'

# 1c — each row carries the documented Ollama fields (no nulls)
echo "$TAGS" | jq -e '.models | all(
  has("name") and has("model") and has("modified_at")
  and has("size") and has("digest") and has("details")
)'

# 1d — digest is blake3-prefixed (NOT sha256: — we are honest about
# the algorithm; see docs/usage.md §Ollama-compat surface)
echo "$TAGS" | jq -r '.models[].digest' \
  | grep -v '^blake3:' \
  && { echo "FAIL: non-blake3 digest"; exit 1; } \
  || echo OK

# 1e — ps lists only Ready supervisors
PS=$(curl -sS http://127.0.0.1:11434/api/ps)
echo "$PS" | jq -e ".models | length >= 1 and any(.name == \"$MODEL_R\")"
echo "$PS" | jq -e ".models | all(.name != \"$MODEL_D\")"  # dormant must not appear

# 1f — REGRESSION (digest stability across endpoints): the digest in
# /api/tags and /api/ps for $MODEL_R must be identical. This is the
# bug fixed in 3afae7a.
TAG_DIGEST=$(echo "$TAGS" \
  | jq -r ".models[] | select(.name == \"$MODEL_R\") | .digest")
PS_DIGEST=$(echo "$PS" \
  | jq -r ".models[] | select(.name == \"$MODEL_R\") | .digest")
test "$TAG_DIGEST" = "$PS_DIGEST" \
  && echo "digest stable: $TAG_DIGEST" \
  || { echo "FAIL: tag=$TAG_DIGEST ps=$PS_DIGEST"; exit 1; }

# 1g — show returns metadata
curl -sS http://127.0.0.1:11434/api/show \
  -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL_R\"}" \
  | jq -e '.details.family != "" and (.model_info | length > 0)'

# 1h — show accepts legacy `name` field (older ollama clients)
curl -sS http://127.0.0.1:11434/api/show \
  -H 'content-type: application/json' \
  -d "{\"name\":\"$MODEL_R\"}" \
  | jq -e '.details.family != ""'

# 1i — show without model returns 400 model_required
curl -sS -o /dev/null -w '%{http_code}\n' \
  http://127.0.0.1:11434/api/show \
  -H 'content-type: application/json' -d '{}' \
  | grep -qx 400
```

**Pass criteria:** every `jq -e` exits 0; 1f prints
`digest stable: blake3:...`; 1i prints `400`.

---

## Test 2 — Family-MRU fallback headers

The behavior neither Ollama nor LM Studio surface. Trigger it
deliberately so you can see the headers a real agent would inspect.

### Setup

Pick a model that will fail to auto-start (easiest: rename its file on
disk after discovery, then drive a request — header-read fails, the
proxy picks a Ready family-MRU fallback).

```bash
DPATH=$(llamastash list --json \
  | jq -r ".models[] | select(.name == \"$MODEL_D\") | .path")
mv "$DPATH" "$DPATH.tmp-hidden"
```

### Same-arch fallback → `launch_failed`

```bash
# $MODEL_D is unreachable; $MODEL_R is Ready and same arch.
curl -i -sS http://127.0.0.1:11434/v1/chat/completions \
  -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL_D\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}" \
  | tee /tmp/fallback-launch.txt \
  | grep -iE '^(HTTP/|x-llamastash)'
```

**Expect:**

- `HTTP/1.1 200 OK` (substituted, not failed)
- `x-llamastash-served-by: <MODEL_R>` (or whatever Ready model was picked)
- `x-llamastash-fallback-reason: launch_failed`

### Cross-arch fallback → `family_mismatch`

Only meaningful if you have a Ready model of arch X **and** a dormant
catalog row of arch Y. If your catalog has only one arch, skip.

```bash
# Force an embedding request to fall through to a chat model (or any
# cross-arch pair you have). $MODEL_E is the dormant embedding model
# whose path is hidden.
curl -i -sS http://127.0.0.1:11434/v1/embeddings \
  -H 'content-type: application/json' \
  -d "{\"model\":\"$MODEL_E\",\"input\":\"hi\"}" \
  | grep -iE '^x-llamastash'
```

**Expect:** `x-llamastash-fallback-reason: family_mismatch` — the wire
signal an embedding-aware client should branch on rather than parse
the unexpected chat-shaped output.

### Cleanup

```bash
mv "$DPATH.tmp-hidden" "$DPATH"
```

---

## Test 3 — `--proxy-port` CLI override

The CLI flag must beat config, and survive the default detached re-exec.

```bash
llamastash daemon stop || true
sleep 1

# 3a — override beats default (no config involved). `daemon start`
# detaches by default; no `--detach` flag exists any more.
llamastash daemon start --proxy-port 18080
llamastash status --json | jq -e '.proxy.listen == "127.0.0.1:18080"'
curl -sS http://127.0.0.1:18080/api/version | jq -e '.version != ""'
llamastash daemon stop && sleep 1

# 3b — override beats config (put a different port in config first)
# Use a temporary config file so the test doesn't perturb the user's.
TMPCFG=$(mktemp -d)/config.yaml
cat > "$TMPCFG" <<'YAML'
proxy:
  enabled: true
  port: 19999
YAML
llamastash --config "$TMPCFG" daemon start --proxy-port 18081
llamastash status --json | jq -e '.proxy.listen == "127.0.0.1:18081"'
llamastash daemon stop && sleep 1

# 3c — no override → config wins (regression for the silent-ignore bug
# fixed alongside the flag)
llamastash --config "$TMPCFG" daemon start
llamastash status --json | jq -e '.proxy.listen == "127.0.0.1:19999"'
llamastash daemon stop && sleep 1

# 3d — ephemeral port (--proxy-port 0). Useful in dev scripts that
# don't want to collide with anything else running.
llamastash daemon start --proxy-port 0
PORT=$(llamastash status --json | jq -r '.proxy.listen' | cut -d: -f2)
test "$PORT" != "0" && test "$PORT" -gt 1024 \
  && echo "ephemeral bound to $PORT" \
  || { echo "FAIL: expected real port, got $PORT"; exit 1; }
curl -sS "http://127.0.0.1:$PORT/api/version" | jq -e '.version != ""'
llamastash daemon stop
```

**Pass criteria:** each `jq -e` exits 0; 3d prints a non-zero bound port
and `/api/version` answers on it.

---

## Test 4 — OpenCode (OpenAI-shape)

Already covered by
[`tests/proxy_real_client_smoke.md`](proxy_real_client_smoke.md)
§Smoke 1 — OpenCode. Re-run as-is and additionally check:

```bash
# After OpenCode has driven a turn against $MODEL_D, /api/ps should
# now show it as Ready — verifies the Ollama discovery surface reflects
# OpenAI-compat-side auto-starts.
curl -sS http://127.0.0.1:11434/api/ps \
  | jq -e ".models | any(.name == \"$MODEL_D\")"
```

---

## Test 5 — Codex CLI (OpenAI-shape)

OpenAI's `codex` CLI supports a custom base URL. Verify against current
Codex docs at run time — flag names drift; the env-var form below is the
stable shape.

### Configuration

```bash
export OPENAI_BASE_URL="http://127.0.0.1:11434/v1"
export OPENAI_API_KEY="ignored-by-llamastash"

# If your codex version pins a specific model, point it at one
# llamastash serves:
codex --model "$MODEL_R" "say hi in three words"
```

### Pass criteria

- Codex completes the turn without auth or model-not-found errors
- Response is plausible (non-empty, terminates)
- Daemon log shows the request hitting `/v1/chat/completions`
- No `x-llamastash-fallback-reason` (confirm with the curl sanity-check
  from the existing runbook §Direct-curl sanity check)

### Auto-start

Repeat with `--model "$MODEL_D"` after a `llamastash stop "$MODEL_D"`
to verify Codex's streaming consumer tolerates the 5-30s first-token
latency.

---

## Test 6 — Ollama-shape discovery clients

This is what the Tier 1 surface unlocks: tools that probe `OLLAMA_HOST`
or `GET /api/tags` to recognise an Ollama-compatible endpoint, then fall
through to the OpenAI-compat completion endpoint for inference.

### 6a — `ollama-python` discovery

```bash
pip install -U ollama  # if not already
OLLAMA_HOST=http://127.0.0.1:11434 python -c '
import ollama
client = ollama.Client(host="http://127.0.0.1:11434")
models = client.list()
print("models:", [m["name"] for m in models.get("models", [])])
v = client.ps()
print("running:", [m["name"] for m in v.get("models", [])])
'
```

**Pass criteria:** listing prints every model from
`llamastash list --json`; running list contains `<MODEL_R>` (and any
other Ready supervisor) but not `<MODEL_D>`.

### 6b — Inference via `ollama-python` (deferred — Tier 2)

```bash
# This SHOULD fail with 404 / not-implemented. /api/chat is Tier 2
# deferred.
OLLAMA_HOST=http://127.0.0.1:11434 python -c '
import ollama
print(ollama.chat(model="'"$MODEL_R"'",
                  messages=[{"role":"user","content":"hi"}]))
' 2>&1 | head -5
```

**Expected:** an HTTP 404 / "no such route" error. **Not a regression**
— Tier 2 inference is tracked in [`TODO.md`](../TODO.md) §R2. If it
unexpectedly *works*, that means someone shipped Tier 2 and this
section is out of date.

### 6c — Continue (VSCode / JetBrains) with OpenAI provider

Add a Continue config block pointing at llamastash:

```json
{
  "models": [
    {
      "title": "llamastash via OpenAI-compat",
      "provider": "openai",
      "model": "<MODEL_R>",
      "apiBase": "http://127.0.0.1:11434/v1",
      "apiKey": "ignored"
    }
  ]
}
```

**Pass criteria:** Continue model picker shows the entry; one chat turn
completes; daemon log shows `/v1/chat/completions` traffic.

> **Note:** Continue's "Ollama provider" specifically uses `/api/chat`
> (Tier 2). Use the **OpenAI provider** pointed at the proxy's `/v1`
> base URL instead — that's the supported path until Tier 2 lands.

### 6d — Open WebUI (Ollama discovery + OpenAI inference)

If you run Open WebUI, point it at llamastash as an OpenAI backend
(not the Ollama backend, for the same Tier 2 reason). The model picker
will populate from `/v1/models` rather than `/api/tags`, but you can
independently verify the Ollama-discovery surface using Test 1 above.

---

## Known limitations — do not file as bugs

These are deliberate scope boundaries documented elsewhere; the manual
plan calls them out so a user-reported "doesn't work" can be triaged
fast.

**Claude Code CLI and Anthropic SDK clients now work** — point them at the proxy with `ANTHROPIC_BASE_URL` (key as `x-api-key`). The proxy forwards `/v1/messages` + `/v1/messages/count_tokens` to llama-server's native Anthropic endpoints; tool calling needs `--jinja` (on by default via `jinja: true`). See [`docs/usage.md §Anthropic-shape clients`](../docs/usage.md). Claude Code's `/effort` works on a llama.cpp model through the mapping in `src/backend/llama_cpp/effort.rs`; re-check it against a real engine with [`scripts/effort-live-check.sh`](../scripts/effort-live-check.sh), which prints the token counts of `output_config.effort` next to the same values sent as `chat_template_kwargs.reasoning_effort` (they must agree) and against an unmapped engine.

| Client | Status | Why | Tracked |
|---|---|---|---|
| **GitHub Copilot** (VSCode / IDEs) | Cannot be redirected | Closed-source binary speaks only to `api.githubcopilot.com`; no base-URL override exists. | Not tracked — out of scope |
| **GitHub Copilot CLI (`gh copilot`)** | Cannot be redirected | Same reason. | Not tracked |
| **Ollama-shape *inference* clients** (Continue with Ollama provider, `ollama-python` `.chat()`, anything that hits `/api/chat`, `/api/generate`, `/api/embed`) | Returns 404 | Tier 2 inference deferred; only Tier 1 discovery ships. | [`TODO.md`](../TODO.md) §R2 — "Ollama-compat Tier 2" |

If a user wants Claude Code → llamastash today, the realistic option is
a community sidecar (like `claude-code-router`) that translates
Anthropic ↔ OpenAI in front of the proxy. Out of scope for the
maintainer-run plan.

---

## Cleanup

```bash
unset OPENAI_BASE_URL OPENAI_API_BASE OPENAI_API_BASE_URL OPENAI_API_KEY OLLAMA_HOST
llamastash daemon stop
```

## Records to keep

- Commit SHA the plan was run against (paste into release PR sign-off)
- Any header-value drift (Codex / Continue / OpenCode env-var rename)
- First-token latency outliers for auto-start (anything > 30s on a
  7B-Q4 model)
- Any deviation from §Known limitations (a client that newly works, or
  one that newly breaks)
