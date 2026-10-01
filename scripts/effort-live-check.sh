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
# Usage: effort-live-check.sh <proxy-origin> [model] [direct-upstream-origin]
#   e.g. effort-live-check.sh http://127.0.0.1:11535 Qwen3.8-27B-UD-Q6_K http://127.0.0.1:41101
#
# The proxy bearer key comes from $LS_PROXY_KEY, else from proxy.api_key in
# ~/.config/llamastash/config.yaml. A keyless loopback proxy needs neither; the
# header is sent either way and ignored.
set -euo pipefail
PROXY=${1:-http://127.0.0.1:11535}
MODEL=${2:-Qwen3.8-27B-UD-Q6_K}
DIRECT=${3:-}
KEY=${LS_PROXY_KEY:-$(grep -m1 'api_key:' ~/.config/llamastash/config.yaml 2>/dev/null | awk '{print $2}')}
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
run "proxy-effort-low" "$PROXY/v1/messages" ',"output_config":{"effort":"low"}'
run "proxy-effort-xhigh" "$PROXY/v1/messages" ',"output_config":{"effort":"xhigh"}'
run "proxy-effort-unsupported" "$PROXY/v1/messages" ',"output_config":{"effort":"max"}'
run "proxy-client-kwarg-wins" "$PROXY/v1/messages" ',"output_config":{"effort":"xhigh"},"chat_template_kwargs":{"reasoning_effort":"low"}'
if [ -n "$DIRECT" ]; then
  run "direct-kwarg-low" "$DIRECT/v1/messages" ',"chat_template_kwargs":{"reasoning_effort":"low"}'
  run "direct-kwarg-xhigh" "$DIRECT/v1/messages" ',"chat_template_kwargs":{"reasoning_effort":"xhigh"}'
fi
