#!/usr/bin/env bash
# Phase 3: settle turbo4 properly, then find the sampling + reasoning setting
# that actually finishes an answer without giving up MTP throughput.
#
#   scripts/bench/run-phase3.sh <llamastash-binary> <model-substring>
#
# Two questions, in order, because the second is worthless if the first is
# mismeasured.
#
# 1. turbo4. Phase 2 scored it FAIL on three empty answers with a 300-token
#    budget. Empty is not wrong: corruption emits a DIFFERENT token, an
#    exhausted budget emits none, and the old gate could not tell those apart
#    because it recorded neither finish_reason nor reasoning_content. Re-run
#    with a generous budget and both fields captured. Note upstream never
#    endorsed this combination anyway -- run_server.sh's `cache` profile
#    hardcodes KV_V=q8_0 with no override, the only non-overridable setting in
#    the file -- so a pass here still would not make it a default.
#
# 2. Sampling. Qwen's official thinking-mode spec is temperature 1.0 / top_k 20
#    / top_p 0.95 / min_p 0.0; this preset runs full greedy at temp 0.0 with
#    llama.cpp's top_k 40 and min_p 0.05 defaults. But this is not stock Qwen:
#    it is a ROCmFP4 quant on a fork running MTP speculative decoding, where
#    upstream measured draft acceptance at ~88% at temp 0 collapsing to ~25% at
#    temp 0.8 -- decode falls back to unassisted speed, roughly 2.5x slower.
#    So the official spec and this stack's throughput advice point opposite
#    ways, and both arms get measured rather than argued about:
#      pi-cache            temp 0.0 xhigh   - what ships today
#      cand-qwen-sampling  temp 1.0 xhigh   - Qwen official, cost unknown here
#      cand-t02            temp 0.2 xhigh   - top of upstream's throughput band
#      cand-t02-med        temp 0.2 medium  - medium injects NO instructions
#      cand-t02-low        temp 0.2 low     - low instructs brevity explicitly
#      cand-nothink        budget 0         - thinking off
#    The template only accepts xhigh/medium/low (verified in the GGUF; `none`
#    raises), and only xhigh and low inject any instruction text at all.
set -uo pipefail

BIN="${1:?usage: run-phase3.sh <binary> <model>}"
MODEL="${2:?usage: run-phase3.sh <binary> <model>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CAND_CONFIG="${CAND_CONFIG:-$HOME/.cache/llamastash-cand/config.yaml}"
LOGS="${LOGS:-$HOME/.cache/llamastash-bench-logs}"   # not /tmp: tmpfs here, a
mkdir -p "$LOGS"                                     # reboot ate the phase-2 logs

echo "=== 1/2  turbo4 correctness: does a divergent tail restore correctly? ==="
# Upstream's `speed` profile pairs TurboQuant KV WITH prompt checkpoints and
# reports it measured working on v1.5.2+ (a divergent-tail follow-up on a 9K
# document finishing in 4-5s instead of a ~33s cold prefill). The older v215
# cache report saying TurboQuant "cannot use cache shifting" predates that fix.
# We run v244, so the fix should be present -- verify rather than assume.
MAX_TOKENS=6000 LS_REAL_CONFIG="$CAND_CONFIG" \
  "$HERE/preset-cache-correctness.sh" "$BIN" "$MODEL" pi-cache cand-turbo \
  2>&1 | tee "$LOGS/turbo-retest.log"

echo
echo "=== 2/2  sampling + reasoning: which setting finishes an answer? ==="
MAX_TOKENS=8000 LS_REAL_CONFIG="$CAND_CONFIG" \
  "$HERE/preset-reasoning-ab.sh" "$BIN" "$MODEL" \
    pi-cache cand-speed cand-budget2k cand-turbo cand-qwen-sampling \
  2>&1 | tee "$LOGS/sampling-ab.log"

echo
echo "PHASE3_DONE"
