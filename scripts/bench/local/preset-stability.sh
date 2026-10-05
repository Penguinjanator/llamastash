#!/usr/bin/env bash
# Which preset survives a long session, and what does it cost in speed?
#
#   scripts/bench/preset-stability.sh <llamastash-binary> <model> <presetA> <presetB> [turns]
#
# Reports BOTH axes from one run, because the choice is a trade and neither
# number decides it alone: a preset that is 10% slower but finishes the session
# beats one that is quicker and dies at turn 40, while a preset that never fails
# and is half the speed is not obviously better either.
#
# preset-ab.sh answers "which is faster" over 4 turns. That sample is too short
# and too tidy to see a failure that happens every twentieth request, and a
# median hides a rate that decays as the context window fills -- so this script
# reports failures by kind, plus median decode AND first-half vs second-half
# decode over a long, growing session.
#
# Failure modes counted separately, because they have different causes:
#   crash    - the llama-server process is gone; every later turn fails
#   timeout  - no response within REQ_TIMEOUT; the server is wedged, not slow
#   http     - a 4xx/5xx or a connection refused
#   empty    - HTTP 200 with no content (a turn that produced nothing usable)
#
# The session deliberately mixes append turns with BRANCH turns that rewrite
# history, because a divergent tail forces a prompt-cache rollback -- the code
# path carrying the known MTP/checkpoint issues, and the one a preset with
# -ctxcp / --cache-reuse exercises but a preset without them never touches.
# Context grows monotonically so later turns run near the window limit, where
# KV pressure is highest.
set -uo pipefail

BIN="${1:?usage: preset-stability.sh <binary> <model> <presetA> <presetB> [turns]}"
MODEL="${2:?usage: preset-stability.sh <binary> <model> <presetA> <presetB> [turns]}"
PRESETS=("${3:?need presetA}" "${4:?need presetB}")
TURNS="${5:-40}"
REQ_TIMEOUT="${REQ_TIMEOUT:-240}"
MAX_TOKENS="${MAX_TOKENS:-400}"

ROOT="${BENCH_ROOT:-$HOME/.cache/llamastash-stability}"
SRC_CONFIG="${LS_REAL_CONFIG:-$HOME/.config/llamastash/config.yaml}"
export LLAMASTASH_STATE_DIR="$ROOT/state"
export LLAMASTASH_CONFIG_DIR="$ROOT/config"
export LLAMASTASH_CACHE_DIR="$ROOT/cache"
mkdir -p "$LLAMASTASH_STATE_DIR" "$LLAMASTASH_CONFIG_DIR" "$LLAMASTASH_CACHE_DIR"
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

"$BIN" daemon stop >/dev/null 2>&1; sleep 1
start_daemon

echo "presets: ${PRESETS[*]}   turns: $TURNS   req timeout: ${REQ_TIMEOUT}s"
echo "power:   TDP $(z13ctl tdp --get 2>/dev/null | awk '/PL1/{print $3}')W  batt $(battery)%"
echo

for preset in "${PRESETS[@]}"; do
  wait_for_charge
  guard
  reset_state
  launch="$("$BIN" start "$MODEL" --preset "$preset" --wait --json 2>&1)"
  port="$(printf '%s' "$launch" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("port",""))' 2>/dev/null)"
  if [[ -z "$port" ]]; then
    echo "### $preset: LAUNCH FAILED"; printf '%s\n' "$launch" | head -3; echo; continue
  fi
  srv_pid="$(pgrep -f "llama-server .*--port $port" | head -1)"
  log="$(ls -t "$LLAMASTASH_CACHE_DIR"/logs/*.log 2>/dev/null | head -1)"
  echo "### $preset  (server pid $srv_pid)"
  PORT="$port" PRESET="$preset" TURNS="$TURNS" SRV_PID="$srv_pid" \
  REQ_TIMEOUT="$REQ_TIMEOUT" MAX_TOKENS="$MAX_TOKENS" python3 - <<'PY'
import json, os, socket, time, urllib.error, urllib.request

port, preset = os.environ['PORT'], os.environ['PRESET']
turns, srv_pid = int(os.environ['TURNS']), int(os.environ['SRV_PID'])
timeout, maxtok = float(os.environ['REQ_TIMEOUT']), int(os.environ['MAX_TOKENS'])
URL = f'http://127.0.0.1:{port}/v1/chat/completions'

def alive(pid):
    try:
        os.kill(pid, 0); return True
    except OSError:
        return False

