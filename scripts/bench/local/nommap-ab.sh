#!/usr/bin/env bash
# A/B the `no-mmap` knob for one model + base preset: arms are the preset
# verbatim with no-mmap forced on and forced off, everything else identical.
#
# Complements preset-ab.sh (which compares whole presets) with the two numbers
# preset-ab cannot see: cold load time (spawn -> ready, from `start --wait`)
# and peak GTT residency, sampled from the amdgpu sysfs node every 2 s. RSS is
# useless on this UMA box -- it omits GTT entirely -- and /proc/meminfo
# MemAvailable + Cached are sampled alongside to tell the two load paths apart:
# mmap parks weights in reclaimable page cache, no-mmap reads them into anon
# memory (MemAvailable drops, Cached does not move).
#
#   scripts/bench/local/nommap-ab.sh <llamastash-binary> <model-substring> <base-preset>
#   ROUNDS=4 scripts/bench/local/nommap-ab.sh ...        # default 2, keep even
#
# Arms are generated as named presets (ab-mmap / ab-nommap) in a sandbox copy
# of the real config, so a preset that already pins no-mmap gets a clean
# "removed" arm instead of relying on CLI override layering. The sandbox config
# is a PyYAML round-trip: comments are dropped, the real file is never touched.
#
# Same last_params and order-bias traps as preset-ab.sh; see its header. The
# daemon is sandboxed (own state/cache dirs, non-default --proxy-port) so the
# user's real daemon is never touched.
set -uo pipefail

BIN="${1:?usage: nommap-ab.sh <binary> <model> <base-preset> }"
MODEL="${2:?usage: nommap-ab.sh <binary> <model> <base-preset> }"
BASE_PRESET="${3:?usage: nommap-ab.sh <binary> <model> <base-preset> }"
ROUNDS="${ROUNDS:-2}"
TURNS="${TURNS:-4}"
CTX_BYTES="${CTX_BYTES:-64000}"
MAX_TOKENS="${MAX_TOKENS:-600}"

ROOT="${BENCH_ROOT:-$HOME/.cache/llamastash-nommap-ab}"
SRC_CONFIG="${LS_REAL_CONFIG:-$HOME/.config/llamastash/config.yaml}"
GTT_NODE="${GTT_NODE:-/sys/class/drm/card1/device/mem_info_gtt_used}"
SANDBOX_PROXY_PORT="${SANDBOX_PROXY_PORT:-41990}"
export LLAMASTASH_STATE_DIR="$ROOT/state"
export LLAMASTASH_CONFIG_DIR="$ROOT/config"
export LLAMASTASH_CACHE_DIR="$ROOT/cache"
mkdir -p "$LLAMASTASH_STATE_DIR" "$LLAMASTASH_CONFIG_DIR" "$LLAMASTASH_CACHE_DIR" "$ROOT/gtt"

# The prompt driver resolves repo sources relative to this script's repo root.
REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

cp -L "$SRC_CONFIG" "$LLAMASTASH_CONFIG_DIR/config.yaml.in"
MODEL_KEY="$(MODEL="$MODEL" BASE_PRESET="$BASE_PRESET" \
  SRC="$LLAMASTASH_CONFIG_DIR/config.yaml.in" DST="$LLAMASTASH_CONFIG_DIR/config.yaml" \
  python3 - <<'PY'
import copy, os, sys, yaml

cfg = yaml.safe_load(open(os.environ['SRC']))
model, base = os.environ['MODEL'], os.environ['BASE_PRESET']
matches = [k for k in cfg.get('presets', {}) if model in k]
if len(matches) != 1:
    sys.exit(f"model substring {model!r} matched {matches}")
key = matches[0]
entries = cfg['presets'][key]['entries']
if base not in entries:
    sys.exit(f"{base!r} not in {key} entries: {sorted(entries)}")
mmap_arm, nommap_arm = copy.deepcopy(entries[base]), copy.deepcopy(entries[base])
for arm in (mmap_arm, nommap_arm):
    arm.setdefault('knobs', {})
