#!/usr/bin/env bash
# Sample GPU clocks, temperature, power and busy% to CSV while a bench runs.
#
#   scripts/bench/gpu-telemetry.sh [out.csv] [interval-seconds]
#
# Exists to separate two explanations that look identical in a throughput
# number: a workload that is memory-bound (core clock sits low because raising
# it would not help, temperature irrelevant) versus one that is thermally
# throttled (core clock starts high and decays as the die heats).
#
# Read the resulting CSV as: if sclk is flat from the first sample and mclk is
# pegged at its top state while package power sits *below* the limit, the
# workload is bandwidth-bound. If sclk starts at the top state and walks down
# as edge temperature climbs, it is thermal.
#
# Columns: unix_ts, iso_time, sclk_mhz, sclk_level, mclk_mhz, mclk_level,
#          edge_c, power_w, busy_pct
set -uo pipefail

OUT="${1:-/tmp/gpu-telemetry.csv}"
INTERVAL="${2:-2}"
CARD="${CARD:-$(ls -d /sys/class/drm/card*/device/pp_dpm_sclk 2>/dev/null | head -1 | xargs dirname)}"
[[ -n "$CARD" ]] || { echo "no amdgpu card found" >&2; exit 1; }
HWMON="$(ls -d "$CARD"/hwmon/hwmon* 2>/dev/null | head -1)"

echo "unix_ts,iso_time,sclk_mhz,sclk_level,mclk_mhz,mclk_level,edge_c,power_w,busy_pct" > "$OUT"
echo "sampling $CARD every ${INTERVAL}s -> $OUT" >&2

# The starred row in pp_dpm_* is the active DPM state.
active() { awk '/\*/{gsub(/Mhz/,"",$2); print $1+0","$2+0; found=1} END{if(!found)print ","}' "$1" 2>/dev/null; }

while :; do
  s="$(active "$CARD/pp_dpm_sclk")"; m="$(active "$CARD/pp_dpm_mclk")"
  slev="${s%%,*}"; smhz="${s##*,}"
  mlev="${m%%,*}"; mmhz="${m##*,}"
  edge="$(awk '{printf "%.1f", $1/1000}' "$HWMON/temp1_input" 2>/dev/null)"
  # power1_average is in microwatts on amdgpu
  pw="$(awk '{printf "%.1f", $1/1000000}' "$HWMON/power1_average" 2>/dev/null)"
  [[ -z "$pw" ]] && pw="$(rocm-smi --showpower 2>/dev/null \
      | awk -F: '/Graphics Package Power/{gsub(/ /,"",$2); print $2}')"
  busy="$(cat "$CARD/gpu_busy_percent" 2>/dev/null)"
  printf '%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
    "$(date +%s)" "$(date -Is)" "${smhz:-}" "${slev:-}" "${mmhz:-}" "${mlev:-}" \
    "${edge:-}" "${pw:-}" "${busy:-}" >> "$OUT"
  sleep "$INTERVAL"
done
