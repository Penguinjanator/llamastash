#!/usr/bin/env bash
# Run a real coding-agent session against two presets and keep the answers.
#
#   scripts/bench/preset-coding-task.sh <llamastash-binary> <model-substring> <presetA> <presetB>
#
# preset-ab.sh answers "which is faster" with a synthetic prompt. This answers
# "which would I rather code with": real repo context (README + architecture doc
# + the backend registry, ~44k tokens), real questions, and a token budget large
# enough that the reasoning block finishes and an actual answer comes out. A cap
# that truncates every turn measures the cap, not the model.
#
# Answers are written to $ROOT/answers/<preset>-<n>.md so quality can be compared
# by reading them, not inferred from throughput.
#
# Same two traps as preset-ab.sh: state.json is wiped between launches so
# last_params cannot bleed one preset's knobs into the other, and whichever
# preset is passed first gets the cool GPU -- pass them in the order that
# disadvantages the one you expect to win.
set -uo pipefail

BIN="${1:?usage: preset-coding-task.sh <binary> <model> <presetA> <presetB>}"
MODEL="${2:?usage: preset-coding-task.sh <binary> <model> <presetA> <presetB>}"
PRESETS=("${3:?need presetA}" "${4:?need presetB}")
MAX_TOKENS=4000

ROOT="${BENCH_ROOT:-$HOME/.cache/llamastash-coding-task}"
SRC_CONFIG="${LS_REAL_CONFIG:-$HOME/.config/llamastash/config.yaml}"
export LLAMASTASH_STATE_DIR="$ROOT/state"
export LLAMASTASH_CONFIG_DIR="$ROOT/config"
export LLAMASTASH_CACHE_DIR="$ROOT/cache"
mkdir -p "$LLAMASTASH_STATE_DIR" "$LLAMASTASH_CONFIG_DIR" "$LLAMASTASH_CACHE_DIR" "$ROOT/answers"
cp -L "$SRC_CONFIG" "$LLAMASTASH_CONFIG_DIR/config.yaml"

. "$(dirname "$0")/lib-power.sh"
power_watchdog_start "$$"
power_traps_install

start_daemon() { "$BIN" daemon start --force >/dev/null 2>&1; sleep 3; }

reset_state() {
  "$BIN" stop --all --yes >/dev/null 2>&1
  "$BIN" daemon stop    >/dev/null 2>&1; sleep 1
  rm -f "$LLAMASTASH_STATE_DIR/state.json"
  start_daemon
}

# Real repo material, the shape an agent actually carries: prose docs plus one
# large source file it would have to reason over.
CTX_FILE="$ROOT/context.txt"
{
  echo "===== README.md =====";            cat README.md
  echo; echo "===== docs/architecture.md ====="; cat docs/architecture.md
  echo; echo "===== src/backend/mod.rs ====="; cat src/backend/mod.rs
} > "$CTX_FILE" 2>/dev/null

"$BIN" daemon stop >/dev/null 2>&1; sleep 1
start_daemon

echo "context: $(wc -c < "$CTX_FILE") bytes of real repo material"
echo "model:   $MODEL"
echo "presets: ${PRESETS[*]}   (first one runs on the cool GPU)"
echo "budget:  max_tokens=$MAX_TOKENS"
echo "power:   TDP $(z13ctl tdp --get 2>/dev/null | awk '/PL1/{print $3}')W  AC $(on_ac)  battery $(battery)%"
echo

