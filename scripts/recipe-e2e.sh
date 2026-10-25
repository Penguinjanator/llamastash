#!/usr/bin/env bash
# End-to-end check of a RECIPES.md block: real engine, real model, real request.
#
# usage: scripts/recipe-e2e.sh [--list] [--keep] [--port-base N] <case> [<case> ...]
#
# A case writes the recipe's config block, with this machine's real paths, into a
# throwaway config dir, starts a daemon on its own state dir and proxy port (the
# user's daemon is never touched), launches the engine, sends one real request
# through the proxy, then tears it all down. The engine child's argv is captured,
# so a knob that never reached the engine shows up in the row.
#
# A case whose engine binary or weights are not on this machine records SKIP and
# the missing path, so one run reports the whole coverage picture.
#
# Results: one TSV row per case in $RECIPE_E2E_ROOT/results.tsv (default
# ~/.cache/llamastash-recipe-e2e). Engine and daemon logs stay in the case dir.
# Exit 0 when no case FAILed.

set -uo pipefail

REPO_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${RECIPE_E2E_BIN:-$REPO_DIR/target/debug/llamastash}
ROOT=${RECIPE_E2E_ROOT:-$HOME/.cache/llamastash-recipe-e2e}
RESULTS=$ROOT/results.tsv
PORT=${RECIPE_E2E_PORT_BASE:-14100}
WAIT_TIMEOUT=${RECIPE_E2E_WAIT:-600}
KEEP=0

HOST=$(cat /proc/sys/kernel/hostname)
LLAMA_DIR=/mnt/work/Workspace/llms/llama.cpp
LLMS=/mnt/work/Workspace/llms

# Per-machine paths and the models that exist here.
if [[ $HOST == deepu-flowz13* ]]; then
  HF_HOME=${HF_HOME:-/mnt/work/huggingface}
  GPU_BUILD=build-hip
  VULKAN_BUILD=build-vulkan
  CHAT_GGUF=/mnt/work/models/qwen3.5-4b/Qwen3.5-4B-Q4_K_S.gguf
  MTP_GGUF=
  KEV_GGUF=/mnt/work/huggingface/hub/models--ggml-org--Kev-0.8B-GGUF/snapshots/e551e319d483ff57e1ff208924b349d397cffc1c/Kev-0.8B-Q8_0.gguf
  LEMOND=/usr/bin/lemond
  SDCPP=$LLMS/stable-diffusion.cpp/build-hip/bin/sd-server
  WHISPER_BIN=$HOME/.cache/lemonade/bin/whispercpp/cpu/whisper-server
else
  HF_HOME=${HF_HOME:-/mnt/work/huggingface}
  GPU_BUILD=build-cuda
  VULKAN_BUILD=
  CHAT_GGUF=/mnt/work/huggingface/hub/models--unsloth--Qwen3.5-4B-GGUF/snapshots/$(cat /mnt/work/huggingface/hub/models--unsloth--Qwen3.5-4B-GGUF/refs/main)/Qwen3.5-4B-Q4_K_S.gguf
  MTP_GGUF=/mnt/work/huggingface/hub/models--unsloth--Qwen3.5-4B-MTP-GGUF/snapshots/$(cat /mnt/work/huggingface/hub/models--unsloth--Qwen3.5-4B-MTP-GGUF/refs/main)/Qwen3.5-4B-MTP-Q4_K_S.gguf
  KEV_GGUF=
  LEMOND=$LLMS/lemonade/lemonade-embeddable-2026.43.3-ubuntu-x64/lemond
  SDCPP=$LLMS/stable-diffusion.cpp/build-cuda/bin/sd-server
  WHISPER_BIN=$HOME/.cache/lemonade/bin/whispercpp/cpu/whisper-server
fi
LAYA_GGUF=${LAYA_GGUF:-}
FLM=$HOME/.local/opt/fastflowlm/flm
STT_WAV=${STT_WAV:-$LLMS/speaches/audio.wav}
E2E_ENGINE_NEXT=${E2E_ENGINE_START:-41100}
TINY_BIN=/mnt/work/huggingface/hub/models--ggerganov--whisper.cpp/snapshots/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-tiny.bin
TURBO_BIN=/mnt/work/models/whisper/ggml-large-v3-turbo-q5_0.bin

# Per-case state, reset by run_case:
#   desc         one-line description
#   CFG          config.yaml contents (the recipe block, real paths)
#   LAUNCH       llamastash argv for the launch, e.g. start <name> --preset x
#   VERIFY       function that proves the engine answered
#   needs        paths that must exist, else SKIP
#   needs_env    env vars that must be set, else SKIP (guards the 80 GB engines)
#   ARG0         substrings asserted in the engine child's argv
#   ROUTES       proxy paths to fetch for v_proxy_route
desc= CFG= VERIFY= EVIDENCE= PROXY= READY_PATH= READY_JQ=
LAUNCH=() needs=() needs_env=() ARG0=() ROUTES=()
CHAT_MODEL= IMG_SIZE=${IMG_SIZE:-256x256} TTS_VOICE=${TTS_VOICE:-alloy}

say() { printf '%s\n' "$*"; }

env_ls() {
  LLAMASTASH_STATE_DIR=$STATE LLAMASTASH_CONFIG_DIR=$CONF LLAMASTASH_CACHE_DIR=$CACHE \
    HF_HOME=$HF_HOME NO_COLOR=1 "$BIN" "$@"
}

daemon_log_tail() { tail -8 "$CACHE/logs/llamastash.log" 2>/dev/null | tr '\n' ' '; }

record() {
  printf '%s\t%s\t%s\t%s\n' "$HOST" "$1" "$2" \
    "$(printf '%s' "$3" | tr '\t\n' '  ' | cut -c1-500)" | tee -a "$RESULTS"
}

state_of() {
  env_ls status --json 2>/dev/null | jq -r '[.models[]?.state] | join(",")' 2>/dev/null
}

wait_ready() {
  local i s
  for ((i = 0; i < WAIT_TIMEOUT / 2; i++)); do
    s=$(state_of)
    case "$s" in
      *ready*) return 0 ;;
      *failed* | *error*) return 1 ;;
    esac
    sleep 2
  done
  return 1
}

