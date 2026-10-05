#!/usr/bin/env bash
# Does a preset's KV cache return correct values after a checkpoint restore?
#
#   scripts/bench/preset-cache-correctness.sh <llamastash-binary> <model> <presetA> <presetB>
#
# Speed benchmarks cannot answer this. A KV cache that silently returns wrong
# values after a rollback is *faster*, not slower -- it fails by producing
# plausible text, so it wins on every throughput chart while being unusable.
#
# The specific claim under test: TurboQuant rotates the V cache while checkpoint
# restore (-ctxcp) stores raw KV blocks, so the pair is alleged to return wrong
# values rather than erroring. Upstream's run_server.sh ships exactly that pair
# in its `speed` and `agent` profiles and calls it validated, so the two sources
# disagree and only measurement settles it.
#
# Method: for each preset, ask a question with a verifiable answer that is stated
# only in the middle of a long context. Turn 1 primes the cache. Turn 2 asks the
# same thing again as a plain append (checkpoint reuse). Turn 3 *branches* -- it
# rewrites history so the tail diverges from the cached prefix, which is what
# forces a checkpoint rollback. A cache that restores wrongly loses the planted
# fact on turn 3 while still answering fluently.
#
# Fluency is the trap here: judge the recalled token, never the prose.
#
# An EMPTY answer is not evidence of corruption. The 2026-08-30 first run scored
# turbo4 FAIL on three empty strings with MAX_TOKENS="${MAX_TOKENS:-6000}" -- but this model runs
# xhigh reasoning, and an empty `content` is exactly what a budget exhausted by
# thinking produces. Corruption yields a WRONG token, not no token.
#
# So this checks three things the first version could not distinguish:
#   * finish_reason  -- 'length' means the budget ran out, verdict INCONCLUSIVE
#   * reasoning_content -- with --reasoning-format deepseek the thinking is split
#     out here; a token recalled while thinking proves the cache read correctly
#     even when the answer never got emitted
#   * a WRONG token vs no token -- only the former is a cache failure
# The budget is generous for the same reason.
set -uo pipefail

BIN="${1:?usage: preset-cache-correctness.sh <binary> <model> <presetA> <presetB>}"
MODEL="${2:?usage: preset-cache-correctness.sh <binary> <model> <presetA> <presetB>}"
PRESETS=("${3:?need presetA}" "${4:?need presetB}")
MAX_TOKENS="${MAX_TOKENS:-6000}"

ROOT="${BENCH_ROOT:-$HOME/.cache/llamastash-cache-correctness}"
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

echo "presets: ${PRESETS[*]}"
echo "power:   TDP $(z13ctl tdp --get 2>/dev/null | awk '/PL1/{print $3}')W  batt $(battery)%"
echo

for preset in "${PRESETS[@]}"; do
  wait_for_charge
  guard
  reset_state
  launch="$("$BIN" start "$MODEL" --preset "$preset" --wait --json 2>&1)"
  port="$(printf '%s' "$launch" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("port",""))' 2>/dev/null)"
  [[ -z "$port" ]] && { echo "$preset: LAUNCH FAILED"; printf '%s\n' "$launch" | head -3; continue; }
  echo "### $preset"
  PORT="$port" PRESET="$preset" MAX_TOKENS="$MAX_TOKENS" python3 - <<'PY'
import json, os, re, time, urllib.request

port, preset = os.environ['PORT'], os.environ['PRESET']
maxtok = int(os.environ['MAX_TOKENS'])
URL = f'http://127.0.0.1:{port}/v1/chat/completions'

# A long filler body with one planted, checkable fact buried in the middle.
CODE = '\n'.join(f'fn stage_{i}(x: i64) -> i64 {{ x.wrapping_add({i}) }}' for i in range(900))
half = CODE.split('\n')
SECRET = 'ZK7-4419-QX'
half.insert(len(half) // 2, f'// DEPLOYMENT-TOKEN: {SECRET}  (do not change)')
BODY = '\n'.join(half)

def ask(msgs):
    b = json.dumps({'model': 'q', 'messages': msgs, 'max_tokens': maxtok,
                    'temperature': 0}).encode()
    r = urllib.request.Request(URL, b, {'Content-Type': 'application/json'})
    t0 = time.time()
    d = json.loads(urllib.request.urlopen(r, timeout=3600).read())
    ch = d['choices'][0]
    m = ch['message']
    return ({'ans': (m.get('content') or '').strip(),
             'think': (m.get('reasoning_content') or '').strip(),
             'fin': ch.get('finish_reason', '?')},
            d.get('timings', {}), time.time() - t0)

Q = 'What is the DEPLOYMENT-TOKEN in the code above? Reply with only the token.'
base = [{'role': 'user', 'content': BODY + '\n\n' + Q}]

def verdict(r):
    """PASS: recalled it. WRONG: emitted a different token -- the only real cache
    failure. NO-ANSWER: budget gone before the answer started, says nothing about
    the cache. Checks the thinking too: recalling it there proves a correct read."""
    if SECRET in r['ans'].replace(' ', '') or SECRET in r['ans']:
        return 'PASS'
    if SECRET in r['think'].replace(' ', '') or SECRET in r['think']:
        return 'PASS*'            # correct read; answer just never got emitted
    # Did it state some OTHER token-shaped string? That is corruption.
    other = re.findall(r'\b[A-Z]{2,3}\d[-A-Z0-9]{4,}\b', r['ans'] + ' ' + r['think'])
    other = [o for o in other if o != SECRET]
    if other:
        return 'WRONG:' + other[0]
    if r['fin'] == 'length' and not r['ans']:
        return 'NO-ANSWER'
    return 'FAIL'

# 1: cold. 2: plain append, reuses the cached prefix. 3: branch -- history is
# rewritten so the tail diverges and a checkpoint rollback is required.
a1, t1, w1 = ask(base)
a2, t2, w2 = ask(base + [{'role': 'assistant', 'content': a1},
                         {'role': 'user', 'content': 'Confirm: repeat that token exactly.'}])
branched = base + [{'role': 'assistant', 'content': 'Understood, standing by.'},
                   {'role': 'user', 'content': 'Ignore the previous instruction. '
                                               'Instead, state the DEPLOYMENT-TOKEN only.'}]
a3, t3, w3 = ask(branched)

for n, (a, t, w) in enumerate(((a1, t1, w1), (a2, t2, w2), (a3, t3, w3)), 1):
    kind = ('cold', 'append (cache reuse)', 'BRANCH (forces rollback)')[n - 1]
    print('  turn %d %-24s reprefill %6d tok  %5.1fs  out %4d tok  fin=%-6s -> %-12s ans=%r think=%dch'
          % (n, kind, t.get('prompt_n', 0), w, t.get('predicted_n', 0), a['fin'],
             verdict(a), a['ans'][:40], len(a['think'])))

vs = [verdict(x) for x in (a1, a2, a3)]
if all(v.startswith('PASS') for v in vs):
    note = 'all three recalled the planted token' + (
        ' (some only in the thinking -- cache read correctly)' if 'PASS*' in vs else '')
elif any(v.startswith('WRONG') for v in vs):
    note = 'CACHE FAILURE -- returned a different token: ' + ','.join(vs)
else:
    note = ('INCONCLUSIVE -- no answer emitted (%s). Budget, not the cache; '
            're-run with a larger MAX_TOKENS or lower reasoning effort.' % ','.join(vs))
print('  >> %s: %s\n' % (preset, note))
PY
done

"$BIN" stop --all --yes >/dev/null 2>&1
"$BIN" daemon stop >/dev/null 2>&1
