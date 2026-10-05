#!/usr/bin/env bash
# Orchestrates the full preset study: TDP sweep -> pick the knee -> compare
# candidates at that knee -> realistic coding run.
#
#   scripts/bench/run-preset-study.sh <llamastash-binary> <model-substring>
#
# The knee is the *lowest* power limit whose decode rate is within KNEE_TOL of
# the best observed. Everything after the sweep runs there rather than at the
# highest limit, because on this chassis a high sustained limit drains the pack
# (measured -16 W net under load with a 100 W PD adapter, while the battery
# status field still reads "Charging") and the extra watts bought no throughput.
#
# Candidate comparisons stay valid at any limit as long as it is held constant
# and the baseline preset is re-measured inside the same batch -- which is why
# BASELINE is always passed first to the candidate run.
set -uo pipefail

BIN="${1:?usage: run-preset-study.sh <binary> <model>}"
MODEL="${2:?usage: run-preset-study.sh <binary> <model>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CAND_CONFIG="${CAND_CONFIG:-$HOME/.cache/llamastash-cand/config.yaml}"
BASELINE="${BASELINE:-pi-cache}"
CANDIDATES=(${CANDIDATES:-cand-turbo cand-ub512 cand-nostrict})
WATTS=(${WATTS:-25 35 45 55 70})
KNEE_TOL="${KNEE_TOL:-3}"     # percent off the best decode still counted as full speed

SWEEP_LOG=/tmp/tdp-sweep.log
CAND_LOG=/tmp/cand2.log
CODING_LOG=/tmp/coding-task.log

echo "=== 1/3  TDP sweep: ${WATTS[*]} W on $BASELINE ==="
"$HERE/tdp-sweep.sh" "$BIN" "$MODEL" "$BASELINE" "${WATTS[@]}" 2>&1 | tee "$SWEEP_LOG"

# Pair each "=== NW" header with the decode rate from the >> line that follows.
KNEE="$(awk -v tol="$KNEE_TOL" '
  /^=== [0-9]+W/      { w = $2; sub(/W$/, "", w) }
  /median decode/     { for (i = 1; i < NF; i++) if ($i == "decode") { d[w] = $(i+1); order[++n] = w } }
  END {
    best = 0; for (k in d) if (d[k] + 0 > best) best = d[k] + 0
    if (best == 0) { print ""; exit }
    knee = ""
    for (i = 1; i <= n; i++) { k = order[i]
      if (d[k] + 0 >= best * (1 - tol/100)) if (knee == "" || k + 0 < knee + 0) knee = k }
    print knee
  }' "$SWEEP_LOG")"

if [[ -z "$KNEE" ]]; then
  echo "!! could not parse a knee from the sweep; leaving TDP as-is"
else
  echo "=== knee: ${KNEE}W (within ${KNEE_TOL}% of best decode) -- applying"
  z13ctl tdp --set "$KNEE" >/dev/null 2>&1
fi
z13ctl tdp --get 2>/dev/null | awk '/PL1/{print "    TDP now "$3"W"}'

echo
echo "=== 2/3  candidates vs $BASELINE at $(z13ctl tdp --get 2>/dev/null | awk '/PL1/{print $3}')W ==="
LS_REAL_CONFIG="$CAND_CONFIG" ROUNDS=1 \
  "$HERE/preset-ab.sh" "$BIN" "$MODEL" "$BASELINE" "${CANDIDATES[@]}" 2>&1 | tee "$CAND_LOG"

echo
echo "=== 3/3  realistic coding task (questions + answers inline) ==="
LS_REAL_CONFIG="$CAND_CONFIG" \
  "$HERE/preset-coding-task.sh" "$BIN" "$MODEL" pi-coding "$BASELINE" 2>&1 | tee "$CODING_LOG"

echo
echo "STUDY_DONE"