mmap_arm['knobs'].pop('no-mmap', None)   # absent knob -> engine default (mmap)
nommap_arm['knobs']['no-mmap'] = True
entries['ab-mmap'] = mmap_arm
entries['ab-nommap'] = nommap_arm
yaml.safe_dump(cfg, open(os.environ['DST'], 'w'), sort_keys=False, allow_unicode=True)
print(key)
PY
)" || exit 2
echo "model:   $MODEL_KEY  (base preset: $BASE_PRESET)"
echo "arms:    ab-mmap [no-mmap absent] vs ab-nommap [no-mmap: true]; server pin: $(python3 -c "import yaml,os; e=yaml.safe_load(open('$LLAMASTASH_CONFIG_DIR/config.yaml'))['presets']['$MODEL_KEY']['entries']['ab-mmap']; print(e.get('server', 'default'))")"

. "$(dirname "$0")/lib-power.sh"
power_watchdog_start "$$"
power_traps_install

start_daemon() { "$BIN" daemon start --force --proxy-port "$SANDBOX_PROXY_PORT" >/dev/null 2>&1; sleep 3; }

reset_state() {
  "$BIN" stop --all --yes >/dev/null 2>&1
  "$BIN" daemon stop    >/dev/null 2>&1; sleep 1
  start_daemon
}

gtt_bytes() { cat "$GTT_NODE" 2>/dev/null || echo 0; }

# A stopped launch only hands GTT back when the process is really gone; the
# next arm's peak is garbage if the previous model still holds memory.
wait_gtt_idle() {
  local deadline=$((SECONDS + 60))
  while (( SECONDS < deadline )); do
    (( $(gtt_bytes) < 536870912 )) && return 0   # < 512 MiB
    sleep 2
  done
  echo "  !! GTT still $(( $(gtt_bytes) / 1073741824 )) GiB after 60s -- peak below may carry over"
}

start_sampler() { # $1: csv path. Self-terminates when this script dies.
  ( while kill -0 "$MAIN_PID" 2>/dev/null; do
      gtt="$(gtt_bytes)"
      read -r _ avail _ < <(awk '/^MemAvailable/{print $1,$2,$3}' /proc/meminfo)
      read -r _ cached _ < <(awk '/^Cached:/{print $1,$2,$3}' /proc/meminfo)
      printf '%s,%s,%s,%s\n' "$(date +%s)" "$gtt" "${avail:-0}" "${cached:-0}" >> "$1"
      sleep 2
    done ) &
  SAMPLER_PID=$!
}

stop_sampler() { # $1: csv path, $2: baseline bytes -> prints peak + delta
  [[ -n "${SAMPLER_PID:-}" ]] && kill "$SAMPLER_PID" 2>/dev/null
  SAMPLER_PID=""
  sleep 1
  python3 - "$1" "$2" <<'PY'
import sys
rows = [l.split(',') for l in open(sys.argv[1]).read().splitlines() if l]
base = int(sys.argv[2])
if not rows:
    print("  !! no GTT samples captured"); sys.exit()
peak = max(int(r[1]) for r in rows)
last_avail = min(int(r[2]) for r in rows)
last_cached = max(int(r[3]) for r in rows)
G = 1073741824
print("  gtt  baseline %6.2f GiB   peak %6.2f GiB   delta %6.2f GiB   "
      "(min MemAvailable %.1f GiB, max Cached %.1f GiB)"
      % (base/G, peak/G, (peak-base)/G, last_avail/1048576, last_cached/1048576))
PY
}

MAIN_PID=$$
CTX_FILE="$ROOT/context.txt"
find "$REPO_ROOT/src" -name '*.rs' -size +4k | sort | xargs cat 2>/dev/null | head -c "$CTX_BYTES" > "$CTX_FILE"

"$BIN" daemon stop >/dev/null 2>&1; sleep 1
start_daemon

echo "context: $(wc -c < "$CTX_FILE") bytes of real source"
echo "rounds:  $ROUNDS x 2 arms x $TURNS turns, max_tokens=$MAX_TOKENS"
echo "power:   TDP $(z13ctl tdp --get 2>/dev/null | awk '/PL1/{print $3}')W  profile $(cat /sys/firmware/acpi/platform_profile)  AC $(on_ac)  battery $(battery)%"
echo