# Some engines answer their ready path before the model is in memory
# (Chatterbox's /health reports model_loaded: false), so a case can additionally
# require a JSON answer from the engine's own port before the request runs.
wait_engine_json() {
  local port i body
  port=$(env_ls status --json 2>/dev/null | jq -r '[.models[]? | select(.state=="ready") | .port] | first // empty')
  [ -n "$port" ] || { EVIDENCE="no ready launch to probe"; return 1; }
  for i in $(seq $((WAIT_TIMEOUT / 5))); do
    body=$(curl -s -m 15 "http://127.0.0.1:$port${READY_PATH:-/health}") && jq -e "$READY_JQ" <<<"$body" >/dev/null 2>&1 && return 0
    sleep 5
  done
  EVIDENCE="engine ${READY_PATH:-/health} never answered $READY_JQ (last: ${body:-none})"
  return 1
}

v_ready_json_then() {
  wait_engine_json && "$1"
}

child_argv() {
  local pid
  pid=$(env_ls status --json 2>/dev/null | jq -r '[.models[]? | select(.state=="ready") | .pid] | first // empty')
  [ -n "$pid" ] && ps -o args= -p "$pid" 2>/dev/null
}

check_argv() {
  local argv needle
  argv=$(child_argv) || true
  [ -n "$argv" ] || { EVIDENCE="no child argv"; return 1; }
  for needle in "${ARG0[@]}"; do
    [[ $argv == *"$needle"* ]] || { EVIDENCE="argv lacks '$needle': ${argv:0:300}"; return 1; }
  done
  EVIDENCE="argv ok; ${argv:0:250}"
  return 0
}

# --- verify helpers -------------------------------------------------------

v_chat() {
  local body out txt
  body=$(jq -nc --arg m "${CHAT_MODEL%.gguf}" '{model: $m, messages: [{role: "user", content: "Reply with exactly: OK"}], max_tokens: 128, stream: false}')
  out=$(curl -sf -m "${REQ_TIMEOUT:-180}" "http://127.0.0.1:$PROXY/v1/chat/completions" -H 'content-type: application/json' -d "$body") ||
    { EVIDENCE="chat request failed"; return 1; }
  txt=$(jq -r '[.choices[0].message.content, .choices[0].text, .choices[0].message.reasoning_content] | map(select(type=="string" and length>0)) | first // empty' <<<"$out")
  [ -n "$txt" ] || { EVIDENCE="empty completion: ${out:0:250}"; return 1; }
  EVIDENCE="reply: ${txt:0:60}"
}

v_embedding() {
  curl -sf -m "${REQ_TIMEOUT:-180}" "http://127.0.0.1:$PROXY/v1/embeddings" -H 'content-type: application/json' \
     -d "{\"model\":\"${CHAT_MODEL%.gguf}\",\"input\":\"hello\"}" |
    jq -e '.data[0].embedding | length > 8' >/dev/null || { EVIDENCE="no embedding"; return 1; }
  EVIDENCE="embedding ok"
}

v_image() {
  local out n
  local try
  for try in 1 2 3 4 5 6; do
    out=$(curl -sf -m "${REQ_TIMEOUT:-2400}" "http://127.0.0.1:$PROXY/v1/images/generations" \
      -H 'content-type: application/json' \
      -d "{\"model\":\"${IMAGE_MODEL:-$CHAT_MODEL}\",\"prompt\":\"a red cube on a table\",\"size\":\"$IMG_SIZE\",\"n\":1}") && break
    sleep 20
  done
  [ -n "${out:-}" ] || { EVIDENCE="image request failed"; return 1; }
  local url
  n=$(jq -r '.data[0].b64_json // empty' <<<"$out" | wc -c)
  url=$(jq -r '.data[0].url // empty' <<<"$out")
  if [ "${n:-0}" -le 4096 ] && [ -n "$url" ]; then
    curl -sf -m 120 "$url" -o "$STATE/image.out" || { EVIDENCE="image url $url not fetchable"; return 1; }
    n=$(wc -c <"$STATE/image.out")
    EVIDENCE="image $IMG_SIZE via $url: $n bytes, $(file -b "$STATE/image.out")"
    return 0
  fi
  [ "${n:-0}" -gt 4096 ] || { EVIDENCE="small image payload (${n} b64 chars): ${out:0:250}"; return 1; }
  EVIDENCE="image $IMG_SIZE, b64 chars: $n"
}

# Media routes: name the entry first, then the OpenAI default name, which the
# proxy routes by mode and the engine maps itself.
v_stt() {
  local name out t try
  for try in 1 2 3 4 5 6; do
    for name in "${STT_MODEL:-$CHAT_MODEL}" "$CHAT_MODEL" whisper-1; do
      out=$(curl -sf -m "${REQ_TIMEOUT:-900}" "http://127.0.0.1:$PROXY/v1/audio/transcriptions" \
        -F "model=$name" -F "file=@$STT_WAV") && break
    done
    [ -n "${out:-}" ] && break
    sleep 20
  done
  [ -n "${out:-}" ] || { EVIDENCE="stt request failed (tried ${STT_MODEL:-$CHAT_MODEL}, $CHAT_MODEL, whisper-1)"; return 1; }
  t=$(jq -r '.text // empty' <<<"$out")
  [ -n "$t" ] || { EVIDENCE="empty transcript: ${out:0:250}"; return 1; }
  EVIDENCE="transcript ($name): ${t:0:60}"
}

v_tts() {
  local name f=$STATE/speech.bin sz code try
  for try in 1 2 3 4 5 6; do
    for name in "${TTS_MODEL:-$CHAT_MODEL}" "$CHAT_MODEL" tts-1; do
      code=$(curl -s -o "$f" -w '%{http_code}' -m "${REQ_TIMEOUT:-900}" "http://127.0.0.1:$PROXY/v1/audio/speech" \
        -H 'content-type: application/json' \
        -d "{\"model\":\"$name\",\"input\":\"Hello from the recipe test.\",\"voice\":\"$TTS_VOICE\"}")
      [ "$code" = 200 ] && break
    done
    [ "$code" = 200 ] && break
    sleep 20
  done
  sz=$(stat -c %s "$f" 2>/dev/null || echo 0)
  [ "$sz" -gt 2000 ] || { EVIDENCE="no audio ($name -> HTTP $code, $sz bytes): $(tr -d '\0' <"$f" 2>/dev/null | head -c 200)"; return 1; }
  EVIDENCE="audio $sz bytes, $(file -b "$f")"
}

v_proxy_route() {
  local path p
  for path in "${ROUTES[@]}"; do
    p=$(curl -s -o "$STATE/route.out" -w '%{http_code}' -m "${REQ_TIMEOUT:-90}" "http://127.0.0.1:$PROXY$path")
    [ "$p" = 200 ] || { EVIDENCE="route $path -> HTTP $p"; return 1; }
  done
  EVIDENCE="routes ok: ${ROUTES[*]} ($(wc -c <"$STATE/route.out") bytes)"
}

