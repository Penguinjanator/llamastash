# R17 Batch 4: `proxy.aliases` visibility and coverage

Status: ready to implement. Tracked in `TODO.md` § R17 Batch 4.

## Goal

Three things an operator cannot do today:

1. Send a `proxy.aliases` name to a vLLM launch. The proxy forwards the client's
   `model` unchanged and vLLM answers only to its `--served-model-name` list, so
   the alias gets vLLM's own `404 NotFoundError`.
2. See the alias table without sending a request. Nothing in `list --json` shows
   the entries or which of them name no model. The only signal is one daemon log
   line per problem, on first use.
3. Trust three claims about the alias rows in `/v1/models` and `/api/tags` that
   have no test: the ambiguous-target refusal, the inherited `mode`, and that a
   listing does not spend the once-per-name warnings real requests need.

## Scope

In: `backend.vllm.rewrite_model`, an alias audit on `list --json`, a startup
validity pass, and the three missing listing tests.

Out (deliberate, do not add): `status --json` and `doctor` alias output, alias
edits over IPC, chains of aliases, alias changes without a daemon restart. The
read-only surface is `list --json`; `doctor` stays as it is.

## Decisions

**D1: reuse the request-model stamp, do not write a vLLM body rewrite.**
`src/proxy/forward.rs::outbound_body` already replaces `body.model` with the
launch's `launch_config[crate::backend::REQUEST_MODEL_KEY]` before forwarding,
and it is what a generic entry's `rewrite_model` uses. vLLM stamps the same key.
This keeps the proxy backend-agnostic.

**D2: the rewrite target is `served_model_name(model_path)`.**
`src/backend/vllm/mod.rs::served_model_aliases` registers that name first, so it
always answers. Every other client spelling (an alias, a partial, a case variant)
resolves to the launch on our side and becomes that name on the way out.

**D3: the knob is `backend.vllm.rewrite_model`, default `false`.**
Opt-in, like generic's `rewrite_model`, because it overrides what the client sent.
Seed it in `VllmBackend::seed_launch_knobs`, which re-derives per launch exactly
like the `cors` seed there, so a config flip takes effect on the next launch.
`vllm_argv` must not change: the flag list stays byte-identical to raw `vllm serve`.

**D4: one audit function, two readers.**
Add `AliasTable::audit(&self, cat: &[CatalogRow]) -> Vec<AliasIssue>` in
`src/proxy/alias.rs`, with `AliasIssue { name, target, state }` and
`state: ok | shadowed | dead | ambiguous | chain`. Decide each state the way a
request decides, so the listing, `list --json` and the log lines all agree about
what is broken.

Written 2026-10-23 as "the same calls `alias_rows` already makes (exact reference
only), no new resolution logic", and widened the same day in review: judging a
value only by the exact rung labelled it `dead` where a client sending that name
was handed `ambiguous_model` with two candidates, which sends the operator hunting
a typo instead of naming one of two models. So the ladder now lives in
`AliasTable::verdict` (exact reference, then chain, then the looser rungs that
decide *why* nothing answered) and is read by the audit *and* by
`src/proxy/route.rs::resolve_aliased_reference`, which used to carry its own copy.
`alias_rows` keeps publishing on the exact rung: that one answers what can route,
not what is wrong. A loose match on one row stays `dead`, because an alias may not
be answered with a guess.

**D5: the startup pass runs once, when the catalog first has a row.**
At daemon start the catalog is empty and every target would look dead. Wait for
the first non-empty catalog, then log every non-`ok` issue through the existing
`warn_shadowed` / `warn_ambiguous_target` / `warn_dead_target` /
`warn_points_at_alias` methods. They are once-per-name by key, so the pass cannot
double-log and a later request for the same alias stays quiet, which is fine: the
operator already has the line. Pick the smallest hook that fits (a short bounded
poll on `catalog.len()`, or a callback after the initial `full_rescan` in
`src/daemon/discovery_task.rs`). Do not build a general scan-notification system.
If the catalog never gets a row, nothing is logged.

**D6: `list --json` gains a top-level `aliases` array.**
Each entry `{name, target, state}`, config order, from a new read-only IPC method
that runs `AliasTable::audit` against the current catalog. The human `list` table
and `--json` byte-stability rules stay as they are, and `aliases` is `[]` when no
`proxy.aliases` is configured (key always present, like every other `--json` key).
To let the control plane read the table, build the `Arc<AliasTable>` once in
`src/daemon/mod.rs` from `opts.proxy.aliases`, put it on `MethodContext`, and pass
that same `Arc` to `ProxyState::from_context_with_auth` so the proxy and the new
method share one table and one warn set.

