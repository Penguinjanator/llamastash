# Lemonade backend setup (NPU / multi-engine)

LlamaStash can use [**Lemonade**](https://github.com/lemonade-sdk/lemonade)
(`lemond`) as a second backend. Lemonade is a long-lived umbrella server that
fans out to engines llama.cpp can't reach — most notably **NPU inference** on
AMD Ryzen AI / XDNA hardware, plus ROCm, ONNX, and others.

> **⚠️ Experimental.** The Lemonade backend is new and lightly road-tested.
> Expect rough edges; behaviour, config keys, and the discovery/routing
> surface may change without notice. llama.cpp remains the stable default.

It is **default-on when the `lemond` binary resolves** (mirroring vLLM): if
`lemond` is on your `PATH`, or `backend.lemonade.servers` points at it, LlamaStash
auto-enables the backend unless you set `backend.lemonade.enabled: false`. When no
`lemond` is found it stays completely dormant — no discovery, no umbrella.
llama.cpp stays the direct, zero-overhead default.

> **LlamaStash does not install Lemonade.** You set up Lemonade and its
> engines yourself; LlamaStash finds the `lemond` binary, supervises it, and
> routes to it. This keeps the install lean and avoids shipping AMD's NPU
> system stack.

## 1. Install Lemonade

Follow Lemonade's own instructions for your platform:
<https://github.com/lemonade-sdk/lemonade>.

Install the engines you want Lemonade to serve (e.g. its llama.cpp / ROCm /
ONNX / Ryzen-AI backends). For **NPU** inference on AMD Ryzen AI you must also
install AMD's NPU system stack — XRT, the NPU firmware, and `flm` — per AMD's
documentation. **LlamaStash does not install any of that**; it only talks to a
working `lemond`.

Verify Lemonade works on its own before wiring it into LlamaStash:

```sh
lemond            # starts the umbrella (default port 13305)
# in another shell:
curl http://127.0.0.1:13305/api/v1/models
```

Then **stop that manual `lemond`**. Once the backend is enabled, LlamaStash
spawns and supervises its own `lemond` on the configured port at daemon start —
leaving your own copy bound to the same port would block it.

## 2. Point LlamaStash at it

If `lemond` is already on your `PATH`, LlamaStash finds it and **auto-enables**
Lemonade — nothing to configure. Point at an off-PATH binary, force enablement
over a config `enabled: false`, or opt out entirely with:

- **Config** — in `config.yaml`:

  ```yaml
  backend:
    lemonade:
      # Tri-state, like vLLM: leave unset for the default (auto: on whenever
      # `lemond` resolves), `true` to force on, `false` to force off even when
      # the binary is present.
      # enabled: true
      # Optional: explicit *absolute* path to the lemond binary (one entry;
      # Lemonade runs a single umbrella). If omitted,
      # LlamaStash looks for `lemond` (or `lemonade`) on your PATH. lemond
      # keeps its config.json + model data in its own default cache dir
      # (`~/.cache/lemonade`), shared with any manual lemond runs.
      servers: [{ binary: /opt/lemonade/lemond }]
      # Optional: the loopback port lemond binds. Defaults to 13305.
      port: 13305
  ```

- **Daemon flag** — `llamastash daemon start --lemonade` (force on)
- **Env var** — `LLAMASTASH_LEMONADE=1` (force on)

Binary resolution: the first `backend.lemonade.servers` entry if set (and it exists),
otherwise `lemond` / `lemonade` on `PATH`. The same resolution drives the
`status` `installed` signal — an off-PATH `servers` binary still reads as
installed.

## 3. How LlamaStash uses it

- **Supervises** one shared `lemond` umbrella, brought up at daemon start on
  the configured loopback port (spawned from the resolved binary; it is *not*
  downloaded). The umbrella's `/live` endpoint is the readiness probe.
- **Discovers** Lemonade models from `lemond`'s `/api/v1/models` and lists
  them in the catalog tagged with the `lemonade` backend (list-only — pull
  models with Lemonade's own tooling).
- **Routes** inference for a Lemonade model through the OpenAI-compatible
  proxy to the umbrella (`/api/v1/...`), loopback-only.
- **Evicts** idle Lemonade models by unloading them from the umbrella
  (`/api/v1/unload`) rather than killing the shared process — it stays up and
  autoloads on the next request.

## 4. Verify

```sh
llamastash status        # the `backends` block lists `lemonade` (installed?, cpu+npu)
llamastash list          # Lemonade registry models appear, tagged `lemonade`
```

Send a chat request for a Lemonade model through the proxy and it routes to the
umbrella. In the TUI Launch picker, a Lemonade model shows only the knobs
lemond honors — `ctx` and the free-form extras (forwarded as the recipe's
`*_args`); the llama.cpp-specific knobs are hidden.

## 5. Troubleshooting

- **`503 backend_unavailable`** from the proxy — the umbrella isn't running.
  Confirm Lemonade is enabled, `lemond` is resolvable (PATH or
  `backend.lemonade.servers`), and start the daemon with `--lemonade`.
- **`status` shows `lemonade: not installed`** — neither a `backend.lemonade.servers` binary nor
  a `lemond` / `lemonade` on `PATH` resolved to a file. Add `lemond` to `PATH`
  or set `backend.lemonade.servers: [{ binary: <full path> }]`.
- **No NPU acceleration** — Lemonade falls back to CPU/GPU when AMD's NPU
  system stack (XRT / firmware / `flm`) isn't installed. Check Lemonade's own
  diagnostics; that stack is AMD's to install, not LlamaStash's.
- **`FLM NPU validation failed: Memlock limits are too low`** — `lemond`
  inherits the LlamaStash daemon's resource limits, and FLM needs to lock
  model memory for the NPU. Many non-login contexts (systemd user services,
  agent sandboxes) cap `memlock` at 8 MB even when
  `/etc/security/limits.conf` says `unlimited`. Check with `ulimit -l` in
  the shell that starts the daemon; start it from a real login shell, or
  raise `DefaultLimitMEMLOCK` for `user@.service` if you manage the daemon
  via systemd. NPU models surface the error on the model's `status` row and
  at request time; CPU/GPU recipes are unaffected.