v_systemone() {
  local out
  out=$(curl -sf -m "${REQ_TIMEOUT:-180}" "http://127.0.0.1:$PROXY/v1/systemone" -H 'content-type: application/json' \
    -d "{\"model\":\"${CHAT_MODEL%.gguf}\",\"state\":{\"command\":\"rm -rf /\"},\"questions\":{\"risky\":{\"type\":\"noul\"}}}") ||
    { EVIDENCE="systemone request failed"; return 1; }
  jq -e '.answers.risky.noul != null' <<<"$out" >/dev/null ||
    { EVIDENCE="unexpected systemone body: ${out:0:250}"; return 1; }
  EVIDENCE="systemone: ${out:0:120}"
}

# Composite helpers.
v_and_argv() { check_argv && v_chat; }
v_image_route() { v_proxy_route && v_image; }
v_systemone_argv() { check_argv && v_systemone; }
v_tts_argv() { check_argv && v_tts; }

# The presets-only blocks in the page assume a llama.cpp build is already
# configured; an isolated daemon has none, so the preset cases add one.
llama_servers() {
  printf 'backend:\n  llamacpp:\n    servers:\n      - binary: %s/%s/bin/llama-server\n      - name: cpu\n        binary: %s/build-cpu/bin/llama-server\n' "$LLAMA_DIR" "$GPU_BUILD" "$LLAMA_DIR"
}

# --- cases ----------------------------------------------------------------

case_llamacpp_builds() {
  desc='llama.cpp builds: probe names + --server selection'
  local second=
  [ -n "$VULKAN_BUILD" ] && second="      - binary: $LLAMA_DIR/$VULKAN_BUILD/bin/llama-server"
  CFG="
backend:
  llamacpp:
    servers:
      - binary: $LLAMA_DIR/$GPU_BUILD/bin/llama-server
$second
      - name: cpu
        binary: $LLAMA_DIR/build-cpu/bin/llama-server
"
  needs=("$LLAMA_DIR/$GPU_BUILD/bin/llama-server" "$CHAT_GGUF")
  CHAT_MODEL=$(basename "$CHAT_GGUF")
  LAUNCH=(start "$CHAT_GGUF" --server llamacpp-cpu)
  ARG0=(build-cpu/bin/llama-server)
  VERIFY=v_probed_selection
}

v_probed_selection() {
  local ids
  ids=$(env_ls status --json | jq -r '[.servers[]?.id] | join(",")')
  [[ $ids == *,llamacpp-cpu || $ids == llamacpp-cpu* ]] ||
    { EVIDENCE="probed servers: ${ids:-none}"; return 1; }
  check_argv && v_chat && EVIDENCE="servers: $ids; $EVIDENCE"
}

case_llamacpp_preset_server() {
  desc='llama.cpp presets: default entry pins the CPU build, knobs reach the server'
  CFG="
backend:
  llamacpp:
    servers:
      - binary: $LLAMA_DIR/$GPU_BUILD/bin/llama-server
      - name: cpu
        binary: $LLAMA_DIR/build-cpu/bin/llama-server
presets:
  $(basename "$CHAT_GGUF"):
    default: cpu
    entries:
      cpu:
        server: llamacpp-cpu
        knobs:
          threads: 8
          parallel: 1
          ctx-size: 4096
"
  needs=("$CHAT_GGUF")
  CHAT_MODEL=$(basename "$CHAT_GGUF")
  LAUNCH=(start "$CHAT_GGUF")
  ARG0=(build-cpu/bin/llama-server '--threads 8' '-c 4096')
  VERIFY=v_and_argv
}

case_llamacpp_glob_preset() {
  desc='llama.cpp presets: a glob key applies to every quant of the model family'
CFG="$(llama_servers)
presets:
  \"Qwen3.5-4B-*\":
    default: small
    entries:
      small:
        knobs:
          ctx-size: 2048
"
  needs=("$CHAT_GGUF")
  CHAT_MODEL=$(basename "$CHAT_GGUF")
  LAUNCH=(start "$CHAT_GGUF")
  ARG0=(-c 2048)
  VERIFY=v_and_argv
}

