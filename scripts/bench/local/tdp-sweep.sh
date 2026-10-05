#!/usr/bin/env bash
# Find the TDP knee for one preset: the lowest power limit that still gives full speed.
#
#   scripts/bench/tdp-sweep.sh <llamastash-binary> <model-substring> <preset> [watts...]
#
# Defaults to 25 35 45 55 70 W. Runs the same measurement path as preset-ab.sh
# (one round, one preset) at each limit, so rows here are directly comparable to
# rows there.
#
# READ THIS BEFORE TRUSTING ANY ROW. On this box the asus-nb-wmi ppt_* writes
# that z13ctl performs are accepted, read back correctly, and DO NOT BIND the
# iGPU: measured 60.0 W package draw under load at ppt_pl1_spl=25 and the same
# 60.0 W at 70, with no power1_cap exposed by amdgpu at all. A sweep run that
# way produces a perfectly flat curve that means nothing, which is exactly the
# mistake this header exists to prevent. Reading the limit back proves only
# that it was stored.
#
# So every row is gated: the script samples power1_average under load and marks
# the row INVALID unless the draw actually tracks the limit. If your rows come
# back INVALID, drive platform_profile (quiet/balanced/performance) instead --
# that path does move GPU power on this machine -- and take several reps per
# point, comparing prefill as well as decode, since prefill is the compute-bound
# half and the one prior benchmarks found sensitive to power.
#
# Restores the entry TDP on exit, including on interrupt.
set -uo pipefail

BIN="${1:?usage: tdp-sweep.sh <binary> <model> <preset> [watts...]}"
MODEL="${2:?usage: tdp-sweep.sh <binary> <model> <preset> [watts...]}"
PRESET="${3:?need a preset}"
shift 3
WATTS=("$@")
[[ ${#WATTS[@]} -gt 0 ]] || WATTS=(25 35 45 55 70)

HERE="$(dirname "$0")"
. "$HERE/lib-power.sh"
ENTRY_TDP="$(z13ctl tdp --get 2>/dev/null | awk '/PL1/{print $3}')"
restore() {
  [[ -n "${ENTRY_TDP:-}" ]] && z13ctl tdp --set "$ENTRY_TDP" >/dev/null 2>&1
  echo "-- TDP restored to ${ENTRY_TDP}W"
}
# The sweep raises TDP on purpose, so it is the likeliest script to outrun the
# charger. Restore the power limit AND disarm the watchdog on every exit path.
on_exit() { restore; power_watchdog_stop; }
trap on_exit EXIT
trap 'on_exit; exit 143' INT TERM
power_watchdog_start "$$"

echo "preset: $PRESET"
echo "sweep:  ${WATTS[*]} W   (entry TDP ${ENTRY_TDP}W)"
echo

for w in "${WATTS[@]}"; do
  if ! z13ctl tdp --set "$w" >/dev/null 2>&1; then
    echo "=== ${w}W: could not set TDP, skipping"; continue
  fi
  got="$(z13ctl tdp --get 2>/dev/null | awk '/PL1/{print $3}')"
  echo "=== ${w}W (PL1 reads ${got}W) ==========================================="
  # Sampled while the GPU is busy below; a limit that the silicon never reaches
  # makes its row meaningless, so the draw is reported next to the result.
  ( sleep 90
    for _ in 1 2 3 4 5; do
      awk '{printf "%.1f\n", $1/1000000}' /sys/class/drm/card*/device/hwmon/hwmon*/power1_average 2>/dev/null | head -1
      sleep 4
    done | sort -rn | head -1 > /tmp/.tdp-draw ) &
  sampler=$!
  ROUNDS=1 "$HERE/preset-ab.sh" "$BIN" "$MODEL" "$PRESET" 2>&1 \
    | grep -E '^  COLD|^    turn|^  >> |LAUNCH FAILED|^-- |^!! '
  wait $sampler 2>/dev/null
  DRAW_UNDER_LOAD="$(cat /tmp/.tdp-draw 2>/dev/null)" 
  # Sampled at idle, this number is useless -- the check that matters is whether
  # draw tracked the limit while the GPU was busy, recorded by the loop above.
  if [[ -n "${DRAW_UNDER_LOAD:-}" ]]; then
    over=$(awk -v d="$DRAW_UNDER_LOAD" -v w="$w" 'BEGIN{print (d > w*1.25) ? 1 : 0}')
    if [[ "$over" == "1" ]]; then
      echo "    !! INVALID ROW: drew ${DRAW_UNDER_LOAD}W under load against a ${w}W limit"
      echo "    !! the ppt_* write is not binding; this row measured ~${DRAW_UNDER_LOAD}W, not ${w}W"
    else
      echo "    draw under load: ${DRAW_UNDER_LOAD}W (limit ${w}W) -- row valid"
    fi
  fi
  echo
done
