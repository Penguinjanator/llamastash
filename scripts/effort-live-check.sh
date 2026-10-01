#!/usr/bin/env bash
# Live check: does an Anthropic effort field change thinking length on a
# llama.cpp model through the proxy?
#
# Exercises the /v1/messages effort mapping (src/backend/llama_cpp/effort.rs)
# against a real llama-server, which no fixture can do: the field's effect is
# the model's own chat template taking a different reasoning branch, so the
# evidence is token counts, not a parsed body. Run it after a llama.cpp upgrade
# — if the "proxy-effort-*" rows track the "direct-kwarg-*" rows without the
# mapping, upstream now handles the field and the hook can go
# (upstream ggml-org/llama.cpp#20479).
#
# Usage: effort-live-check.sh <proxy-origin> <model> [direct-upstream-origin]
#   e.g. effort-live-check.sh http://127.0.0.1:11535 Qwen3.8-27B-UD-Q6_K http://127.0.0.1:41101
#   cheap variant (1.5 GB, loads in seconds):
#     EFFORT_HIGH=ultra EFFORT_BAD=xhigh effort-live-check.sh \
#       http://127.0.0.1:11535 LFM2.5-2.6B-Q3.8-TBrilliance-NEO-IQ4_XS \
#       http://127.0.0.1:41101
#   pulled with:
#     llamastash pull DavidAU/LFM2.5-2.6B-Qwen3.8-Turbo-Brilliance-Power-X12-NEO-MAX-GGUF:LFM2.5-2.6B-Q3.8-TBrilliance-NEO-IQ4_XS.gguf
#
# The model needs a chat template that defines `reasoning_effort`. A model
# whose template has no effort branch (Qwen3.5-4B, e.g.) answers every row
# identically and proves nothing beyond the plumbing. The accepted names are
# per-template: Qwen3.8 takes `xhigh` / `medium` / `low` and raises on anything
# else, DAU's fusion templates take `low` ... `ultra` and fall back to their own
# default instead of raising. Override the row values with `EFFORT_LOW`,
# `EFFORT_HIGH` and `EFFORT_BAD` when the defaults do not fit the model.
#
# The proxy bearer key comes from $LS_PROXY_KEY. A keyless loopback proxy needs
# none, and the header is sent either way and ignored.
set -euo pipefail
if [ $# -lt 2 ]; then
  awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
  exit 2
fi
PROXY=$1
MODEL=$2
DIRECT=${3:-}
KEY=${LS_PROXY_KEY:-$(grep -m1 'api_key:' ~/.config/llamastash/config.yaml 2>/dev/null | awk '{print $2}' || true)}
PROMPT='A farmer has 17 sheep. All but 9 die. He then buys 3 dozen eggs and sells them at a 20% discount off $2.50 each. How many sheep are left and what does he earn? Reason carefully.'

run() { # $1=label  $2=url  $3=extra json fragment (may be empty)
  local body
  body=$(printf '{"model":"%s","max_tokens":600,"temperature":0,"messages":[{"role":"user","content":"%s"}]%s}' "$MODEL" "$PROMPT" "$3")
  curl -sS -X POST "$2" -H 'content-type: application/json' -H "x-api-key: $KEY" -d "$body" |
    jq -r --arg l "$1" '[$l, (.usage.output_tokens|tostring),
      ((.content[]?|select(.type=="thinking")|.thinking//"")|length|tostring)] | @tsv'
}

printf 'LABEL\tOUTPUT_TOKENS\tTHINKING_CHARS\n'
run "proxy-no-effort" "$PROXY/v1/messages" ""
ELLOW=${EFFORT_LOW:-low}
EHIGH=${EFFORT_HIGH:-xhigh}
EBAD=${EFFORT_BAD:-max}
run "proxy-effort-low" "$PROXY/v1/messages" ",\"output_config\":{\"effort\":\"$ELLOW\"}"
run "proxy-effort-xhigh" "$PROXY/v1/messages" ",\"output_config\":{\"effort\":\"$EHIGH\"}"
run "proxy-effort-unsupported" "$PROXY/v1/messages" ",\"output_config\":{\"effort\":\"$EBAD\"}"
run "proxy-client-kwarg-wins" "$PROXY/v1/messages" ",\"output_config\":{\"effort\":\"$EHIGH\"},\"chat_template_kwargs\":{\"reasoning_effort\":\"$ELLOW\"}" 
if [ -n "$DIRECT" ]; then
  run "direct-kwarg-low" "$DIRECT/v1/messages" ",\"chat_template_kwargs\":{\"reasoning_effort\":\"$ELLOW\"}"
  run "direct-kwarg-xhigh" "$DIRECT/v1/messages" ",\"chat_template_kwargs\":{\"reasoning_effort\":\"$EHIGH\"}" 
fi
