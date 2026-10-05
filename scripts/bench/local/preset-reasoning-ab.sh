#!/usr/bin/env bash
# What does reasoning effort cost, and is the cheaper setting good enough?
#
#   scripts/bench/preset-reasoning-ab.sh <llamastash-binary> <model> <preset>...
#
# Measures TIME TO A COMPLETE ANSWER, which is the number that decides an
# interactive coding loop and the one every other harness here misses. A t/s
# figure says nothing about a turn that spends its whole budget thinking and
# returns an empty string: measured on this model at xhigh, a 4000-token budget
# produced 0 characters on 2 of 3 real questions after 4-5 minutes of decoding.
#
# So the metric is: did a complete answer arrive (finish_reason == 'stop'), how
# long did it take wall-clock, and how much of the budget went to reasoning
# rather than answer. A preset that answers in 60 s at 'low' effort can beat one
# that thinks for 6 minutes and returns nothing, even if the second would have
# been more thorough had it finished.
#
# Answers are printed and saved so the quality half of the trade is judged by
# reading them, not assumed from the effort label.
set -uo pipefail

BIN="${1:?usage: preset-reasoning-ab.sh <binary> <model> <preset>...}"
MODEL="${2:?usage: preset-reasoning-ab.sh <binary> <model> <preset>...}"
shift 2
PRESETS=("$@")
[[ ${#PRESETS[@]} -ge 1 ]] || { echo "need at least one preset"; exit 2; }
MAX_TOKENS="${MAX_TOKENS:-8000}"   # must be generous: the point is to let it finish
CTX_BYTES="${CTX_BYTES:-40000}"

ROOT="${BENCH_ROOT:-$HOME/.cache/llamastash-reasoning}"
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

CTX_FILE="$ROOT/context.txt"
{ cat README.md docs/architecture.md docs/usage.md docs/troubleshooting.md \
    src/backend/mod.rs src/launch/params.rs 2>/dev/null; } | head -c "$CTX_BYTES" > "$CTX_FILE"

"$BIN" daemon stop >/dev/null 2>&1; sleep 1
start_daemon

echo "presets: ${PRESETS[*]}"
echo "budget:  max_tokens=$MAX_TOKENS   context: $(wc -c < "$CTX_FILE") bytes"
echo "power:   TDP $(z13ctl tdp --get 2>/dev/null | awk '/PL1/{print $3}')W  batt $(battery)%"
echo

for preset in "${PRESETS[@]}"; do
  wait_for_charge
  guard
  reset_state
  launch="$("$BIN" start "$MODEL" --preset "$preset" --wait --json 2>&1)"
  port="$(printf '%s' "$launch" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("port",""))' 2>/dev/null)"
  [[ -z "$port" ]] && { echo "### $preset: LAUNCH FAILED"; printf '%s\n' "$launch" | head -3; echo; continue; }
  echo "### $preset"
  PORT="$port" PRESET="$preset" MAX_TOKENS="$MAX_TOKENS" CTX_FILE="$CTX_FILE" \
  ANSWER_DIR="$ROOT/answers" python3 - <<'PY'
import json, os, time, urllib.request

port, preset = os.environ['PORT'], os.environ['PRESET']
maxtok, adir = int(os.environ['MAX_TOKENS']), os.environ['ANSWER_DIR']
URL = f'http://127.0.0.1:{port}/v1/chat/completions'
src = open(os.environ['CTX_FILE'], errors='replace').read()

QUESTIONS = [
    "What does this project do, and what are its three most important modules? Two sentences each.",
    "A preset saved from the TUI loses its `backend` and `server` fields while the CLI keeps them. "
    "What breaks for the user, and what is the fix?",
]

def ask(msgs):
    b = json.dumps({'model': 'q', 'messages': msgs, 'max_tokens': maxtok}).encode()
    r = urllib.request.Request(URL, b, {'Content-Type': 'application/json'})
    t0 = time.time()
    d = json.loads(urllib.request.urlopen(r, timeout=7200).read())
    ch = d['choices'][0]
    return ch['message'], ch.get('finish_reason', '?'), d.get('timings', {}), time.time() - t0

msgs = [{'role': 'user', 'content': 'You are a senior engineer reading this repository.\n\n'
                                    + src + '\n\n' + QUESTIONS[0]}]
complete = 0
for i, q in enumerate(QUESTIONS):
    if i:
        msgs = msgs + [{'role': 'assistant', 'content': prev or 'ok'}, {'role': 'user', 'content': q}]
    m, fin, t, wall = ask(msgs)
    prev = (m.get('content') or '').strip()
    think_txt = (m.get('reasoning_content') or '').strip()
    out = t.get('predicted_n', 0)
    # MTP acceptance is the number that decides whether raising temperature is
    # affordable here: upstream measured ~88% at temp 0 collapsing to ~25% at
    # temp 0.8, which drops decode back to unassisted speed.
    acc = 100 * t.get('draft_n_accepted', 0) / max(t.get('draft_n', 1), 1)
    ans_tok = len(prev) / 4
    think = len(think_txt) / 4 if think_txt else max(out - ans_tok, 0)
    done = (fin == 'stop' and prev != '')
    complete += done
    open(os.path.join(adir, f'{preset}-{i+1}.md'), 'w').write(
        f'# {preset} - Q{i+1}\n\n**Q:** {q}\n\n**finish:** {fin}  **wall:** {wall:.0f}s  '
        f'**out:** {out} tok\n\n---\n\n{prev}\n')
    print('  Q%d  %-9s wall %5.0fs  out %4d tok (~%4.0f think / ~%4.0f ans)  '
          '%5.2f t/s  accept %4.1f%%  ans %5dch  think %6dch  finish=%s'
          % (i + 1, 'COMPLETE' if done else 'CUT OFF', wall, out, think, ans_tok,
             t.get('predicted_per_second', 0), acc, len(prev), len(think_txt), fin))
    if prev:
        head = '\n'.join(prev.splitlines()[:6])
        print('\n'.join('      | ' + l for l in head.splitlines()))
    else:
        print('      | <NO ANSWER - %d chars of thinking, finish=%s>' % (len(think_txt), fin))
        if think_txt:
            tail = ' '.join(think_txt.split())[-200:]
            print('      | thinking tail: ...%s' % tail)
    print()
print('  >> %-12s complete answers %d/%d\n' % (preset, complete, len(QUESTIONS)))
PY
done

"$BIN" stop --all --yes >/dev/null 2>&1
"$BIN" daemon stop >/dev/null 2>&1
echo "answers: $ROOT/answers"
echo "REASONING_DONE"
