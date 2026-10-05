#!/usr/bin/env bash
# DFlash2 sidecar vs embedded MTP, both on the same engine.
#
#   scripts/bench/dflash-ab.sh [fork-llama-server] [target.gguf] [drafter.gguf]
#
# The DFlash2 claim (65.6 t/s structured, 4.7x bare) comes from a model card
# measured on LaurentZuijdwijk's fork, which our ROCmFPX engine is not: the
# `--spec-draft-adaptive` flag that claim depends on was never upstreamed and
# does not apply cleanly to our pinned tree. So every arm here runs on the FORK
# build -- comparing DFlash2 on one engine against MTP on another would confound
# the drafter with the engine.
#
# Content type is the whole story with speculation: block drafting accepts
# near-100% on predictable output and falls apart on prose, so structured and
# prose prompts are measured separately and never averaged. Greedy, 300 tokens,
# matching the card's stated method so the numbers are comparable to its table.
set -uo pipefail

BIN="${1:-$HOME/Workspace/llms/llama.cpp-lz/build/bin/llama-server}"
TARGET="${2:-/mnt/work/huggingface/hub/models--julianmb--Qwen-3.8-27B-ROCmFP4-FAST-GGUF/snapshots/716591090c9066652cd186b6e9194222525fdae8/Qwen3.8-27B-ROCmFP4-FAST.gguf}"
DRAFT="${3:-/mnt/work/huggingface/hub/models--agentionai--Qwen3.8-27B-DFlash2-ROCmFP4-FAST-GGUF/snapshots/main/Qwen3.8-27B-DFlash2-Q4_0_ROCMFP4_FAST.gguf}"
PORT="${PORT:-41199}"
COMMON=(-m "$TARGET" --host 127.0.0.1 --port "$PORT" -ngl 999 -fa on -b 2048 -ub 512 -c 32768 --temp 0.0)

for f in "$BIN" "$TARGET" "$DRAFT"; do [[ -e "$f" ]] || { echo "missing: $f"; exit 2; }; done
BIN_LS=/bin/true; BIN="$BIN" . "$(dirname "$0")/lib-power.sh" 2>/dev/null || true
BIN_SAVE="$BIN"

run() {  # run <label> <extra-args...>
  local label="$1"; shift
  "$BIN_SAVE" "${COMMON[@]}" "$@" > /tmp/dflash-srv.log 2>&1 &
  local pid=$!
  for _ in $(seq 1 120); do
    [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/health" 2>/dev/null)" == "200" ]] && break
    kill -0 $pid 2>/dev/null || { echo "  $label: SERVER DIED"; tail -3 /tmp/dflash-srv.log|sed 's/^/    /'; return; }
    sleep 5
  done
  PORT="$PORT" LABEL="$label" python3 - <<'PY'
import json, os, time, urllib.request
port, label = os.environ['PORT'], os.environ['LABEL']
URL=f'http://127.0.0.1:{port}/v1/chat/completions'
TASKS={
 'structured':'Output a JSON array of 40 objects, each {"id":N,"sq":N*N,"hex":"0xNN"} for N=1..40. JSON only.',
 'prose':'Write a flowing 300-word essay on why unified memory changes local LLM inference. No lists.'}
for kind,q in TASKS.items():
    b=json.dumps({'model':'q','messages':[{'role':'user','content':q}],
                  'max_tokens':300,'temperature':0}).encode()
    r=urllib.request.Request(URL,b,{'Content-Type':'application/json'})
    t0=time.time()
    try: d=json.loads(urllib.request.urlopen(r,timeout=900).read())
    except Exception as e: print('  %-22s %-11s ERROR %s'%(label,kind,type(e).__name__)); continue
    t=d.get('timings',{}); dn,da=t.get('draft_n',0),t.get('draft_n_accepted',0)
    print('  %-22s %-11s %6.2f t/s   %4d tok  %5.1fs  accept %5.1f%%'
          %(label,kind,t.get('predicted_per_second',0),t.get('predicted_n',0),
            time.time()-t0, 100*da/max(dn,1) if dn else 0))
PY
  kill $pid 2>/dev/null; wait $pid 2>/dev/null; sleep 3
}

echo "engine: $("$BIN_SAVE" --version 2>&1|head -1)"
echo "target: $(basename "$TARGET")   drafter: $(basename "$DRAFT")"
echo
run "bare"            --spec-type none
run "mtp-n4"          --spec-type draft-mtp --spec-draft-n-max 4 --spec-draft-p-min 0.0
run "dflash-fixed-n3" --spec-type draft-dflash -md "$DRAFT" --spec-draft-n-max 3 --spec-draft-ngl 99
run "dflash-adaptive" --spec-type draft-dflash -md "$DRAFT" --spec-draft-adaptive \
                      --spec-draft-n-min 3 --spec-draft-n-max 7 --spec-draft-ngl 99
echo
echo "DFLASH_DONE"