**D7: no new config for the listings.** `alias_rows` stays as it is; the tests in
U3 prove what it already does.

## Units

They share files (`docs/usage.md`, `CHANGELOG.md`), so they run in one session,
in this order.

### U1: vLLM served-name rewrite

- `src/backend/vllm/mod.rs`: `rewrite_model: bool` on `VllmConfig` (+ `Default`),
  stamp `REQUEST_MODEL_KEY` in `seed_launch_knobs` when it is on.
- `config.example.yaml`: the knob in the `vllm:` block, with what it is for.
- `docs/vllm-setup.md`: the line that says an alias cannot reach vLLM today.
- `docs/usage.md`: the `backend.vllm` block and, if it names the limit, the
  "aliases do not reach vLLM" sentence in the proxy model-id section.
- `RECIPES.md`: mention the knob where the vLLM block explains served names.
- Tests in `src/backend/vllm/mod.rs`: argv does not change either way, the stamp
  is present only when the knob is on, and the stamped value equals
  `served_model_name`. One test in `tests/vllm_backend_test.rs` that a launch with
  the knob on forwards `body.model` as the served name (use the existing fake
  server; no real vLLM on this machine).

### U2: alias audit surface

- `src/proxy/alias.rs`: `AliasIssue`, `AliasTable::audit`, unit tests for each
  state including `ok`, plus a test that `audit` does not consume the once-per-name
  warn set.
- `src/proxy/state.rs`, `src/daemon/context.rs`, `src/daemon/mod.rs`: shared
  `Arc<AliasTable>` per D6.
- Startup pass per D5 (new small function, called from the daemon).
- `src/ipc/methods.rs`: the read-only method (name it after what it returns, e.g.
  `proxy_aliases`), `docs/usage.md` IPC method list.
- `src/cli/output.rs` + `src/cli/list.rs`: `aliases` in `list --json`.
- `docs/usage.md`: the `list --json` sample plus a short note that this is where
  a dead alias shows up; `docs/architecture.md` if it describes the alias table.
- `CHANGELOG.md`: one line under `[Unreleased]`.
- Tests: `list --json` shape test in `src/cli/output.rs`, and an integration test
  (in `tests/`) that a configured-but-dead alias reads `dead` and a good one reads
  `ok`, and that the startup line appears once in the daemon log.

### U3: the three listing claims

- In `tests/proxy_models.rs` and `tests/proxy_ollama_compat.rs`, three cases each:
  an alias whose target two models answer to stays out of the listing; an alias row
  carries the `mode` of the model it names; reading the listing does not spend the
  once-per-name warning (after `GET /v1/models`, a real request for the same broken
  alias still gets its line).
- If a case shows a real bug, fix it in `src/proxy/router.rs` and say so in the
  commit body.

Strike the three `TODO.md` lines in the same commit as the code they describe.

## Verification

```bash
cargo check --features test-fixtures
cargo nextest run --features test-fixtures --lib vllm
cargo nextest run --features test-fixtures --lib proxy::alias
cargo nextest run --features test-fixtures --test vllm_backend_test --test proxy_models --test proxy_ollama_compat --test cli_integration_test
cargo nextest run --features test-fixtures   # full suite, once, at the end
make lint
```

Live smoke for the new surface, in an isolated state so no real daemon is touched
(never bare `llamastash`; build first):

```bash
cargo build --bin llamastash
export LLAMASTASH_STATE_DIR=/tmp/ls-alias-e2e/state LLAMASTASH_CONFIG_DIR=/tmp/ls-alias-e2e/cfg
mkdir -p "$LLAMASTASH_STATE_DIR" "$LLAMASTASH_CONFIG_DIR/models"
# one fixture GGUF in the scan root, and a proxy.aliases block with one live name
# and one name that matches nothing
./target/debug/llamastash daemon start --proxy-port 11446
./target/debug/llamastash list --json | jq '.aliases'
./target/debug/llamastash daemon logs 2>/dev/null | grep -c "proxy.aliases" || true
./target/debug/llamastash daemon stop
```

Expect: both entries listed, `ok` for the live one, `dead` for the other, and the
dead-target warning in the daemon log. Then full `cargo nextest run
--features test-fixtures` and `make lint` clean.

## Definition of done

All three `TODO.md` items struck with a what-changed note; docs,
`config.example.yaml` and `CHANGELOG.md` updated in the same commits as the code;
the commands above green; the live smoke output recorded in the result. No push,
no PR.
