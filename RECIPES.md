# Recipes

Working `config.yaml` blocks for engines and models. Each one names the engine version it was written for. Replace the `/path/to/...` parts with your own paths, then run `llamastash daemon restart`: the daemon reads `config.yaml` at start.

What each field means is in [`docs/usage.md`](docs/usage.md): § [Generic backend](docs/usage.md#generic-backend) for `backend.generic.servers`, § [`presets`](docs/usage.md#llamastash-presets-model-ref-action) for preset keys and precedence. [`config.example.yaml`](config.example.yaml) has every key with a comment.

| Model | Engine | Recipe |
|---|---|---|
| any GGUF | llama.cpp, several builds side by side | [llama.cpp builds](#llamacpp-builds) |
| safetensors repos | vLLM, SGLang | [vLLM and SGLang](#vllm-and-sglang) |
| Qwen3.8 Flash-Next | gufo, Halogen, CIRU | [Qwen3.8 Flash-Next](#qwen38-flash-next) |
| Qwen3.8 27B | gufo with a DFlash2 draft | [Qwen3.8 27B](#qwen38-27b) |
| DeepSeek V4 Flash / PRO | ds4, llama.cpp | [DeepSeek V4](#deepseek-v4) |
| GLM 4.x, Gemma 4 | llama.cpp with MTP | [MTP models on llama.cpp](#mtp-models-on-llamacpp-glm-gemma-4) |
| Qwen3.5 4B vision | llama.cpp on a 4 GB GPU | [Small GPU: Qwen3.5 4B vision](#small-gpu-qwen35-4b-vision) |
| any Lemonade model | Lemonade (`lemond`) | [Lemonade](#lemonade) |
| NPU LLMs, Whisper | FastFlowLM | [FastFlowLM (AMD NPU)](#fastflowlm-amd-npu) |
| Whisper | whisper.cpp | [whisper.cpp](#whispercpp) |

## llama.cpp builds

llama.cpp runs every GGUF by default. List each `llama-server` build you have; the first one is the default. LlamaStash runs each with `--list-devices` at daemon start and names it after its compute type (`llamacpp-rocm`, `llamacpp-vulkan`, `llamacpp-cuda`), or `llamacpp-<name>` when you set `name:`.

```yaml
backend:
  llamacpp:
    servers:
      - binary: /path/to/llama.cpp/build-hip/bin/llama-server      # llamacpp-rocm, the default
      - binary: /path/to/llama.cpp/build-vulkan/bin/llama-server   # llamacpp-vulkan
      - name: cpu                                                  # llamacpp-cpu: never opens the GPU
        binary: /path/to/llama.cpp/build-cpu/bin/llama-server
```

```bash
llamastash status                                  # the servers it probed
llamastash start <model> --server llamacpp-vulkan  # one launch on the Vulkan build
```

A preset pins a build with `server: llamacpp-vulkan`, so the model always launches on it. The CPU build is handy for a small helper model next to a big GPU model, since it leaves GPU memory alone.

A Vulkan build on a host that probes as AMD or NVIDIA works. It changes the binary, not the detected host, so `init`, host metrics and UAT preflight still report `amd` / `nvidia` while the launch runs the Vulkan build.

## vLLM and SGLang

Both serve safetensors HuggingFace repos; a GGUF still runs on llama.cpp. Install steps, the container route and distro caveats are in [vLLM setup](docs/vllm-setup.md) and [SGLang setup](docs/sglang-setup.md).

Point each backend at its launcher. LlamaStash only checks that the path exists, so a wrapper script around `docker run` works too. With no `servers` entry it looks for `vllm` / `sglang` on `$PATH`, and either backend is on whenever its launcher is found.

```yaml
backend:
  vllm:
    # enabled: false                    # keep it off while the path stays set
    servers:
      - binary: /home/you/.venvs/vllm/bin/vllm
    cors: false                         # vLLM allows every origin by default, and the proxy relays that
  sglang:
    servers:
      - binary: /home/you/.venvs/sglang/bin/sglang
```

With both installed, a repo lists both backends and `auto` picks vLLM. Pick SGLang with `start <repo> --backend sglang` or a preset's `backend:` line, as below.

On a unified-memory host (an APU such as Strix Halo or DGX Spark) LlamaStash caps the KV cache itself when you leave the memory knobs unset. To pick the size yourself, set one in a preset; either knob turns the automatic cap off. Neither is remembered for the next plain `start`.

```yaml
presets:
  Qwen/Qwen2.5-0.5B-Instruct:        # a safetensors row is named by its repo id
    entries:
      vllm-16g:
        backend: vllm
        knobs:
          kv-cache-memory-bytes: 16G   # absolute cap; wins over the fraction
          # gpu-memory-utilization: 0.5  # or a fraction of the whole pool
      sglang-128k:
        backend: sglang
        knobs:
          max-total-tokens: 131072     # SGLang caps in tokens, not bytes
          # mem-fraction-static: 0.5
```

```bash
llamastash start Qwen/Qwen2.5-0.5B-Instruct --preset vllm-16g
```

## Qwen3.8 Flash-Next

Three engines besides llama.cpp: gufo (commit `d9a84f1`), Halogen (`0.17.0` Docker image) and CIRU (commit `3cf984c`). Each takes 80-100 GB, so run one at a time.

### gufo

A native binary that runs a catalog GGUF directly, so it uses `model` and needs no wrapper. It checks the request's `model` field, hence `--served-model-name "{name}"`, and `rewrite_model` sets that field so a request for the plain model id still reaches a named launch. Loads in about 22 s; `/ready` returns 503 until then.

```yaml
backend:
  generic:
    servers:
      - name: gufo                                  # server id: generic-gufo
        model: Qwen3.8-Flash-Next-UD-Q4_K_XL        # a model id; globs work too
        binary: /path/to/gufo/build/release/gufo
        args: [serve, --host, "{host}", --port, "{port}", --sessions, "1", llm,
               --served-model-name, "{name}", --model, "{model}",
               --mtp-model, /path/to/MTP/mtp-Qwen3.8-Flash-Next-shared-Q8_0.gguf,
               --top-p, "0.95", --top-k, "20"]
        knobs:
          - {flag: --context, ctx: true, default: "131072"}
          - {flag: --speculative, default: mtp}
          - {flag: --temperature, default: "1.0"}
          - --seed
        ready: /ready
        stop_grace_secs: 60
        rewrite_model: true

presets:
  Qwen3.8-Flash-Next-*:
    entries:
      gufo-128k:
        server: generic-gufo       # the preset picks the engine
        knobs:
          context: 131072
          speculative: mtp
          temperature: '1.0'
      gufo-256k:
        server: generic-gufo
        knobs:
          context: 262144
```

```bash
llamastash start Qwen3.8-Flash-Next-UD-Q4_K_XL --preset gufo-256k
llamastash start Qwen3.8-Flash-Next-UD-Q4_K_XL --server generic-gufo -- --seed 7
```

llama.cpp stays the default server for the GGUF. Add `default: gufo-128k` under the preset key to run gufo on a plain `start` and on proxy requests too.

### Halogen

A Docker image configured by `HALOGEN_*` env vars, with its own `.hgn` weights (not a catalog GGUF), so it is its own row. Its knobs feed the container through `env`. `halogen-pool` sets `HALOGEN_KV_POOL_POSITIONS`, the positions kept resident across all conversations: a pool the size of the window holds one full-length conversation, twice the window holds two. `reasoning_effort` lists the levels its chat template accepts, so `integrations` gives the row effort levels in pi, OpenCode, Zed and Codex. Halogen also takes `minimal`, `high` and (from 0.15.1) `max` as aliases of those levels, and `none` turns thinking off. A preset that sets `halogen-effort` changes what the server uses when a request sends no effort, but not `reasoning_effort_default`, so Codex and Zed still write `xhigh` for it. Leave `vision` off unless the wrapper sets `HALOGEN_VISION_TOWER`; without it Halogen refuses images with a `400`.

```yaml
backend:
  generic:
    servers:
      - name: flash-next-halogen
        binary: ~/bin/halogen-serve.sh
        args: ["{port}"]
        arch: qwen4exp                       # row info: arch and params as on the model's GGUF row
        params: 177B
        quant: v2-4.16bpw                    # the v2 checkpoint's average bits per weight
        ctx: 262144
        reasoning_effort: [none, low, medium, xhigh]
        reasoning_effort_default: xhigh      # same as halogen-effort's default
        memory_gib: 89                       # startup log at a 524288 pool: 89.1 GiB in all
        knobs:
          - {flag: --ctx-window, id: halogen-ctx, ctx: true, default: "131072"}
          - {flag: --halogen-kv-pool, id: halogen-pool, default: "131072"}
          - {flag: --halogen-temperature, id: halogen-temp, default: "1.0"}
          - {flag: --halogen-mtp-depth, id: halogen-mtp-depth, default: "3"}
          - {flag: --halogen-reasoning-effort, id: halogen-effort, default: xhigh}
        env:
          HALOGEN_MODEL_ID: "{name}"
          HALOGEN_CTX: "{halogen-ctx}"
          HALOGEN_KV_POOL_POSITIONS: "{halogen-pool}"
          HALOGEN_TEMPERATURE: "{halogen-temp}"
          HALOGEN_MTP_DEPTH: "{halogen-mtp-depth}"
          HALOGEN_REASONING_EFFORT: "{halogen-effort}"
        ready: /v1/models
        stop_grace_secs: 90
        ready_timeout_secs: 600

presets:
  flash-next-halogen:
    entries:
      dual-256k:                   # two full-length conversations stay resident
        knobs:
          halogen-ctx: 262144
          halogen-pool: 524288
      halogen-medium:
        knobs:
          halogen-effort: medium   # shorter thinking, faster turns
```

```bash
llamastash start flash-next-halogen --preset dual-256k
llamastash start flash-next-halogen --preset halogen-medium
```

`~/bin/halogen-serve.sh`:

```sh
#!/bin/sh
# llamastash generic wrapper for Halogen 0.17.0 (v2 .hgn checkpoint).
# $1 = port; HALOGEN_* come from the entry env.
# The server is closed source, so it runs on an internal network with no
# outbound access. Docker can't publish ports from one, so socat forwards
# loopback to the container; pdeathsig stops socat if this script dies.
port="$1"
name="llamastash-halogen-$port"          # per port, so two launches don't collide
net=halogen-net
docker rm -f "$name" >/dev/null 2>&1      # leftover from a crashed daemon
# A stopped --rm container keeps its name until removal finishes (33 s seen),
# and rm -f returns early while that runs.
for _ in $(seq 120); do docker container inspect "$name" >/dev/null 2>&1 || break; sleep 1; done
docker network inspect "$net" >/dev/null 2>&1 || docker network create --internal "$net" >/dev/null || exit 1
hub=/path/to/huggingface/hub
repo=models--peonist-ai--halogen-qwen3.8-flash-next
hg=/hub/$repo/snapshots/$(cat "$hub/$repo/refs/main") || exit 1   # the downloaded revision
docker run -d --rm --name "$name" --network "$net" \
  --device /dev/kfd --device /dev/dri \
  --group-add "$(getent group video | cut -d: -f3)" --group-add "$(getent group render | cut -d: -f3)" \
  --ipc=host --ulimit memlock=-1:-1 -v "$hub":/hub:ro \
  -e HALOGEN_API_PORT=8080 -e HALOGEN_MODEL_ID \
  -e HALOGEN_CHECKPOINT="$hg/qwen38-flash-next-v2.hgn" \
  -e HALOGEN_TOKENIZER="$hg/tokenizer" \
  -e HALOGEN_CTX -e HALOGEN_KV_POOL_POSITIONS -e HALOGEN_MTP_DEPTH -e HALOGEN_REASONING_EFFORT \
  -e HALOGEN_MAX_TOKENS_DEFAULT=16384 -e HALOGEN_TEMPERATURE -e HALOGEN_TOP_P=0.95 -e HALOGEN_TOP_K=20 \
  ghcr.io/peonist-ai/halogen-flash-server:0.17.0 >/dev/null || exit 1
# One clean stop: SIGTERM from llamastash becomes `docker stop`, which the
# image turns into an engine shutdown (its own SIGKILL comes 30 s later).
trap 'docker stop -t 60 "$name" >/dev/null 2>&1' TERM INT
docker logs -f "$name" 2>&1 &
# Forward only once the API listens; earlier, socat logs every readiness probe as refused.
ip=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$net\").IPAddress}}" "$name")
until curl -so /dev/null "http://$ip:8080/v1/models"; do
  [ "$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" = true ] || exit 1
  sleep 2
done
setpriv --pdeathsig TERM socat TCP-LISTEN:"$port",bind=127.0.0.1,reuseaddr,fork TCP:"$ip":8080 &
fwd=$!
docker wait "$name" >/dev/null &
wait $!
kill "$fwd" 2>/dev/null
```

- `docker run -d` plus `docker wait` keeps the wrapper in the foreground while the container never sees the process-group SIGTERM directly, so the engine gets exactly one signal. Keep `docker stop -t` above the image's 30 s and `stop_grace_secs` above `-t`.
- The loop after `docker rm -f` covers a relaunch on the same port right after a stop. A stopped `--rm` container keeps its name until Docker finishes removing it, which can take over 30 s. `docker rm -f` returns at once during that time (`removal of container ... is already in progress`), and `docker run` then fails with `Conflict. The container name ... is already in use`.
- The `--internal` network gives the container no outbound access. Docker does not publish ports for a container on an internal network, so `socat` forwards `127.0.0.1:$port` to it. The host needs `socat`, `setpriv` and `curl`. Without the isolation, publish the port instead (`-p "127.0.0.1:$port:8080"`, no `socat`), as this example did for 0.14.0. The API binds `0.0.0.0:8080` inside the container, so keep it off `--network host`.
- 0.17.0 serves the v2 checkpoint, `qwen38-flash-next-v2.hgn` (62.1 GiB), and finds its lookup table `qwen38-flash-next-ngram.hgn` (47.7 GiB) beside it. The draft head is inside the checkpoint, so there is no `HALOGEN_MTP_HEAD`. The older w4b checkpoint still loads but is deprecated.
- It does not check the request's `model` field. The engine was listening 88 s after start when it read the checkpoint from disk, and after 6 to 8 s with the weights still in page cache.
- `HALOGEN_REASONING_EFFORT` sets the default thinking effort: `minimal`, `low`, `medium`, `high` or `xhigh`, mapped to the template's `low`, `medium` and `xhigh`. Unset, the template's own default applies, which is `xhigh`. A request that sends `reasoning_effort` still wins. The wrapper has to pass the variable through (`-e HALOGEN_REASONING_EFFORT`), or the preset has no effect.

#### Halogen for agent harnesses (several conversations at once)

A harness that fans out subagents keeps a parent conversation and several child conversations alive at once. Newer Halogen images have variables for that. The names below match Halogen's [FLAGS.md](https://github.com/peonist-ai/halogen-flash-server/blob/main/docs/FLAGS.md) at 0.18.10, newer than the 0.17.0 image above, so bump the image tag in the wrapper. Add the knobs and env lines to the entry above, and the same `-e HALOGEN_...` names to the wrapper's `docker run`:

```yaml
        knobs:
          # ...the five knobs above, plus:
          - {flag: --halogen-slots, id: halogen-slots, default: "4"}                    # conversations generating at once
          - {flag: --halogen-max-tok, id: halogen-max-tok, default: "32768"}            # largest single prefill call
          - {flag: --halogen-cache-entries, id: halogen-cache-entries, default: "24"}   # prompt-cache entries
          - {flag: --halogen-cache-branches, id: halogen-cache-branches, default: "2"}  # resume points per conversation
        env:
          # ...the six lines above, plus:
          HALOGEN_KV_SLOTS: "{halogen-slots}"
          HALOGEN_MAX_TOK: "{halogen-max-tok}"
          HALOGEN_CACHE_ENTRIES: "{halogen-cache-entries}"
          HALOGEN_CACHE_BRANCHES: "{halogen-cache-branches}"

presets:
  flash-next-halogen:
    default: multi-agent
    entries:
      multi-agent:                 # a 256k parent plus three 128k subagents
        knobs:
          halogen-ctx: 262144
          halogen-pool: 786432
          halogen-slots: 6
          halogen-max-tok: 32768
          halogen-cache-entries: 56
          halogen-cache-branches: 3
      multi-agent-max:             # a 256k parent plus four 128k subagents
        knobs:
          halogen-ctx: 262144
          halogen-pool: 917504
          halogen-slots: 8
          halogen-max-tok: 16384   # halves the ~9 GB prefill scratch to pay for the bigger pool
          halogen-cache-entries: 64
          halogen-cache-branches: 3

proxy:
  aliases:                         # names a harness can give its parent and subagent models
    flash-next-256k: flash-next-halogen
    flash-next-128k: flash-next-halogen
```

- Each request reserves its prompt plus `max_tokens` positions in the pool when it is admitted and waits when they are not free, so size `halogen-pool` for the conversations you expect at once. Slots cost about 115 MB each; four streams make about 76 t/s in total and eight about 85, and past eight the total stops growing (Halogen's own numbers).
- `halogen-cache-branches: 3` keeps resume points for a parent forked into two children. On `2`, the children's entries replace the parent's resume point, so the parent reads its history again on its next turn.
- An alias names a model, so all the aliases above reach the one running Halogen launch.
- Optional wrapper lines: `-e HALOGEN_CACHE_DIR=/cache` with a mounted directory keeps the prompt cache across restarts (needs a filesystem that accepts direct I/O, so not tmpfs), and `-e HALOGEN_FORK=1 -e HALOGEN_FORK_MAP=auto` starts each next turn from where the reply ended instead of reading the reply back in (0.18.3+). `-e HALOGEN_VISION_TOWER=1` loads `qwen38-flash-next-vision.hgn` from beside the checkpoint; add `vision: true` to the entry with it.

### CIRU

A `run-server.sh` launcher configured by env vars that `exec`s its own llama-server build and passes extra args through. The wrapper exports the fixed variables; per-launch values go in the entry's `env`.

```yaml
backend:
  generic:
    servers:
      - name: flash-next-ciru
        binary: ~/bin/ciru-serve.sh
        knobs:
          - {flag: --ciru-ctx, id: ciru-ctx, ctx: true, default: "65536"}
          - {flag: --mtp-depth, id: mtp-depth, default: "3"}
        env:
          PORT: "{port}"
          CONTEXT_SIZE: "{ciru-ctx}"
          MTP_DEPTH: "{mtp-depth}"
        ready: /health
        stop_grace_secs: 60
        ready_timeout_secs: 600
```

`~/bin/ciru-serve.sh`:

```sh
#!/bin/sh
# llamastash generic wrapper for CIRU (3cf984c). Env comes from the entry;
# extra args pass through to llama-server.
export ROCM_ROOT=/path/to/ciru/.venv-rocm/lib/python3.12/site-packages/_rocm_sdk_devel
export CIRU_RUNTIME_ROOT=/path/to/ciru/runtime
export MODEL_DIR=/path/to/huggingface/hub/models--jcbtc--Qwen3.8-Flash-CIRU-STRIX-IU4/snapshots/<revision>
export ENABLE_UI=0 HOST=127.0.0.1
exec /path/to/ciru/scripts/ciru/run-server.sh "$@"
```

- `exec` hands the PID to llama-server, so llamastash's one SIGTERM reaches the engine directly.
- CIRU answers under its own alias (`Qwen3.8-Flash-CIRU-STRIX-IU4`) and does not check the request's `model` field. Loads in about 115 s.

## Qwen3.8 27B

A ROCm build of gufo runs the 27B GGUF with z-lab's DFlash2 draft model. Without `default:` in the preset, llama.cpp stays the default server for this GGUF.

```yaml
backend:
  generic:
    servers:
      - name: gufo-27b                              # server id: generic-gufo-27b
        model: Qwen3.8-27B-UD-Q6_K
        binary: /path/to/gufo/build/release/gufo
        args: [serve, --host, "{host}", --port, "{port}", --sessions, "1", llm,
               --served-model-name, "{name}", --model, "{model}",
               --speculative, dflash2,
               --dflash-model, /path/to/Qwen3.8-27B-DFlash2-Q4_K_M.gguf,
               --top-p, "0.95", --top-k, "20"]
        knobs:
          - {flag: --context, ctx: true, default: "131072"}
          - {flag: --temperature, default: "1.0"}
          - --seed
        ready: /ready
        stop_grace_secs: 60
        rewrite_model: true

presets:
  Qwen3.8-27B-UD-Q6_K.gguf:
    entries:
      gufo-256k:
        server: generic-gufo-27b
        knobs:
          context: 262144
```

## DeepSeek V4

[ds4](https://github.com/antirez/ds4) (antirez's DwarfStar) runs the DeepSeek-V4 Flash/PRO GGUFs from [huggingface.co/antirez/deepseek-v4-gguf](https://huggingface.co/antirez/deepseek-v4-gguf) through its `ds4-server` binary. It was a dedicated backend up to 0.4.0. It's now a generic server entry, which gives the same argv, readiness and speed (13.2 t/s both ways with ds4 `0aaea5a` and the Flash IQ2_XXS `0731` GGUF). You build `ds4-server` yourself (`git clone https://github.com/antirez/ds4 && cd ds4 && make`). llama.cpp runs these GGUFs too, from **b9840** on, and stays their default server.

```yaml
backend:
  generic:
    servers:
      - name: ds4                                   # server id: generic-ds4
        model: "DeepSeek-V4-*"
        binary: /path/to/ds4/ds4-server
        args: [-m, "{model}", --host, "{host}", --port, "{port}"]
        ready: /v1/models            # ds4-server binds its port only after the load
        knobs:
          - {flag: --ctx, id: ds4-ctx, ctx: true}
          - --power
          - --tokens
          - {flag: --threads, id: ds4-threads}
          - --kv-disk-dir
          - --kv-disk-space-mb
          - {flag: --ssd-streaming, switch: true}
          - --ssd-streaming-cache-experts
          - --ssd-streaming-preload-experts
          - {flag: --ssd-streaming-cold, switch: true}
          - {flag: --warm-weights, switch: true}
          - {flag: --quality, switch: true}
          - {flag: --mtp, id: mtp-model}
          - {flag: --mtp-draft, id: ds4-mtp-draft}
          - --mtp-margin
          - {flag: --dspark, switch: true}
          - --dspark-confidence
          - {flag: --dspark-strict, switch: true}

presets:
  DeepSeek-V4-Flash-*-0731.gguf:
    default: ds4-mtp
    entries:
      ds4:
        server: generic-ds4
      ds4-mtp:
        server: generic-ds4
        knobs:
          mtp-model: /path/to/DeepSeek-V4-Flash-MTP-Q4K-Q8_0-F32.gguf
      ds4-dspark:
        server: generic-ds4
        knobs:
          dspark: true
          mtp-model: /path/to/DeepSeek-V4-Flash-DSpark-support-0731.gguf
```

```bash
llamastash start DeepSeek-V4-Flash-IQ2XXS-w2Q2K-AProjQ8-SExpQ8-OutQ8-chat-v2-imatrix-0731 --ctx 32768
llamastash start DeepSeek-V4-Flash-IQ2XXS-w2Q2K-AProjQ8-SExpQ8-OutQ8-chat-v2-imatrix-0731 --preset ds4-dspark
```

The knob ids match the old backend's, so presets keep working. The exceptions are `ds4-ctx`, `ds4-threads` and `ds4-mtp-draft`, because `ctx`, `threads` and `mtp-draft-n` are built-in llama.cpp ids. `--ctx` still sets the context window. Any other `ds4-server` flag goes in the extras tail after `--`. `ds4-dspark` needs both `dspark` and `mtp-model`; read the caveats in [DSpark speculative decoding](docs/usage.md#dspark-speculative-decoding) before you rely on it.

**Migrating from the ds4 backend.** A config that still has `backend.ds4:` is rejected at load, and the error points here.

1. Stop every running ds4 model (`llamastash stop <model>`) **before** you upgrade. The new daemon can't re-adopt a `ds4-server` the old backend started: it keeps running and holds its port and memory, but `status` doesn't list it and `stop` can't reach it. If one is left over, find it with `pgrep -a ds4-server` and `kill` its PID.
2. Delete the `backend.ds4:` block, and drop `--ds4` / `LLAMASTASH_DS4` from your scripts.
3. Add the `generic` entry above, with `binary` set to your `ds4-server`.
4. In your ds4 presets, replace `backend: ds4` with `server: generic-ds4`. llama.cpp is now the default server for these GGUFs, so a preset or `--server generic-ds4` picks ds4. A leftover `backend: ds4` with no `server:` doesn't fail: it runs the model on llama.cpp. The same goes for the remembered last launch of a ds4 model, until you launch it once with `--server generic-ds4`.
5. Set `mtp-model` in a preset to pair a draft head. The old backend found the MTP or DSpark file next to the model on its own; a generic entry doesn't.
6. Run `llamastash daemon restart`.

What the dedicated backend did that the generic entry doesn't:

- **No automatic routing or fallback.** A DeepSeek-V4 GGUF defaults to llama.cpp. llama.cpp runs these files from **b9840** on ([ggml-org/llama.cpp#24162](https://github.com/ggml-org/llama.cpp/pull/24162)); an older `llama-server` fails with `unknown model architecture: 'deepseek4'`.
- **No automatic SSD streaming.** Set `ssd-streaming: true` yourself on a box below the memory floor (about 128 GB on CUDA/ROCm, 96 GB on Metal). The admission check still sizes the launch from the GGUF, so a streaming launch that doesn't fit needs `start --force`.
- **No MTP and streaming reconciliation.** `ds4-server` refuses `--ssd-streaming` together with `--mtp` after the full model load, so don't set both.
- **No split-half guard.** The PRO `*-Layers00-30.gguf` / `*-Layers-31-output.gguf` halves are for ds4's distributed mode; launch a single-file GGUF instead.

Two `ds4-server` behaviors to know about. Its `/v1/models` always lists both `deepseek-v4-flash` and `deepseek-v4-pro`, whatever is loaded; chat requests echo back the `model` you send, so no `rewrite_model` is needed. And `--kv-disk-dir` is ds4's own persistent cache: it holds conversation data under ds4's permissions at exactly the path you give, so point it at a private directory.

## MTP models on llama.cpp (GLM, Gemma 4)

LlamaStash turns MTP on by itself on llama.cpp for a model with a draft head: an embedded one (GLM 4.x, Qwen3.5/3.6, DeepSeek) or a separate `mtp-*.gguf` next to the model (Gemma 4; `llamastash pull` fetches it with the model). See [MTP speculative decoding](docs/usage.md#mtp-speculative-decoding). Presets let you turn it off or tune the draft length per launch, which is worth measuring: on some models speculation costs more than it saves.

```yaml
presets:
  glm4moe:                   # arch key: every GLM 4.x MoE GGUF
    entries:
      no-mtp:
        knobs:
          mtp: off
  gemma-4-31B-*:             # glob key: every quant of Gemma 4 31B
    entries:
      no-mtp:
        knobs:
          mtp: off
      draft-5:
        knobs:
          mtp-draft-n: 5     # tokens drafted per step
```

```bash
llamastash start <gemma-4-31B model> --preset no-mtp
```

A key that names no model is read as a GGUF architecture id, so `glm4moe` covers every GGUF converted from a `Glm4MoeForCausalLM` checkpoint (the arch id comes from llama.cpp's converter, `conversion/glm.py`). The `Arch` column of `llamastash list` shows the id for your files. The GLM block has not been run on a GLM model yet.

## Small GPU: Qwen3.5 4B vision

Screenshot reading on a 4 GB laptop GPU (RTX 3050 Ti Laptop, CUDA build of llama.cpp). The knob values are tuned for that card.

```yaml
presets:
  Qwen3.5-4B-Q4_K_S.gguf:
    default: vision
    entries:
      vision:
        idle_ttl_secs: 600              # unload after 10 idle minutes (proxy auto-starts only)
        knobs:
          ctx-size: 4096                # what fits beside the weights
          n-gpu-layers: 99
          cache-type-k: q8_0            # K only: a quantized V cache needs flash-attn
          flash-attn: false             # flash-attn corrupts Qwen3.5 image embeddings
          parallel: 1
        extras: ["--no-mmproj-offload"] # the vision encoder wants ~4.6 GB, so it stays on the CPU
```

## Other engines

### Lemonade

[Lemonade](https://github.com/lemonade-sdk/lemonade) runs one long-lived `lemond` that serves every model it has downloaded and loads each one on the first request that names it. Written for `lemond` 2026.42.1 with its `llamacpp:rocm` backend (b11395) and `unsloth/Qwen3-0.6B-GGUF`.

```yaml
backend:
  generic:
    servers:
      - name: lemonade                              # server id: generic-lemonade
        binary: /usr/bin/lemond
        args: ["--port", "{port}", "--host", "{host}", "--no-broadcast",
               "--log-file", "disabled"]
        ready: /live
        ready_timeout_secs: 90
        stop_grace_secs: 20                         # lemond needs ~6 s to exit on SIGTERM
        memory_gib: 1

proxy:
  aliases:
    Qwen3-0.6B-GGUF: lemonade                      # one alias per Lemonade model you name
```

`lemond` picks its own port for nothing and takes the one llamastash reserves, so it publishes one row (`lemonade`) no client can send as `body.model`. The alias is what clients use, and the alias string survives into the forwarded body, which is how `lemond` finds the model: `curl :11435/v1/chat/completions -d '{"model":"Qwen3-0.6B-GGUF",...}'` came back in 1.9 s (cold, including Lemonade's own model load) at 103 t/s, and auto-start through the alias works. `GET http://127.0.0.1:13305/api/v1/models?show_all=true` lists every name the engine will accept (179 on 2026.42.1). `stop_grace_secs` is not decoration: `lemond` takes about 6 s to exit, over the 5 s default grace.

What one entry does not do:

- **The model list is not the catalog.** LlamaStash shows one `lemonade` row, not Lemonade's models. Each name a client should use needs its own `proxy.aliases` line, and nothing warns you when an alias names a model Lemonade does not have.
- **One launch, one umbrella, every model inside it.** `stop` takes the umbrella down (about 4.7 s), not one model. Two launched models means two `lemond` processes at about 38 MB each idle, and no shared residency.
- **No context control.** `lemond` takes context as the `ctx_size` field of its `POST /api/v1/load`, which argv cannot reach. You get whatever `~/.config/lemonade/config.json` says, and `--ctx` on a generic entry does nothing here.
- **`lemond`'s own children show up as `external`.** The `llama-server` it spawns appears in `status` as an unmanaged row.

### FastFlowLM (AMD NPU)

[FastFlowLM](https://github.com/ROCm/FastFlowLM) (`flm`) runs LLMs, Whisper ASR and embedding models on AMD XDNA2 NPUs. It speaks the OpenAI shape, so it needs no dedicated backend. Written for `flm` v1.0.9 (portable `fastflowlm_1.0.9_linux.tar.gz`) on Linux 7.2.10 with the in-tree `amdxdna` driver, NPU firmware 1.1.2.65 and `xrt` + `xrt-plugin-amdxdna`. `flm validate` should pass before you add the entry.

```yaml
backend:
  generic:
    servers:
      - name: "qwen3:0.6b"                          # the name must be flm's own model tag
        binary: ~/.local/opt/fastflowlm/flm
        args: [serve, "{name}", --host, "{host}", -p, "{port}", --cors, "0"]
        ready: /v1/version          # flm serves no /health; it loads before it binds
        ready_timeout_secs: 180
        stop_grace_secs: 20
        memory_gib: 2
        arch: qwen3
        params: 0.6B
        quant: q4nx
        ctx: 32768
        modes: [chat]
        knobs:
          - {flag: --ctx-len, id: ctx-len, ctx: true, default: "32768"}
          - {flag: --pmode, default: performance}    # powersaver | balanced | performance | turbo
```

The `name` is the load-bearing line. FLM switches models on the `model` field of a request and the proxy forwards that field unchanged, so the row id has to be the engine's tag: an entry called `flm-qwen3-06b` got `{"error":"Failed to load flm-qwen3-06b model!"}`, and naming it `qwen3:0.6b` (with `{name}` in `args`, so the tag is written once) works. Verified: 82.9 t/s decode for `qwen3:0.6b` on the NPU through the proxy, 0.66 s round trip, `--ctx 8192` reaching the child as `--ctx-len 8192` through the `ctx: true` knob.

To carry every tag you have downloaded rather than one, mint the rows from FLM's own store and drop the per-model blocks:

```yaml
backend:
  generic:
    servers:
      - name: flm
        binary: ~/.local/opt/fastflowlm/flm
        args: [serve, "{name}", --host, "{host}", -p, "{port}", --cors, "0"]
        ready: /v1/version
        ready_timeout_secs: 180
        stop_grace_secs: 20
        modes: [chat]
        rows_from:
          command: [~/.local/opt/fastflowlm/flm, list, --filter, installed, --json]
          items: models
          name: model
          when: {path: label, contains: reasoning}   # reasoning LLMs; see the note below
          arch: details.family
          params: details.parameter_size
          quant: details.quantization_level
          ctx: default_context_length
          memory_gib: footprint
        knobs:
          - {flag: --ctx-len, id: ctx-len, ctx: true}
          - {flag: --pmode, default: performance}    # powersaver | balanced | performance | turbo
```

After `llamastash daemon restart` the catalog lists `qwen3:0.6b` and every other installed tag FLM labels `reasoning`, each with its own arch, params, quant, context and footprint, and each one starts and answers by name with no `proxy.aliases` line. The `--ctx-len` knob declares no default, so each model starts with the context FLM's listing gives it (`default_context_length`, 32768 for `qwen3:0.6b`), and `--ctx` reaches FLM because that knob carries `ctx: true`; drop the knob and `--ctx` is accepted but goes nowhere (the launch's `params.ctx` never changes), the minted context is only a display value. Pick the filter by what FLM actually labels: on v1.0.9 the reasoning filter keeps the reasoning models and drops audio, but it also drops every LLM FLM labels something else or nothing at all (on v1.0.9: gemma3:4b, qwen3-it:4b, qwen3vl-it:4b and 6 more labelled tags, plus gemma3:1b, llama3.1, llama3.2, lfm2, phi4-mini and qwen2.5-it which carry no label), while `when: installed` keeps everything installed including whisper. There is no "not" filter, so a filter that means "LLMs only" has to name what you want. See [Minting rows from an engine's own listing](docs/usage.md#minting-rows-from-an-engines-own-listing).

ASR and embedding are separate processes, one NPU model per type: `flm serve --asr 1` (or `-e 1`) takes no model tag and answers `POST /v1/audio/transcriptions`. That route is not one the llamastash proxy forwards (see the caveat in [Proxy routes](docs/usage.md#proxy-openai-compatible-listener)), so an ASR entry is supervised, ready-polled and stopped by llamastash but its clients post to the launch port from `llamastash status` directly. FLM also answers the Ollama shapes (`/api/tags`, `/api/chat`, `/api/generate`, `/api/ps`, `/api/npu/status`) on the same port.

Three FLM notes: `/v1/models` lists FLM's whole model list, not what you downloaded; a request for a tag you do not have makes FLM try to download it; and its speed lands in `usage.decoding_speed_tps` and `usage.prefill_speed_tps`, which the request log reads, so `llamastash requests` shows FLM's own numbers rather than an estimate.

### whisper.cpp

`whisper-server` (whisper.cpp v1.8.4, from `lemonade backends install whispercpp:cpu`) serves two routes: `GET /health` and `POST /inference` (multipart `file=@clip.wav`, replies `{"text": ...}`). It has no `/v1/models`. Two flags rename its own routes onto the OpenAI transcription shape: `--request-path /v1/audio --inference-path /transcriptions` answers `POST /v1/audio/transcriptions` with 200 and `{"text": ...}`, and the plain `/inference` then 404s. The llamastash proxy does not forward that route yet (see [Proxy routes](docs/usage.md#proxy-openai-compatible-listener)), so an ASR client posts to the launch port from `llamastash status`.

```yaml
backend:
  generic:
    servers:
      - name: whisper-tiny
        binary: ~/.cache/lemonade/bin/whispercpp/cpu/whisper-server
        args: [-m, /path/to/huggingface/hub/models--ggerganov--whisper.cpp/snapshots/<rev>/ggml-tiny.bin,
               --host, "{host}", --port, "{port}",
               --request-path, /v1/audio, --inference-path, /transcriptions]
        env:
          LD_LIBRARY_PATH: ~/.cache/lemonade/bin/whispercpp/cpu   # the binary's own dir
        ready: /v1/audio/health
        memory_gib: 1
```

`ready` moves with the request path: `--request-path /v1/audio` puts health at `/v1/audio/health` and `/health` answers 404. Two gotchas: the binary needs `LD_LIBRARY_PATH` set to its own directory or it dies with `libwhisper.so.1: cannot open shared object file` (exit 127), and a leading `~/` in an `env` value resolves to your home directory the way `binary` does (the rest of the value is passed through, so a `~` in the middle of a path list is literal). Its weights are `ggml-*.bin`, which the model scanner does not list, so the path lives in the entry.
