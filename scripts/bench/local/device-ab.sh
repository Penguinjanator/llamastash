#!/usr/bin/env bash
# A/B two llama.cpp devices on the same preset, alternating to cancel thermal drift.
#
# Answers "is Vulkan0 or ROCm0 the right pin for this model?" with warm-cache
# decode, which is what a coding agent actually experiences once the prompt
# cache is primed. Runs in a dedicated config + state dir so it never touches
# the user's real llamastash setup.
#
#   scripts/bench/device-ab.sh <llamastash-binary> <model-substring> [rounds]
#
# Each round, per device: launch, prime the cache with a long prompt, then take
# TURNS measured follow-up turns that hit the cache. Reports every sample plus a
# median, and battery level per round because throughput on this box has halved
# on low battery while platform_profile still read "performance".
set -uo pipefail

BIN="${1:?usage: device-ab.sh <binary> <model> [rounds]}"
MODEL="${2:?usage: device-ab.sh <binary> <model> [rounds]}"
ROUNDS="${3:-2}"
DEVICES=(Vulkan0 ROCm0)
TURNS=4
WARM_FNS=500          # ~16k tokens of prompt to prime the cache
PORT=41100

ROOT="${BENCH_ROOT:-$HOME/.cache/llamastash-device-ab}"
export LLAMASTASH_STATE_DIR="$ROOT/state"
export LLAMASTASH_CONFIG_DIR="$ROOT/config"
export LLAMASTASH_CACHE_DIR="$ROOT/cache"
mkdir -p "$LLAMASTASH_STATE_DIR" "$LLAMASTASH_CONFIG_DIR" "$LLAMASTASH_CACHE_DIR"

# pi-cache settings verbatim; only `device:` differs between the two entries.
emit_preset() {
  cat <<YAML
      bench-${1,,}:
        mode: chat
        ctx: 131072
        reasoning: true
        device: $1
        n_gpu_layers: 99
        flash_attn: true
        parallel: 1
        threads: 16
        batch_size: 2048
        ubatch_size: 1024
        cache_type_k: q8_0
        cache_type_v: q8_0
        no_mmap: true
        mtp: "on"
        mtp_draft_n: 4
        extras:
          - --spec-draft-p-min
          - "0.0"
          - --spec-mtp-strict-qwen
          - --chat-template-kwargs
          - '{"reasoning_effort":"medium","preserve_thinking":false}'
          - --reasoning-budget
          - "8192"
          - -ctxcp
          - "64"
          - -cpent
          - "4096"
          - -cram
          - "32768"
          - --cache-prompt
          - --cache-reuse
          - "256"
          - --cont-batching
          - --kv-unified
          - --poll
          - "100"
          - --temperature
          - "1.0"
          - --top-p
          - "0.95"
          - --top-k
          - "20"
YAML
}

{
  echo "backend:"
  echo "  llamacpp:"
  echo "    servers:"
  echo "      - binary: ${LLAMA_SERVER_SHIM:-/home/deepu/.local/bin/q38rocm-llama-server}"
  echo "        name: ROCmFP4"
  echo "presets:"
  echo "  Qwen3.8-27B-ROCmFP4-FAST.gguf:"
  echo "    entries:"
  for d in "${DEVICES[@]}"; do emit_preset "$d"; done
} > "$LLAMASTASH_CONFIG_DIR/config.yaml"

battery() { cat /sys/class/power_supply/BAT0/capacity 2>/dev/null || echo '?'; }

"$BIN" daemon stop >/dev/null 2>&1; sleep 1
"$BIN" daemon start >/dev/null 2>&1; sleep 3

echo "model:  $MODEL"
echo "config: $LLAMASTASH_CONFIG_DIR/config.yaml"
echo "rounds: $ROUNDS x ${#DEVICES[@]} devices x $TURNS warm turns"
echo

for r in $(seq 1 "$ROUNDS"); do
  for dev in "${DEVICES[@]}"; do
    preset="bench-${dev,,}"
    "$BIN" stop --all --yes >/dev/null 2>&1
    if ! "$BIN" start "$MODEL" --server llamacpp-ROCmFP4 --preset "$preset" --wait >/dev/null 2>&1; then
      echo "round $r $dev: LAUNCH FAILED"; continue
    fi
    PORT="$PORT" DEV="$dev" ROUND="$r" TURNS="$TURNS" WARM_FNS="$WARM_FNS" BATT="$(battery)" python3 - <<'PY'
import json, os, statistics, urllib.request

port, dev, rnd = os.environ['PORT'], os.environ['DEV'], os.environ['ROUND']
turns, warm_fns, batt = int(os.environ['TURNS']), int(os.environ['WARM_FNS']), os.environ['BATT']
URL = f'http://127.0.0.1:{port}/v1/chat/completions'

src = '\n'.join(
    'fn helper_%d(x: i32) -> i32 { x.wrapping_mul(%d).wrapping_add(%d) }' % (i, i % 7 + 2, i)
    for i in range(warm_fns))
warm = 'Review this Rust module and name its most repetitive pattern.\n\n' + src

def call(msgs, max_tokens):
    body = json.dumps({'model': 'q', 'messages': msgs,
                       'max_tokens': max_tokens, 'temperature': 0}).encode()
    req = urllib.request.Request(URL, body, {'Content-Type': 'application/json'})
    d = json.loads(urllib.request.urlopen(req, timeout=3600).read())
    return d['choices'][0]['message'], d.get('timings', {})

msgs = [{'role': 'user', 'content': warm}]
m, t = call(msgs, 80)
print('round %s %-8s prime: %d tok prefill in %5.1fs (%.0f t/s)'
      % (rnd, dev, t.get('prompt_n', 0), t.get('prompt_ms', 0) / 1000,
         t.get('prompt_per_second', 0)))

follow = ['Now suggest a macro removing that duplication.',
          'Write one unit test for the macro.',
          'What is the time complexity of the generated code?',
          'Rewrite the macro to accept a custom operator.']
dec, acc = [], []
for i in range(turns):
    msgs = msgs + [{'role': 'assistant', 'content': m.get('content') or 'ok'},
                   {'role': 'user', 'content': follow[i % len(follow)]}]
    m, t = call(msgs, 300)
    a = 100 * t.get('draft_n_accepted', 0) / max(t.get('draft_n', 1), 1)
    dec.append(t.get('predicted_per_second', 0)); acc.append(a)
    print('    turn %d  cached %6s  prefill %5.1fs  decode %6.2f t/s  accept %5.1f%%'
          % (i + 1, t.get('prompt_n', 0), t.get('prompt_ms', 0) / 1000, dec[-1], a))
print('  >> %-8s median decode %6.2f t/s   median accept %5.1f%%   battery %s%%'
      % (dev, statistics.median(dec), statistics.median(acc), batt))
print()
PY
  done
done

"$BIN" stop --all --yes >/dev/null 2>&1
"$BIN" daemon stop >/dev/null 2>&1
echo "sandbox: $ROOT"