for r in $(seq 1 "$ROUNDS"); do
  if (( r % 2 == 1 )); then order=(ab-mmap ab-nommap); else order=(ab-nommap ab-mmap); fi
  for arm in "${order[@]}"; do
    wait_for_charge
    guard
    reset_state
    wait_gtt_idle
    baseline="$(gtt_bytes)"
    csv="$ROOT/gtt/r${r}-${arm}.csv"
    start_sampler "$csv"

    t0=$SECONDS
    launch="$("$BIN" start "$MODEL" --preset "$arm" --wait --json 2>&1)"
    load_s=$(( SECONDS - t0 ))
    port="$(printf '%s' "$launch" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("port",""))' 2>/dev/null)"
    if [[ -z "$port" ]]; then
      echo "round $r $arm: LAUNCH FAILED after ${load_s}s"
      printf '%s\n' "$launch" | head -5
      stop_sampler "$csv" "$baseline"
      echo
      continue
    fi
    srv_pid="$(pgrep -f "llama-server .*--port $port" | head -1)"
    echo "round $r $arm  load ${load_s}s  pid ${srv_pid:-?}  engine $(readlink /proc/$srv_pid/exe 2>/dev/null || echo '?')"
    flags="$(tr '\0' '\n' < /proc/$srv_pid/cmdline 2>/dev/null | grep -xE -- '--no-mmap|--mlock|--lazy-mode|-lm|--direct-io' | tr '\n' ' ')"
    echo "  mmap argv: ${flags:-none}"

    PORT="$port" PRESET="$arm" ROUND="$r" LOAD_S="$load_s" TURNS="$TURNS" MAX_TOKENS="$MAX_TOKENS" \
    CTX_FILE="$CTX_FILE" BATT="$(battery)" AC="$(on_ac)" python3 - <<'PY'
import json, os, statistics, time, urllib.request

port, preset, rnd = os.environ['PORT'], os.environ['PRESET'], os.environ['ROUND']
turns, batt, ac = int(os.environ['TURNS']), os.environ['BATT'], os.environ['AC']
maxtok = int(os.environ['MAX_TOKENS'])
URL = f'http://127.0.0.1:{port}/v1/chat/completions'
src = open(os.environ['CTX_FILE'], errors='replace').read()

def call(msgs, max_tokens):
    body = json.dumps({'model': 'q', 'messages': msgs, 'max_tokens': max_tokens}).encode()
    req = urllib.request.Request(URL, body, {'Content-Type': 'application/json'})
    t0 = time.time()
    d = json.loads(urllib.request.urlopen(req, timeout=3600).read())
    ch = d['choices'][0]
    return ch['message'], ch.get('finish_reason', '?'), d.get('timings', {}), time.time() - t0

warm = ('You are reviewing this Rust codebase. Answer briefly.\n\n' + src
        + '\n\nName the single most repeated pattern above.')
msgs = [{'role': 'user', 'content': warm}]
m, fin, t, wall = call(msgs, maxtok)
print('  COLD  %6d tok prefill  %6.1fs  (%5.0f t/s)  wall %5.1fs'
      % (t.get('prompt_n', 0), t.get('prompt_ms', 0) / 1000,
         t.get('prompt_per_second', 0), wall))

follow = ['Suggest one refactor that removes that duplication.',
          'Write a unit test for your suggestion.',
          'What edge case would that test miss?',
          'Summarize your three answers in two lines.']
dec, acc, pre, walls = [], [], [], []
for i in range(turns):
    msgs = msgs + [{'role': 'assistant', 'content': m.get('content') or 'ok'},
                   {'role': 'user', 'content': follow[i % len(follow)]}]
    m, fin, t, wall = call(msgs, maxtok)
    a = 100 * t.get('draft_n_accepted', 0) / max(t.get('draft_n', 1), 1)
    dec.append(t.get('predicted_per_second', 0)); acc.append(a)
    pre.append(t.get('prompt_ms', 0) / 1000); walls.append(wall)
    print('    turn %d  reprefill %6d tok %5.1fs   decode %6.2f t/s   accept %5.1f%%   wall %5.1fs'
          % (i + 1, t.get('prompt_n', 0), pre[-1], dec[-1], a, wall))
print('  >> %-10s decode %6.2f t/s  reprefill %4.1fs  wall %5.1fs  accept %4.1f%%  batt %s%% ac %s'
      % (preset, statistics.median(dec), statistics.median(pre), statistics.median(walls),
         statistics.median(acc), batt, ac))
PY
    sleep 2   # let the sampler catch the settled residency, not just the peak
    stop_sampler "$csv" "$baseline"
    echo
  done
done

"$BIN" stop --all --yes >/dev/null 2>&1
"$BIN" daemon stop >/dev/null 2>&1
echo "sandbox: $ROOT   gtt csvs: $ROOT/gtt/   (real config untouched)"