case_small_gpu_vision() {
  desc='Small GPU: Qwen3.5 4B vision preset (n-gpu-layers, q8_0 K cache, mmproj on CPU)'
CFG="$(llama_servers)
presets:
  $(basename "$CHAT_GGUF"):
    default: vision
    entries:
      vision:
        idle_ttl_secs: 600
        knobs:
          ctx-size: 4096
          n-gpu-layers: 99
          cache-type-k: q8_0
          flash-attn: false
          parallel: 1
        extras: [\"--no-mmproj-offload\"]
"
  needs=("$CHAT_GGUF")
  CHAT_MODEL=$(basename "$CHAT_GGUF")
  LAUNCH=(start "$CHAT_GGUF")
  ARG0=(--n-gpu-layers 99 '--cache-type-k q8_0' --no-mmproj-offload '-c 4096' '--flash-attn off')
  VERIFY=v_and_argv
}

case_mtp_knobs() {
  desc='MTP knobs: mtp-draft-n and mtp off on an MTP GGUF'
CFG="$(llama_servers)
presets:
  $(basename "$MTP_GGUF"):
    default: no-mtp
    entries:
      no-mtp:
        knobs:
          mtp: off
      draft-5:
        knobs:
          mtp-draft-n: 5
"
  needs=("$MTP_GGUF")
  CHAT_MODEL=$(basename "$MTP_GGUF")
  LAUNCH=(start "$MTP_GGUF" --preset draft-5)
  VERIFY=v_and_argv
}

case_mtp_off() {
  desc='MTP knobs: the no-mtp entry turns draft tokens off'
CFG="$(llama_servers)
presets:
  $(basename "$MTP_GGUF"):
    default: no-mtp
    entries:
      no-mtp:
        knobs:
          mtp: off
"
  needs=("$MTP_GGUF")
  CHAT_MODEL=$(basename "$MTP_GGUF")
  LAUNCH=(start "$MTP_GGUF" --preset no-mtp)
  VERIFY=v_and_argv
}

decision_cfg() {
  cat <<EOF

backend:
  llamacpp:
    servers:
      - binary: $LLAMA_DIR/$GPU_BUILD/bin/llama-server
      - name: cpu
        binary: $LLAMA_DIR/build-cpu/bin/llama-server
presets:
  Kev-0.8B-Q8_0.gguf:
    default: automode
    entries:
      automode:
        server: llamacpp-cpu
        knobs:
          threads: 8
          parallel: 1
          ctx_size: 1024
        extras: ["--sleep-idle-seconds", "300"]
  laya-typed-decisions-Q8_0.gguf:
    default: automode
    entries:
      automode:
        server: llamacpp-cpu
        knobs:
          mode: embedding
          threads: 8
          parallel: 1
          ctx_size: 512
        extras: ["--sleep-idle-seconds", "300"]
EOF
}

case_decision_kev() {
  desc='Decision models: Kev on the CPU build, automode preset, /v1/systemone answer'
  CFG=$(decision_cfg)
  needs=("$KEV_GGUF" "$LLAMA_DIR/build-cpu/bin/llama-server")
  CHAT_MODEL=Kev-0.8B-Q8_0.gguf
  LAUNCH=(start "$KEV_GGUF")
  ARG0=(build-cpu/bin/llama-server '--threads 8' '-c 1024' '--sleep-idle-seconds 300')
  VERIFY=v_systemone_argv
}

case_decision_laya() {
  desc='Decision models: Laya on the CPU build in embedding mode'
  CFG=$(decision_cfg)
  needs=("$LAYA_GGUF")
  CHAT_MODEL=laya-typed-decisions-Q8_0.gguf
  LAUNCH=(start "$LAYA_GGUF")
  ARG0=(build-cpu/bin/llama-server '-c 512' --embedding)
  VERIFY=v_embedding_argv
}

v_embedding_argv() { check_argv && v_embedding; }

case_sdcpp_moss() {
  desc='stable-diffusion.cpp: MOSS-Image entry (image mode + /sdapi/v1 route)'
  CFG="
backend:
  generic:
    servers:
      - name: moss-image
        binary: $SDCPP
        args: [--diffusion-model, /mnt/work/models/moss-image/MOSS-Image-SFT-Q4_K.gguf,
               --llm, /mnt/work/models/moss-image/Qwen3.5-9B-Q4_K_M.gguf,
               --vae, /mnt/work/models/moss-image/flux2-vae.safetensors,
               --tokenizer, /mnt/work/models/moss-image/tokenizer.json,
               --cfg-scale, \"3.0\", --offload-to-cpu,
               --listen-ip, \"{host}\", --listen-port, \"{port}\"]
        ready: /v1/models
        modes: [image]
        routes: [/sdapi/v1]
        memory_gib: 14
        ready_timeout_secs: 600
"
  needs=("$SDCPP" /mnt/work/models/moss-image/MOSS-Image-SFT-Q4_K.gguf)
  CHAT_MODEL=moss-image
  LAUNCH=(start moss-image)
  ARG0=(--cfg-scale 3.0 --offload-to-cpu)
  ROUTES=(/sdapi/v1/options)
  VERIFY=v_image_route
}

case_sdcpp_krea2() {
  desc='stable-diffusion.cpp: Krea 2 entry'
  CFG="
backend:
  generic:
    servers:
      - name: krea2
        binary: $SDCPP
        args: [--diffusion-model, /mnt/work/models/krea2/Krea-2-Base-Q4_K_M.gguf,
               --llm, /mnt/work/models/krea2/Qwen3VL-4B-Instruct-Q4_K_M.gguf,
               --vae, /mnt/work/models/krea2/wan_2.1_vae.safetensors,
               --cfg-scale, \"3.5\",
               --listen-ip, \"{host}\", --listen-port, \"{port}\"]
        ready: /v1/models
        modes: [image]
        memory_gib: 12
        ready_timeout_secs: 300
"
  needs=("$SDCPP" /mnt/work/models/krea2/Krea-2-Base-Q4_K_M.gguf)
  CHAT_MODEL=krea2
  LAUNCH=(start krea2)
  VERIFY=v_image
}

case_comfyui() {
  desc='ComfyUI entry: /system_stats readiness + declared routes through the proxy'
  CFG="
backend:
  generic:
    servers:
      - name: comfyui
        binary: $LLMS/ComfyUI/.venv/bin/python
        args: [$LLMS/ComfyUI/main.py, --listen, \"{host}\", --port, \"{port}\"]
        ready: /system_stats
        routes: [/prompt, /history, /view, /upload, /object_info, /queue, /system_stats]
        memory_gib: 4
        ready_timeout_secs: 300
"
  needs=("$LLMS/ComfyUI/.venv/bin/python" "$LLMS/ComfyUI/main.py")
  CHAT_MODEL=comfyui
  LAUNCH=(start comfyui)
  ROUTES=(/system_stats /object_info)
  VERIFY=v_proxy_route
}

case_localai() {
  desc='LocalAI entry: stt + tts + image modes on one binary'
  CFG="
backend:
  generic:
    servers:
      - name: localai
        binary: $LLMS/localai/local-ai-v4.12.1-linux-amd64
        args: [run, --address, \"{host}:{port}\", --models-path, $LLMS/localai/models,
               --backends-path, $LLMS/localai/backends, --data-path, $LLMS/localai/data]
        ready: /readyz
        modes: [stt, tts, image]
        memory_gib: 4
        ready_timeout_secs: 180
"
  needs=("$LLMS/localai/local-ai-v4.12.1-linux-amd64")
  CHAT_MODEL=whisper-1
  # LocalAI names its models itself: tts-1 wants a Kokoro voice, not an OpenAI alias.
  TTS_VOICE=af_heart
  IMAGE_MODEL=dall-e-3
  LAUNCH=(start localai)
  VERIFY=v_localai
}

v_localai() {
  local note= ok=1
  v_stt && note="stt: $EVIDENCE" || { note="stt: $EVIDENCE"; ok=0; }
  v_tts && note="$note; tts: $EVIDENCE" || { note="$note; tts: $EVIDENCE"; ok=0; }
  v_image && note="$note; image: $EVIDENCE" || { note="$note; image: $EVIDENCE"; ok=0; }
  EVIDENCE="$note"
  [ $ok = 1 ]
}

case_whisper_tiny() {
  desc='whisper.cpp (lemonade download): OpenAI /v1/audio shape'
  CFG="
backend:
  generic:
    servers:
      - name: whisper-tiny
        binary: $WHISPER_BIN
        args: [-m, $TINY_BIN,
               --host, \"{host}\", --port, \"{port}\",
               --request-path, /v1/audio, --inference-path, /transcriptions]
        env:
          LD_LIBRARY_PATH: $(dirname "$WHISPER_BIN")
        ready: /v1/audio/health
        modes: [stt]
        memory_gib: 1
"
  needs=("$WHISPER_BIN" "$TINY_BIN")
  CHAT_MODEL=whisper-tiny
  LAUNCH=(start whisper-tiny)
  VERIFY=v_stt
}

case_whisper_turbo() {
  desc='whisper.cpp build-cuda: OpenAI shape + native /inference route'
  CFG="
backend:
  generic:
    servers:
      - name: whisper-turbo
        binary: $LLMS/whisper.cpp/build-cuda/bin/whisper-server
        args: [-m, $TURBO_BIN, --host, \"{host}\", --port, \"{port}\",
               --request-path, /v1/audio, --inference-path, /transcriptions]
        env: {LD_LIBRARY_PATH: $LLMS/whisper.cpp/build-cuda/bin}
        ready: /v1/audio/health
        modes: [stt]
        memory_gib: 2
      - name: whisper-native
        binary: $LLMS/whisper.cpp/build-cuda/bin/whisper-server
        args: [-m, $TURBO_BIN, --host, \"{host}\", --port, \"{port}\"]
        env: {LD_LIBRARY_PATH: $LLMS/whisper.cpp/build-cuda/bin}
        ready: /health
        routes: [/inference]
        memory_gib: 2
"
  needs=("$LLMS/whisper.cpp/build-cuda/bin/whisper-server" "$TURBO_BIN")
  CHAT_MODEL=whisper-turbo
  LAUNCH=(start whisper-turbo)
  VERIFY=v_stt
}

case_whisper_native() {
  desc='whisper.cpp native API through a declared /inference route'
  CFG="
backend:
  generic:
    servers:
      - name: whisper-native
        binary: $LLMS/whisper.cpp/build-cuda/bin/whisper-server
        args: [-m, $TURBO_BIN, --host, \"{host}\", --port, \"{port}\"]
        env: {LD_LIBRARY_PATH: $LLMS/whisper.cpp/build-cuda/bin}
        ready: /health
        routes: [/inference]
        memory_gib: 2
"
  needs=("$LLMS/whisper.cpp/build-cuda/bin/whisper-server" "$TURBO_BIN")
  CHAT_MODEL=whisper-native
  LAUNCH=(start whisper-native)
  VERIFY=v_whisper_inference
}

v_whisper_inference() {
  local out
  out=$(curl -sf -m "${REQ_TIMEOUT:-900}" "http://127.0.0.1:$PROXY/inference" -F "file=@$STT_WAV") ||
    { EVIDENCE="/inference route request failed"; return 1; }
  local t
  t=$(jq -r '.text // empty' <<<"$out")
  [ -n "$t" ] || { EVIDENCE="no text from /inference: ${out:0:200}"; return 1; }
  EVIDENCE="/inference: ${t:0:60}"
}

case_speaches() {
  desc='Speaches entry: stt + tts on uvicorn --factory'
  CFG="
backend:
  generic:
    servers:
      - name: speaches
        binary: /bin/sh
        args: [-c, 'cd $LLMS/speaches && exec .venv/bin/uvicorn --factory speaches.main:create_app --host \"\$1\" --port \"\$2\"',
               sh, \"{host}\", \"{port}\"]
        env: {HF_HOME: $HF_HOME}
        ready: /health
        modes: [stt, tts]
        memory_gib: 2
        ready_timeout_secs: 180
"
  needs=("$LLMS/speaches/.venv/bin/uvicorn")
  CHAT_MODEL=speaches
  STT_MODEL=Systran/faster-whisper-small
  LAUNCH=(start speaches)
  VERIFY=v_speaches
}

v_speaches() {
  local note= ok=1
  v_stt && note="stt: $EVIDENCE" || { note="stt: $EVIDENCE"; ok=0; }
  v_tts && note="$note; tts: $EVIDENCE" || { note="$note; tts: $EVIDENCE"; ok=0; }
  EVIDENCE="$note"
  [ $ok = 1 ]
}

case_piper() {
  desc='Piper http_server entry'
  CFG="
backend:
  generic:
    servers:
      - name: piper
        binary: $LLMS/piper/.venv/bin/python
        args: [-m, piper.http_server, -m, en_US-lessac-medium, --data-dir, $LLMS/piper/voices,
               --host, \"{host}\", --port, \"{port}\"]
        ready: /voices
        routes: [/synthesize, /voices]
        memory_gib: 1
"
  needs=("$LLMS/piper/.venv/bin/python")
  CHAT_MODEL=piper
  LAUNCH=(start piper)
  ROUTES=(/voices)
  VERIFY=v_piper
}

v_piper() {
  local f=$STATE/piper.wav
  curl -sf -m "${REQ_TIMEOUT:-300}" "http://127.0.0.1:$PROXY/synthesize" -H 'content-type: application/json'     -d '{"text":"Hello from the recipe test.","voice":"en_US-lessac-medium"}' -o "$f" ||
    { EVIDENCE="POST /synthesize failed"; return 1; }
  local sz
  sz=$(stat -c %s "$f" 2>/dev/null || echo 0)
  [ "$sz" -gt 2000 ] || { EVIDENCE="/synthesize returned $sz bytes: $(file -b "$f")"; return 1; }
  EVIDENCE="/synthesize $sz bytes, $(file -b "$f")"
}

case_kokoro_fastapi() {
  desc='Kokoro-FastAPI entry'
  CFG="
backend:
  generic:
    servers:
      - name: kokoro
        binary: $LLMS/Kokoro-FastAPI/.venv/bin/uvicorn
        args: [api.src.main:app, --host, \"{host}\", --port, \"{port}\"]
        env:
          USE_GPU: \"true\"
          PYTHONPATH: $LLMS/Kokoro-FastAPI:$LLMS/Kokoro-FastAPI/api
          MODEL_DIR: $LLMS/Kokoro-FastAPI/api/src/models
          VOICES_DIR: $LLMS/Kokoro-FastAPI/api/src/voices/v1_0
          WEB_PLAYER_PATH: $LLMS/Kokoro-FastAPI/web
        ready: /v1/models
        modes: [tts]
        memory_gib: 2
        ready_timeout_secs: 180
"
  needs=("$LLMS/Kokoro-FastAPI/.venv/bin/uvicorn")
  CHAT_MODEL=kokoro
  LAUNCH=(start kokoro)
  VERIFY=v_tts
}

case_chatterbox() {
  desc='Chatterbox TTS API entry'
  CFG="
backend:
  generic:
    servers:
      - name: chatterbox
        binary: $LLMS/chatterbox-tts-api/.venv/bin/uvicorn
        args: [app.main:app, --host, \"{host}\", --port, \"{port}\"]
        env:
          PYTHONPATH: $LLMS/chatterbox-tts-api
          DEVICE: cuda
          USE_MULTILINGUAL_MODEL: \"false\"
          MODEL_CACHE_DIR: $LLMS/chatterbox-tts-api/models
          VOICE_SAMPLE_PATH: $LLMS/chatterbox-tts-api/voice-sample.mp3
          VOICE_LIBRARY_DIR: $LLMS/chatterbox-tts-api/voices
          LONG_TEXT_DATA_DIR: $LLMS/chatterbox-tts-api/data/long_text_jobs
        ready: /health
        modes: [tts]
        memory_gib: 4
        ready_timeout_secs: 600
"
  needs=("$LLMS/chatterbox-tts-api/.venv/bin/uvicorn")
  CHAT_MODEL=chatterbox
  READY_PATH=/health
  READY_JQ=.model_loaded
  LAUNCH=(start chatterbox)
  VERIFY="v_ready_json_then v_tts"
}

case_lemonade() {
  desc='Lemonade lemond entry: /live readiness + proxy aliases'
  CFG="
backend:
  generic:
    servers:
      - name: lemonade
        binary: $LEMOND
        args: [\"--port\", \"{port}\", \"--host\", \"{host}\", \"--no-broadcast\", \"--log-file\", \"disabled\"]
        env: {HF_HOME: $HF_HOME}
        ready: /live
        ready_timeout_secs: 90
        stop_grace_secs: 20
        modes: [chat, stt, tts, image]
        memory_gib: 1

proxy:
  aliases:
    Whisper-Tiny: lemonade
    kokoro-v1: lemonade
    SD-Turbo-GGUF: lemonade
"
  needs=("$LEMOND")
  CHAT_MODEL=lemonade
  LAUNCH=(start lemonade)
  VERIFY=v_lemonade
}

v_lemonade() {
  local port models ids alias_hit
  port=$(env_ls status --json | jq -r '[.models[] | select(.state=="ready") | .port] | first')
  models=$(curl -sf -m 30 "http://127.0.0.1:$port/api/v1/models" | jq -r '[.[].path] | join(",")' 2>/dev/null)
  ids=$(curl -sf -m 30 "http://127.0.0.1:$PROXY/v1/models" | jq -r '[.data[].id] | join(",")')
  alias_hit=
  for a in kokoro-v1 SD-Turbo-GGUF Whisper-Tiny; do
    [[ ,${ids,,}, == *,"${a,,}",* ]] && alias_hit="$alias_hit $a"
  done
  EVIDENCE="aliases on /v1/models:${alias_hit:- none}; lemond local models: ${models:-none}"
  [ -n "$alias_hit" ] || return 1
  # one real request through an alias, when lemond has a model for it
  if [[ ,${models,,}, == *kokoro* ]]; then
    CHAT_MODEL=kokoro-v1 && v_tts && EVIDENCE="tts via alias: $EVIDENCE" && return 0
  fi
  if [[ ,${models,,}, == *whisper* ]]; then
    CHAT_MODEL=Whisper-Tiny && v_stt && EVIDENCE="stt via alias: $EVIDENCE" && return 0
  fi
  EVIDENCE="alias routing ok, no local lemond model to call; $EVIDENCE"
  return 0
}

case_lancet() {
  desc='LANCET docker wrapper entry'
  CFG="
backend:
  generic:
    servers:
      - name: lancet
        binary: $HOME/Workspace/oss-libs/pi-automode-classifier/lancet/lancet-serve.sh
        args: [\"{port}\"]
        ready: /health
        memory_gib: 0.5
        stop_grace_secs: 10
"
  needs=("$HOME/Workspace/oss-libs/pi-automode-classifier/lancet/lancet-serve.sh")
  CHAT_MODEL=lancet
  LAUNCH=(start lancet)
  VERIFY=v_systemone
}

case_fflm_chat() {
  desc='FastFlowLM: one entry pinned to flm model tag qwen3:0.6b'
  CFG="
backend:
  generic:
    servers:
      - name: \"qwen3:0.6b\"
        binary: $FLM
        args: [serve, \"{name}\", --host, \"{host}\", -p, \"{port}\", --cors, \"0\"]
        ready: /v1/version
        ready_timeout_secs: 180
        stop_grace_secs: 20
        memory_gib: 2
        arch: qwen3
        params: 0.6B
        quant: q4nx
        ctx: 32768
        modes: [chat]
        knobs:
          - {flag: --ctx-len, id: ctx-len, ctx: true, default: \"32768\"}
          - {flag: --pmode, default: performance}
"
  needs=("$FLM")
  CHAT_MODEL=qwen3:0.6b
  LAUNCH=(start qwen3:0.6b)
  ARG0=(--pmode performance '--ctx-len 32768')
  VERIFY=v_and_argv
}

case_fflm_rows_from() {
  desc='FastFlowLM: rows_from mints a row per installed flm model'
  CFG="
backend:
  generic:
    servers:
      - name: flm
        binary: $FLM
        args: [serve, \"{name}\", --host, \"{host}\", -p, \"{port}\", --cors, \"0\"]
        ready: /v1/version
        ready_timeout_secs: 180
        stop_grace_secs: 20
        modes: [chat]
        rows_from:
          command: [$FLM, list, --filter, installed, --json]
          items: models
          name: model
          when: {path: label, contains: reasoning}
          arch: details.family
          params: details.parameter_size
          quant: details.quantization_level
          ctx: default_context_length
          memory_gib: footprint
        knobs:
          - {flag: --ctx-len, id: ctx-len, ctx: true}
          - {flag: --pmode, default: performance}
"
  needs=("$FLM")
  CHAT_MODEL=qwen3:0.6b
  LAUNCH=(start qwen3:0.6b)
  VERIFY=v_rows_from
}

v_rows_from() {
  local rows
  rows=$(env_ls list --json | jq -r '[.models[] | select(.backend=="generic") | .name] | join(",")')
  [[ $rows == *qwen3:0.6b* ]] || { EVIDENCE="minted rows: ${rows:-none}"; return 1; }
  v_chat && EVIDENCE="rows: $rows; $EVIDENCE"
}

case_fflm_whisper_npu() {
  desc='FastFlowLM: Whisper on the NPU (stt mode)'
  CFG="
backend:
  generic:
    servers:
      - name: whisper-npu
        binary: $FLM
        args: [serve, --asr, \"1\", --host, \"{host}\", -p, \"{port}\", --cors, \"0\"]
        ready: /v1/version
        ready_timeout_secs: 180
        stop_grace_secs: 20
        memory_gib: 2
        modes: [stt]
"
  needs=("$FLM")
  CHAT_MODEL=whisper-npu
  LAUNCH=(start whisper-npu)
  VERIFY=v_stt
}

# The 80 GB Flash-Next engines: guarded so a run on the box that is serving
# something else cannot evict it. Set RECIPE_E2E_ALLOW_HEAVY=1 to enable.
case_halogen() {
  desc='Halogen (Docker) entry + presets + harness aliases'
  CFG="
backend:
  generic:
    servers:
      - name: flash-next-halogen
        binary: ${HALOGEN_SERVE:-$HOME/.local/bin/halogen-serve}
        args: [\"{port}\"]
        arch: qwen4exp
        params: 177B
        quant: v2-4.16bpw
        ctx: 262144
        reasoning_effort: [none, low, medium, xhigh]
        reasoning_effort_default: xhigh
        memory_gib: 89
        knobs:
          - {flag: --ctx-window, id: halogen-ctx, ctx: true, default: \"131072\"}
          - {flag: --halogen-kv-pool, id: halogen-pool, default: \"131072\"}
          - {flag: --halogen-temperature, id: halogen-temp, default: \"1.0\"}
          - {flag: --halogen-mtp-depth, id: halogen-mtp-depth, default: \"3\"}
          - {flag: --halogen-reasoning-effort, id: halogen-effort, default: xhigh}
        env:
          HALOGEN_MODEL_ID: \"{name}\"
          HALOGEN_CTX: \"{halogen-ctx}\"
          HALOGEN_KV_POOL_POSITIONS: \"{halogen-pool}\"
          HALOGEN_TEMPERATURE: \"{halogen-temp}\"
          HALOGEN_MTP_DEPTH: \"{halogen-mtp-depth}\"
          HALOGEN_REASONING_EFFORT: \"{halogen-effort}\"
        ready: /v1/models
        stop_grace_secs: 90
        ready_timeout_secs: 600

presets:
  flash-next-halogen:
    entries:
      dual-256k:
        knobs:
          halogen-ctx: 262144
          halogen-pool: 524288
      halogen-medium:
        knobs:
          halogen-effort: medium

proxy:
  aliases:
    flash-next-256k: flash-next-halogen
"
  needs=("${HALOGEN_SERVE:-$HOME/.local/bin/halogen-serve}")
  needs_env=(RECIPE_E2E_ALLOW_HEAVY)
  CHAT_MODEL=flash-next-halogen
  LAUNCH=(start flash-next-halogen --preset halogen-medium)
  VERIFY=v_chat
}

case_strata() {
  desc='Strata entry (serve --profile)'
  CFG="
backend:
  generic:
    servers:
      - name: flash-next-strata
        binary: ~/.local/bin/strata
        args: [serve, --profile, ${STRATA_PROFILE:-$HOME/.local/share/strata/install/strata-unsloth-ud-q4_k_xl.json},
               --host, \"{host}\", --port, \"{port}\", --shutdown-timeout, \"30\", --quiet]
        arch: qwen4exp
        params: 177B
        quant: UD-Q4_K_XL
        ctx: 65536
        ready: /health
        stop_grace_secs: 90
        ready_timeout_secs: 900
"
  needs=("$HOME/.local/bin/strata" "${STRATA_PROFILE:-$HOME/.local/share/strata/install/strata-unsloth-ud-q4_k_xl.json}")
  needs_env=(RECIPE_E2E_ALLOW_HEAVY)
  CHAT_MODEL=flash-next-strata
  LAUNCH=(start flash-next-strata)
  VERIFY=v_chat
}

case_gufo() {
  desc='gufo entry + gufo-128k/256k presets + --seed extra'
  CFG="
backend:
  generic:
    servers:
      - name: gufo
        model: Qwen3.8-Flash-Next-UD-Q4_K_XL
        binary: ${GUFO_BIN:-/mnt/work/Workspace/llms/gufo/build/release-rocm10/gufo}
        args: [serve, --host, \"{host}\", --port, \"{port}\", --sessions, \"1\", llm,
               --served-model-name, \"{name}\", --model, \"${GUFO_GGUF:-}\",
               --mtp-model, \"${GUFO_MTP:-}\",
               --top-p, \"0.95\", --top-k, \"20\"]
        knobs:
          - {flag: --context, ctx: true, default: \"131072\"}
          - {flag: --speculative, default: mtp}
          - {flag: --temperature, default: \"1.0\"}
          - --seed
        ready: /ready
        stop_grace_secs: 60
        rewrite_model: true

presets:
  Qwen3.8-Flash-Next-*:
    entries:
      gufo-128k:
        server: generic-gufo
        knobs:
          context: 131072
          speculative: mtp
          temperature: '1.0'
      gufo-256k:
        server: generic-gufo
        knobs:
          context: 262144
"
  needs=("${GUFO_BIN:-/mnt/work/Workspace/llms/gufo/build/release-rocm10/gufo}" "${GUFO_GGUF:-/nonexistent}")
  needs_env=(RECIPE_E2E_ALLOW_HEAVY)
  CHAT_MODEL=Qwen3.8-Flash-Next-UD-Q4_K_XL
  LAUNCH=(start Qwen3.8-Flash-Next-UD-Q4_K_XL --preset gufo-128k)
  VERIFY=v_and_argv
}

case_ciru() {
  desc='CIRU entry (wrapper + env knobs)'
  CFG="
backend:
  generic:
    servers:
      - name: flash-next-ciru
        binary: ${CIRU_SERVE:-$HOME/bin/ciru-serve.sh}
        knobs:
          - {flag: --ciru-ctx, id: ciru-ctx, ctx: true, default: \"65536\"}
          - {flag: --mtp-depth, id: mtp-depth, default: \"3\"}
        env:
          PORT: \"{port}\"
          CONTEXT_SIZE: \"{ciru-ctx}\"
          MTP_DEPTH: \"{mtp-depth}\"
        ready: /health
        stop_grace_secs: 60
        ready_timeout_secs: 600
"
  needs=("${CIRU_SERVE:-$HOME/bin/ciru-serve.sh}")
  needs_env=(RECIPE_E2E_ALLOW_HEAVY)
  CHAT_MODEL=flash-next-ciru
  LAUNCH=(start flash-next-ciru)
  VERIFY=v_chat
}

case_ds4() {
  desc='DeepSeek V4 generic entry (knob table) + MTP/DSpark presets'
  CFG="
backend:
  generic:
    servers:
      - name: ds4
        model: \"DeepSeek-V4-*\"
        binary: ${DS4_BIN:-/nonexistent/ds4-server}
        args: [-m, \"{model}\", --host, \"{host}\", --port, \"{port}\"]
        ready: /v1/models
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
          mtp-model: ${DS4_MTP:-/nonexistent-mtp.gguf}
      ds4-dspark:
        server: generic-ds4
        knobs:
          dspark: true
          mtp-model: ${DS4_DSPARK:-/nonexistent-dspark.gguf}
"
  needs=("${DS4_BIN:-/nonexistent/ds4-server}" "${DS4_GGUF:-/nonexistent.gguf}")
  CHAT_MODEL=ds4
  LAUNCH=(start "${DS4_GGUF:-/nonexistent.gguf}")
  VERIFY=v_chat
}

case_vllm() {
  desc='vLLM backend: safetensors row + kv-cache-memory-bytes knob'
  CFG="
backend:
  vllm:
    servers:
      - binary: ${VLLM_BIN:-$HOME/.venvs/vllm/bin/vllm}
    cors: false
presets:
  ${VLLM_REPO:-Qwen/Qwen2.5-0.5B-Instruct}:
    entries:
      vllm-16g:
        backend: vllm
        knobs:
          kv-cache-memory-bytes: 16G
"
  needs=("${VLLM_BIN:-$HOME/.venvs/vllm/bin/vllm}")
  CHAT_MODEL=${VLLM_REPO:-Qwen/Qwen2.5-0.5B-Instruct}
  LAUNCH=(start "${VLLM_REPO:-Qwen/Qwen2.5-0.5B-Instruct}" --preset vllm-16g)
  VERIFY=v_chat
}

case_sglang() {
  desc='SGLang backend: safetensors row + max-total-tokens knob'
  CFG="
backend:
  sglang:
    servers:
      - binary: ${SGLANG_BIN:-$HOME/.venvs/sglang/bin/sglang}
presets:
  ${SGLANG_REPO:-Qwen/Qwen2.5-0.5B-Instruct}:
    entries:
      sglang-128k:
        backend: sglang
        knobs:
          max-total-tokens: 131072
"
  needs=("${SGLANG_BIN:-$HOME/.venvs/sglang/bin/sglang}")
  CHAT_MODEL=${SGLANG_REPO:-Qwen/Qwen2.5-0.5B-Instruct}
  LAUNCH=(start "${SGLANG_REPO:-Qwen/Qwen2.5-0.5B-Instruct}" --preset sglang-128k)
  VERIFY=v_chat
}

# --- runner ---------------------------------------------------------------

run_case() {
  local name=$1 missing= d
  desc=$name
  CFG= LAUNCH=() VERIFY= needs=() needs_env=() ARG0=() ROUTES=() EVIDENCE= CHAT_MODEL=
  STT_MODEL= TTS_MODEL= IMAGE_MODEL= IMG_SIZE=${IMG_SIZE:-256x256} READY_PATH= READY_JQ=
  "case_$name" || { record "$name" "ERROR" "case definition failed"; return 1; }
  for d in "${needs[@]}"; do
    [ -n "$d" ] && [ -e "$d" ] || missing="$missing $d"
  done
  for d in "${needs_env[@]}"; do
    [ -n "${!d:-}" ] || missing="$missing env:$d"
  done
  if [ -n "$missing" ]; then
    record "$name" "SKIP" "not on this machine:$missing"
    return 0
  fi

  d=$ROOT/$name
  rm -rf "$d"
  mkdir -p "$d/state" "$d/config" "$d/cache"
  STATE=$d/state CONF=$d/config CACHE=$d/cache
  # Each case gets its own engine ports, so a child still exiting cannot make
  # the next one fail to bind, and a parallel run stays off the user's 41100+.
  ENGINE_START=$E2E_ENGINE_NEXT
  E2E_ENGINE_NEXT=$((ENGINE_START + 20))
  {
    printf 'daemon:\n  port_range:\n    start: %s\n    end: %s\n' "$ENGINE_START" "$((ENGINE_START + 19))"
    printf '%s\n' "$CFG"
  } >"$d/config/config.yaml"
  PROXY=$PORT
  PORT=$((PORT + 1))
  say "== $name: $desc"
  say "   dir: $d  proxy: 127.0.0.1:$PROXY"

  # A busy box can make the detached daemon slower to bind than the client's
  # bounded wait, so poll for a live control plane before calling it a failure.
  local started= t
  for t in 1 2 3; do
    env_ls daemon start --proxy-port "$PROXY" >"$d/daemon-start.log" 2>&1 && { started=1; break; }
    for t in $(seq 20); do
      [ -n "$(state_of)" ] && { started=1; break; }
      sleep 2
    done
    [ -n "$started" ] && break
  done
  if [ -z "$started" ]; then
    record "$name" "FAIL" "daemon start failed: $(tail -3 "$d/daemon-start.log" | tr '\n' ' ')"
    return 1
  fi
  local rc=0
  if [ ${#LAUNCH[@]} -gt 0 ]; then
    if ! env_ls "${LAUNCH[@]}" >"$d/start.log" 2>&1; then
      record "$name" "FAIL" "start exited $?; $(tail -3 "$d/start.log" | tr '\n' ' ')"
      env_ls daemon stop >/dev/null 2>&1
      return 1
    fi
    if ! wait_ready; then
      record "$name" "FAIL" "not ready in ${WAIT_TIMEOUT}s (state: $(state_of)); $(daemon_log_tail)"
      env_ls stop --all --yes >/dev/null 2>&1
      env_ls daemon stop >/dev/null 2>&1
      return 1
    fi
  fi
  if $VERIFY; then
    record "$name" "PASS" "$EVIDENCE"
  else
    record "$name" "FAIL" "${EVIDENCE:-verify failed}; log: $(daemon_log_tail)"
    rc=1
  fi
  if [ "$KEEP" = 0 ]; then
    env_ls stop --all --yes >/dev/null 2>&1
    env_ls daemon stop >/dev/null 2>&1
  fi
  return $rc
}

usage() {
  sed -n '2,19p' "${BASH_SOURCE[0]}" | cut -c3-
  say
  declare -F | awk '$3 ~ /^case_/ {sub("case_", "", $3); print "  " $3}'
}

[ -x "$BIN" ] || { say "no binary at $BIN (cargo build --bin llamastash)"; exit 2; }
mkdir -p "$ROOT"

ARGS=()
while [ $# -gt 0 ]; do
  case $1 in
    --list | -h | --help) usage; exit 0 ;;
    --keep) KEEP=1 ;;
    --port-base) PORT=$2; shift ;;
    *) ARGS+=("$1") ;;
  esac
  shift
done
[ ${#ARGS[@]} -eq 0 ] && { usage; exit 2; }

overall=0
for c in "${ARGS[@]}"; do
  run_case "$c" || overall=1
done
exit $overall
