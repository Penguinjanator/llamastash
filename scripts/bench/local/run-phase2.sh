#!/usr/bin/env bash
# Phase 2: gate turbo4 on correctness, then rank the survivors on speed and
# stability together.
#
#   scripts/bench/run-phase2.sh <llamastash-binary> <model-substring>
#
# Order is deliberate. cand-turbo measured +13% decode and 85% MTP acceptance
# against pi-cache, but the disputed claim about it is that TurboQuant's rotated
# V cache plus checkpoint restore returns WRONG values without erroring. A cache
# that restores wrongly is faster and still fluent, so the speed win is exactly
# what corruption would look like. Correctness runs first and everything
# downstream is skipped if it fails -- there is no point tuning a combination
# built on a broken cache.
set -uo pipefail

BIN="${1:?usage: run-phase2.sh <binary> <model>}"
MODEL="${2:?usage: run-phase2.sh <binary> <model>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CAND_CONFIG="${CAND_CONFIG:-$HOME/.cache/llamastash-cand/config.yaml}"

echo "=== 1/3  correctness gate: does turbo4 survive a checkpoint rollback? ==="
LS_REAL_CONFIG="$CAND_CONFIG" \
  "$HERE/preset-cache-correctness.sh" "$BIN" "$MODEL" pi-cache cand-turbo 2>&1 | tee /tmp/correctness.log

# pi-cache is the control: if IT fails the planted-token recall, the harness is
# wrong, not the cache, and no verdict about turbo4 can be drawn from this run.
ctrl_ok=$(grep -c 'pi-cache: all three recalled' /tmp/correctness.log || true)
turbo_ok=$(grep -c 'cand-turbo: all three recalled' /tmp/correctness.log || true)

if [[ "$ctrl_ok" != "1" ]]; then
  echo
  echo "!! CONTROL FAILED: pi-cache did not recall the planted token either."
  echo "!! The harness or the prompt is at fault, not turbo4. No verdict."
  echo "PHASE2_DONE"; exit 0
fi

if [[ "$turbo_ok" != "1" ]]; then
  echo
  echo "!! turbo4 FAILED the correctness gate while pi-cache passed."
  echo "!! The config comment was right; do not ship turbo4. Skipping its variants."
  CANDS=(pi-cache cand-ub512)
else
  echo
  echo "=== turbo4 passed the correctness gate; including its variants ==="
  CANDS=(pi-cache cand-turbo cand-combo cand-turbo-nostrict cand-ub512)
fi

echo
echo "=== 2/3  speed, 2 rounds so the deltas are not single samples ==="
LS_REAL_CONFIG="$CAND_CONFIG" ROUNDS=2 \
  "$HERE/preset-ab.sh" "$BIN" "$MODEL" "${CANDS[@]}" 2>&1 | tee /tmp/phase2-speed.log

# Rank by decode, then carry the top two into the long run. Speed alone does not
# decide anything -- a preset that leads here and drops a session loses.
TOP2="$(grep -E '^  >> ' /tmp/phase2-speed.log \
        | awk '{for(i=1;i<NF;i++) if($i=="decode"){print $2, $(i+1)}}' \
        | awk '{s[$1]+=$2; n[$1]++} END{for(k in s) printf "%.2f %s\n", s[k]/n[k], k}' \
        | sort -rn | head -2 | awk '{print $2}' | paste -sd' ')"
echo
echo "=== top two on speed: $TOP2 ==="

echo
echo "=== 3/3  stability + speed over a long session (40 turns, branch every 5th) ==="
LS_REAL_CONFIG="$CAND_CONFIG" \
  "$HERE/preset-stability.sh" "$BIN" "$MODEL" $TOP2 40 2>&1 | tee /tmp/stability.log

echo
echo "PHASE2_DONE"
