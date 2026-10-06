# LlamaStash Command Reference

Agent-facing command reference. Prefer the JSON forms documented here.

## Installation and health

### Install the binary

```bash
# macOS
brew install llamastash/llamastash/llamastash

# Linux
curl -fsSL https://llamastash.dev/install.sh | sh

# Portable fallback
cargo install llamastash
```

### Verify the binary

```bash
llamastash --version
```

### First-run setup

```bash
llamastash init --recommended --json
llamastash init --recommended --only server --json
llamastash init --recommended --offline --json
```

Important exit codes:

- `0`: full success
- `72`: init aborted before substantive work
- `73`: download failed
- `74`: smoke launch failed

### Health check

```bash
llamastash doctor --json
```

`doctor` always exits `0`. Inspect `findings`.

## Catalog and runtime state

### List models

```bash
llamastash list --json
llamastash list --json | jq '.models[].name'
```

Use exact discovered names from this output for later commands.

### Status

```bash
llamastash status --json
llamastash status --json | jq .proxy
llamastash status --json | jq -r .proxy.listen
```

Use `status --json` for:

- running model state
- daemon build and pid
- host CPU, RAM, and GPU readings
- proxy listen address and bind status

## Model lifecycle

### Start a model

```bash
llamastash start <exact-model-name>
llamastash start <exact-model-name> --ctx 16384 --reasoning on
```

After `start`, confirm with:

```bash
llamastash status --json
```

### Stop a model

```bash
llamastash stop <exact-model-name>
llamastash stop --all --yes
```

Important exit codes:

- `66`: zero or multiple matches
- `67`: launch failed
- `68`: stop failed

### Read a launch's log

```bash
llamastash logs <launch-id-or-model-name> --json
llamastash logs <launch-id-or-model-name> -n 50 --json | jq -r '.lines[]'
```

The target is a launch id from `status --json` (for example `L3`), a port, a
launch name, or part of a running model's name. `--json` returns
`{"launch_id": "...", "lines": [...]}` with the last 200 lines, or `-n` lines.

Use it when a launch is in the `error` state or a model answers with errors: a
launch that died stays in `status` until it is stopped, and its log has the
cause. Do not pass `-f` from an agent, it follows the log and does not return.
An ambiguous name exits `66`.

## Discovery and downloads

### Recommend models

```bash
llamastash recommend --json
```

### Pull from HuggingFace

```bash
llamastash pull <owner/repo[:filename.gguf]> --json
llamastash pull <owner/repo[:filename.gguf]> --revision <sha> --json
```

Use `--revision <sha>` for reproducible downloads.

## Proxy for other harnesses

### Read the current base URL

```bash
llamastash status --json | jq -r '.proxy.listen'
```

Convert that to:

```text
http://127.0.0.1:<port>/v1
```

Typical defaults:

- normal mode: `11435`
- Ollama-compat mode: `11434`

### Quick sanity check

```bash
curl -sS http://127.0.0.1:11435/v1/models
```

If a client gets connection refused, first check:

```bash
llamastash status --json | jq .proxy
```

### Find out why a request failed or hangs

```bash
llamastash requests --json
llamastash requests <model-name> --json     # or a running launch: L3, its port, its name
llamastash requests --json | jq -c '.requests[] | {route, status, state, error, cause, launch_id}'
```

The proxy keeps its last 1000 requests in memory, newest first. `-n` sets how
many to return (default 100). Read per request:

- `status`, with `error` and `cause` when the proxy answered itself
  (`model_not_found`, `launch_failed`, `upstream_unreachable`). A status with a
  `null` `error` came from the model's own server, so read that launch's log.
- `state`: `in_flight`, `done`, `client_closed`, or `upstream_error`
- `auto_start`, `evicted`, `fallback`: whether the request started the model,
  which launches it unloaded to make room, and why another model answered
- `ttfb_ms`, `duration_ms`, `prompt_tokens`, `completion_tokens`,
  `tokens_per_second`

`summary` holds the totals for the same scope. The log is empty after a daemon
restart, and it has no prompt or response text.

## References

- `INSTALL.md#for-ai-agents`
- `README.md#cli-exit-codes`
- `docs/usage.md`
- `tests/proxy_real_client_smoke.md`
