#!/usr/bin/env bash
# Battery/AC helpers shared by the bench scripts. Source it; do not run it.
#
# Long benches on the ROG Flow Z13 outlast the pack. Throughput on this box has
# halved on low battery while platform_profile still read "performance", so a
# run that drains into the red produces numbers that look valid and are not.
# `wait_for_charge` parks the bench with the model stopped until the pack comes
# back, rather than measuring through the brownout.
#
# Callers must define $BIN before sourcing.
#
# The floor MUST be enforced continuously, not at preset boundaries. On
# 2026-08-30 a run died with the box powered off by upowerd at 2%: the guard
# passed at 38% "Charging", then the charger latched off mid-leg and the pack
# took the full 85-91 W load, falling 30% -> 2% in 16 minutes while no boundary
# check was due. At that drain rate the 15% floor is ~10 minutes of headroom and
# a single leg runs for an hour, so only a sampling watchdog can hold it.
#
# Do not trust the `status` field either. It read "Charging" for 48 minutes at
# 4-16 W while the pack sat flat at 36% and slowly declined -- the adapter was
# losing to the GPU the whole time. `discharging_now` reads the sign of the
# current instead.
#
#   PAUSE_BELOW  park the run under this charge   (default 30)
#   RESUME_AT    resume once back up to this      (default 70)
#   MIN_BATT     hard abort floor                 (default 15)
#   WATCH_EVERY  watchdog sample interval, sec    (default 15)
#   DISCHARGE_N  consecutive discharging samples that abort (default 4)

PAUSE_BELOW="${PAUSE_BELOW:-30}"
RESUME_AT="${RESUME_AT:-70}"
MIN_BATT="${MIN_BATT:-15}"
WATCH_EVERY="${WATCH_EVERY:-15}"
DISCHARGE_N="${DISCHARGE_N:-4}"
FLAT_GIVEUP_MIN="${FLAT_GIVEUP_MIN:-20}"

battery() { cat /sys/class/power_supply/BAT0/capacity 2>/dev/null || echo '?'; }
on_ac()   { cat /sys/class/power_supply/AC0/online 2>/dev/null || echo '?'; }
batt_status() { cat /sys/class/power_supply/BAT0/status 2>/dev/null || echo '?'; }

# True when the pack is actually losing charge. Checked instead of `status`
# because "Charging" stayed true through the drain that killed the 08-30 run.
discharging_now() { [[ "$(batt_status)" == "Discharging" ]]; }

# Watts in or out of the pack, unsigned.
batt_watts() {
  local uw; uw="$(cat /sys/class/power_supply/BAT0/power_now 2>/dev/null)" || return
  [[ -n "$uw" ]] && awk -v u="$uw" 'BEGIN{printf "%.1f", u/1000000}'
}

# Continuous floor enforcement for the whole run.
#
# guard() and wait_for_charge() only fire between presets. This samples
# throughout a leg and is the only thing standing between a charger dropout and
# a hard power-off. It stops the model FIRST on every abort path: that drops
# system draw immediately, which is both what protects the pack and what lets
# the charger re-latch.
POWER_WATCHDOG_PID=""
power_watchdog_start() {
  local target="${1:-$$}"
  power_watchdog_stop
  (
    disch=0
    while :; do
      sleep "$WATCH_EVERY"
      b="$(battery)"; [[ "$b" == '?' ]] && continue
      reason=""
      if [[ "$b" -lt "$MIN_BATT" ]]; then
        reason="battery ${b}% < ${MIN_BATT}% floor"
      elif discharging_now; then
        disch=$((disch + 1))
        if (( disch >= DISCHARGE_N )); then
          reason="pack discharging $(batt_watts)W for $((disch * WATCH_EVERY))s at ${b}% (AC $(on_ac)) -- charger is not holding the load"
        fi
      else
        disch=0
      fi
      [[ -z "$reason" ]] && continue
      echo "!! POWER WATCHDOG: $reason" >&2
      echo "!! stopping models and aborting the run" >&2
      "$BIN" stop --all --yes >/dev/null 2>&1
      "$BIN" daemon stop     >/dev/null 2>&1
      pkill -f 'llama-server .*--port' >/dev/null 2>&1
      kill -TERM "$target" >/dev/null 2>&1
      sleep 10
      kill -KILL "$target" >/dev/null 2>&1
      exit 0
    done
  ) &
  POWER_WATCHDOG_PID=$!
  echo "-- power watchdog armed (pid $POWER_WATCHDOG_PID): floor ${MIN_BATT}%, sampling ${WATCH_EVERY}s"
}