FILLER = '\n'.join(f'fn step_{i}(v: i64) -> i64 {{ v.wrapping_mul({i%13+2}) }}' for i in range(600))
msgs = [{'role': 'user', 'content':
         'Review this Rust module and answer follow-ups concisely.\n\n' + FILLER
         + '\n\nName one problem with this code.'}]

crash = timeouts = http = empty = ok = 0
first_fail = None
lat, dec, acc = [], [], []
for t in range(1, turns + 1):
    if not alive(srv_pid):
        crash += 1
        first_fail = first_fail or (t, 'crash')
        print('    turn %2d  CRASH — server process gone' % t)
        break
    # Every 5th turn branches: rewrite history so the tail diverges from the
    # cached prefix and the server must roll back to a checkpoint.
    if t % 5 == 0:
        body = msgs[:1] + [{'role': 'assistant', 'content': 'Acknowledged.'},
                           {'role': 'user', 'content':
                            f'Disregard the prior thread. Fresh question {t}: '
                            'name a different problem and justify it.'}]
    else:
        body = msgs + [{'role': 'user', 'content': f'Follow-up {t}: expand on that in two sentences.'}]
    req = urllib.request.Request(
        URL, json.dumps({'model': 'q', 'messages': body, 'max_tokens': maxtok}).encode(),
        {'Content-Type': 'application/json'})
    t0 = time.time()
    try:
        d = json.loads(urllib.request.urlopen(req, timeout=timeout).read())
        el = time.time() - t0
        txt = (d['choices'][0]['message'].get('content') or '').strip()
        tm = d.get('timings', {})
        lat.append(el)
        if tm.get('predicted_per_second'):
            dec.append(tm['predicted_per_second'])
            acc.append(100 * tm.get('draft_n_accepted', 0) / max(tm.get('draft_n', 1), 1))
        if not txt:
            empty += 1; first_fail = first_fail or (t, 'empty')
            print('    turn %2d  EMPTY (%.0fs)' % (t, el))
        else:
            ok += 1
            msgs = body + [{'role': 'assistant', 'content': txt}]
            if t % 10 == 0:
                print('    turn %2d  ok  %.0fs  %5.2f t/s  reprefill %5d tok  ctx~%d msgs'
                      % (t, el, tm.get('predicted_per_second', 0), tm.get('prompt_n', 0), len(msgs)))
    except (socket.timeout, TimeoutError):
        timeouts += 1; first_fail = first_fail or (t, 'timeout')
        print('    turn %2d  TIMEOUT after %.0fs' % (t, time.time() - t0))
    except urllib.error.HTTPError as e:
        http += 1; first_fail = first_fail or (t, f'http {e.code}')
        print('    turn %2d  HTTP %s' % (t, e.code))
    except Exception as e:
        http += 1; first_fail = first_fail or (t, type(e).__name__)
        print('    turn %2d  ERR %s' % (t, type(e).__name__))

fails = crash + timeouts + http + empty
def med(xs): return sorted(xs)[len(xs)//2] if xs else 0
# Speed is reported early-vs-late as well as median: a preset that starts fast
# and degrades as the window fills is a different proposition from one that
# holds its rate, and the median hides exactly that.
half = max(len(dec)//2, 1)
early, late = med(dec[:half]), med(dec[half:])
print('  >> %-10s STABILITY  ok %d/%d  fails %d  (crash %d, timeout %d, http %d, empty %d)  '
      'first failure: %s  server %s'
      % (preset, ok, turns, fails, crash, timeouts, http, empty,
         ('turn %d %s' % first_fail) if first_fail else 'none',
         'alive' if alive(srv_pid) else 'DEAD'))
print('  >> %-10s SPEED      median %5.2f t/s  (first half %5.2f -> second half %5.2f, %+.0f%%)  '
      'median latency %.0fs  accept %4.1f%%'
      % (preset, med(dec), early, late,
         (100*(late-early)/early) if early else 0, med(lat), med(acc)))
PY
  # Engine-side evidence for whatever the client saw.
  if [[ -n "$log" && -f "$log" ]]; then
    hits="$(grep -ciE 'error|abort|assert|out of memory|failed|mismatch|terminate' "$log" 2>/dev/null || echo 0)"
    echo "     server log: $hits error-ish lines  ($log)"
    grep -iE 'error|abort|assert|out of memory|failed|mismatch|terminate' "$log" 2>/dev/null | tail -3 | sed 's/^/       /'
  fi
  echo
done

"$BIN" stop --all --yes >/dev/null 2>&1
"$BIN" daemon stop >/dev/null 2>&1
echo "STABILITY_DONE"