for preset in "${PRESETS[@]}"; do
  wait_for_charge
  guard
  reset_state
  launch="$("$BIN" start "$MODEL" --preset "$preset" --wait --json 2>&1)"
  port="$(printf '%s' "$launch" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("port",""))' 2>/dev/null)"
  if [[ -z "$port" ]]; then
    echo "$preset: LAUNCH FAILED"; printf '%s\n' "$launch" | head -5; continue
  fi
  srv_pid="$(pgrep -f "llama-server .*--port $port" | head -1)"
  echo "### $preset"
  echo "  knobs: $(tr '\0' '\n' < /proc/$srv_pid/cmdline 2>/dev/null \
             | grep -vE '^/|gguf$|^--host$|^127|^--port$|^[0-9]+$|^-m$' | paste -sd' ' | cut -c1-400)"
  PORT="$port" PRESET="$preset" MAX_TOKENS="$MAX_TOKENS" CTX_FILE="$CTX_FILE" \
  ANSWER_DIR="$ROOT/answers" BATT="$(battery)" AC="$(on_ac)" python3 - <<'PY'
import json, os, time, urllib.request

port, preset = os.environ['PORT'], os.environ['PRESET']
maxtok = int(os.environ['MAX_TOKENS'])
adir, batt, ac = os.environ['ANSWER_DIR'], os.environ['BATT'], os.environ['AC']
URL = f'http://127.0.0.1:{port}/v1/chat/completions'
src = open(os.environ['CTX_FILE'], errors='replace').read()

def call(msgs, max_tokens):
    body = json.dumps({'model': 'q', 'messages': msgs, 'max_tokens': max_tokens}).encode()
    req = urllib.request.Request(URL, body, {'Content-Type': 'application/json'})
    t0 = time.time()
    d = json.loads(urllib.request.urlopen(req, timeout=7200).read())
    ch = d['choices'][0]
    return ch['message'], ch.get('finish_reason', '?'), d.get('timings', {}), time.time() - t0

QUESTIONS = [
    "Summarize this codebase: what it does, its main modules, and how they fit together. "
    "Be concrete and cite module paths.",
    "I want to add a new backend. Which files do I have to touch, what is the contract "
    "each one imposes, and what is the rule about where a backend's name may appear?",
    "In the preset save path, the TUI drops `backend` and `server` while the CLI sends them. "
    "Explain what breaks for a user because of that, and what the fix is.",
]

msgs = [{'role': 'user',
         'content': 'You are a senior engineer reading an unfamiliar Rust repository.\n\n'
                    + src + '\n\n' + QUESTIONS[0]}]
first = True
for i, q in enumerate(QUESTIONS):
    if not first:
        msgs = msgs + [{'role': 'assistant', 'content': prev or 'ok'},
                       {'role': 'user', 'content': q}]
    m, fin, t, wall = call(msgs, maxtok)
    prev = (m.get('content') or '').strip()
    first = False
    path = os.path.join(adir, f'{preset}-{i+1}.md')
    with open(path, 'w') as fh:
        fh.write(f'# {preset} — Q{i+1}\n\n**Q:** {q}\n\n**finish:** {fin}  '
                 f'**wall:** {wall:.1f}s  **out:** {t.get("predicted_n", 0)} tok\n\n---\n\n{prev}\n')
    print('\n  ' + '-' * 76)
    print('  Q%d: %s' % (i + 1, q))
    print('  prefill %6d tok %6.1fs   out %4d tok  decode %6.2f t/s   '
          'accept %5.1f%%   wall %6.1fs   %5d chars  finish=%s'
          % (t.get('prompt_n', 0), t.get('prompt_ms', 0) / 1000,
             t.get('predicted_n', 0), t.get('predicted_per_second', 0),
             100 * t.get('draft_n_accepted', 0) / max(t.get('draft_n', 1), 1),
             wall, len(prev), fin))
    print('  ' + '-' * 76)
    # The answers are the point of this script, so they go to stdout too -- a
    # throughput number cannot tell you which preset reasons better.
    print('\n'.join('  ' + l for l in prev.splitlines()) if prev else '  <EMPTY ANSWER>')
    print()
print('  answers -> %s/%s-*.md   batt %s%% ac %s' % (adir, preset, batt, ac))
print()
PY
done

"$BIN" stop --all --yes >/dev/null 2>&1
"$BIN" daemon stop >/dev/null 2>&1
echo "answers: $ROOT/answers   (real config untouched)"