# The watchdog's SIGTERM must actually end the run. A plain
# `trap power_watchdog_stop EXIT INT TERM` does the opposite: bash runs the
# handler, the handler kills the watchdog before it can escalate to SIGKILL,
# and then execution RESUMES from where the signal landed. On 2026-08-31 that
# let an aborted preset roll straight into the next one on a latched charger.
# Install with `power_traps_install` instead of trapping by hand.
power_traps_install() {
  trap 'power_watchdog_stop' EXIT
  trap 'power_watchdog_stop; echo "!! aborted by signal" >&2; exit 143' INT TERM
}

power_watchdog_stop() {
  [[ -n "$POWER_WATCHDOG_PID" ]] && kill "$POWER_WATCHDOG_PID" 2>/dev/null
  POWER_WATCHDOG_PID=""
}

# Hard floor. Stops everything and leaves the box idle.
guard() {
  local b; b="$(battery)"
  if [[ "$b" != '?' && "$b" -lt "$MIN_BATT" ]]; then
    echo "!! battery ${b}% < ${MIN_BATT}% ($(batt_status), $(batt_watts)W) -- stopping all models and aborting"
    "$BIN" stop --all --yes >/dev/null 2>&1
    "$BIN" daemon stop >/dev/null 2>&1
    exit 3
  fi
}

# Park until the pack recovers. The model is stopped first: at high TDP this
# machine can draw more than the charger supplies, so charging only makes
# progress once the GPU is idle.
wait_for_charge() {
  local b; b="$(battery)"
  [[ "$b" == '?' ]] && return 0
  [[ "$b" -ge "$PAUSE_BELOW" ]] && return 0

  echo "-- battery ${b}% below ${PAUSE_BELOW}%: stopping model, waiting for ${RESUME_AT}%"
  "$BIN" stop --all --yes >/dev/null 2>&1
  local last="$b" flat=0
  while :; do
    sleep 60
    b="$(battery)"
    if [[ "$b" -ge "$RESUME_AT" ]]; then
      echo "-- battery ${b}%, resuming"
      return 0
    fi
    if [[ "$b" -le "$last" ]]; then
      flat=$((flat + 1))
    else
      flat=0
    fi
    # Idle and still not gaining: the charger is not keeping up at this TDP.
    # Report rather than silently juggling power limits mid-experiment.
    # A latched charger needs ~15-20 min of idle to re-latch; giving up after
    # 5 flat minutes aborted runs that would have recovered on their own.
    if (( flat >= FLAT_GIVEUP_MIN )); then
      echo "!! battery stuck at ${b}% for ${FLAT_GIVEUP_MIN} min with the model stopped (AC $(on_ac), $(batt_status))."
      echo "!! the charger is not recovering at this TDP -- aborting so the run is not"
      echo "!! measured through a brownout. Lower TDP or swap the charger, then rerun."
      "$BIN" daemon stop >/dev/null 2>&1
      exit 4
    fi
    if [[ "$b" -lt "$MIN_BATT" ]]; then
      echo "!! battery ${b}% < ${MIN_BATT}% while parked -- aborting"
      "$BIN" daemon stop >/dev/null 2>&1
      exit 3
    fi
    echo "   ... ${b}% (ac $(on_ac))"
  done
}
