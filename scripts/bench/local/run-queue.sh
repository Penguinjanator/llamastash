#!/usr/bin/env bash
# Resilient preset queue: one preset per attempt, resumed after power aborts.
#
#   scripts/bench/run-queue.sh <llamastash-binary> <model> <preset>...
#
# On this chassis the charger cannot hold a sustained GPU load: the pack drains
# even with AC connected and `status` reading Charging, and a latch persists for
# a while after the load stops. A single long multi-preset run therefore dies
# partway through and stays dead -- on 2026-08-31 a three-cell factorial lost
# two cells that way and then sat idle for hours while the pack refilled.
#
# So: one preset per invocation of the inner harness, a completion marker per
# preset, and a wait for real charge (not just AC-present) between attempts.
# Re-running this script skips whatever already has a marker, so it is safe to
# restart at any time and safe to leave running unattended.
set -uo pipefail

BIN="${1:?usage: run-queue.sh <binary> <model> <preset>...}"
MODEL="${2:?usage: run-queue.sh <binary> <model> <preset>...}"
shift 2
PRESETS=("$@")
LOGS="${LOGS:-$HOME/.cache/llamastash-bench-logs}"
STATE="$LOGS/queue"; mkdir -p "$STATE"
CTX_BYTES="${CTX_BYTES:-160000}"
MAX_TOKENS="${MAX_TOKENS:-10000}"
CAND="${CAND:-$HOME/.cache/llamastash-cand/config.yaml}"
RESUME_AT="${RESUME_AT:-60}"      # need this much charge before starting a cell
ATTEMPTS="${ATTEMPTS:-4}"
HERE="$(cd "$(dirname "$0")" && pwd)"

batt(){ cat /sys/class/power_supply/BAT0/capacity 2>/dev/null || echo 0; }
draining(){ [[ "$(cat /sys/class/power_supply/BAT0/status 2>/dev/null)" == "Discharging" ]]; }

for p in "${PRESETS[@]}"; do
  [[ -f "$STATE/$p.done" ]] && { echo "SKIP $p (already has data)"; continue; }
  for a in $(seq 1 "$ATTEMPTS"); do
    # Wait for a pack that can actually carry a cell. Charge alone is not
    # enough: a latched charger reads AC-present while the pack drains.
    while :; do
      b="$(batt)"
      if [[ "$b" -ge "$RESUME_AT" ]] && ! draining; then break; fi
      echo "WAIT $p: batt ${b}% $(cat /sys/class/power_supply/BAT0/status 2>/dev/null), need >=${RESUME_AT}% and not draining"
      sleep 300
    done
    echo "RUN $p (attempt $a/$ATTEMPTS, batt $(batt)%)"
    CTX_BYTES="$CTX_BYTES" MAX_TOKENS="$MAX_TOKENS" LS_REAL_CONFIG="$CAND" \
      "$HERE/preset-reasoning-ab.sh" "$BIN" "$MODEL" "$p" > "$LOGS/cell-$p.log" 2>&1
    if grep -q ">> $p" "$LOGS/cell-$p.log"; then
      touch "$STATE/$p.done"
      echo "OK $p: $(grep -oE 'complete answers [0-9]+/[0-9]+' "$LOGS/cell-$p.log" | tail -1)"
      grep -E "^  Q[0-9]" "$LOGS/cell-$p.log"
      break
    fi
    echo "FAIL $p attempt $a: $(grep -oE '!! .*' "$LOGS/cell-$p.log" | head -1)"
  done
  [[ -f "$STATE/$p.done" ]] || echo "GAVE UP on $p after $ATTEMPTS attempts"
done
echo "QUEUE_DONE"
