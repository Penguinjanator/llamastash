# Engine launch surface for the Qwen3.8-Flash-Next speed study. THIS IS THE FILE
# AN EXPERIMENT EDITS. serve.sh hashes it and restarts the engine when the hash
# changes, so a knob only takes effect by editing here.
#
# Sourced, not executed. Keep it side-effect free.

# Which runtime to measure: gufo (GGUF, ROCm 10) or halogen (native .hgn, docker).
ENGINE="${ENGINE:-halogen}"

PORT="${PORT:-41198}"

# --- model artifacts ---------------------------------------------------------

GUFO_DIR=/mnt/work/huggingface/hub/models--unsloth--Qwen3.8-Flash-Next-GGUF/snapshots/5d16c055a7c5cb276e721ee154f9c22420dde2a1
GUFO_MODEL="$GUFO_DIR/UD-Q4_K_XL/Qwen3.8-Flash-Next-UD-Q4_K_XL-00001-of-00004.gguf"
GUFO_MTP="$GUFO_DIR/MTP/mtp-Qwen3.8-Flash-Next-shared-Q8_0.gguf"

# --- workload-fixed sampling (NOT optimization knobs) ------------------------
# Held constant across the whole study so a run that changes the text cannot
# pass as a speed win. bench.py sends these; serve.sh only mirrors the launch
# default for gufo.
TOP_P=0.95
TOP_K=20

# --- gufo knobs --------------------------------------------------------------

GUFO_BINARY=/mnt/work/Workspace/llms/gufo/build/release-rocm10/gufo
GUFO_SPEC=mtp
GUFO_DRAFT_TOKENS=7
GUFO_MIN_DRAFT_TOKENS=1
GUFO_DRAFT_POLICY=adaptive
GUFO_CONTEXT=131072
GUFO_SESSIONS=1
GUFO_PREFILL_CHUNK=512

gufo_argv() {
  printf '%s\n' "$GUFO_BINARY" serve llm \
    --host 127.0.0.1 --port "$PORT" \
    --model "$GUFO_MODEL" \
    --served-model-name ar-flash-next \
    --sessions "$GUFO_SESSIONS" \
    --context "$GUFO_CONTEXT" \
    --top-p "$TOP_P" --top-k "$TOP_K" \
    --speculative "$GUFO_SPEC" \
    --mtp-model "$GUFO_MTP" \
    --draft-tokens "$GUFO_DRAFT_TOKENS" \
    --min-draft-tokens "$GUFO_MIN_DRAFT_TOKENS" \
    --draft-policy "$GUFO_DRAFT_POLICY" \
    --prefill-chunk "$GUFO_PREFILL_CHUNK"
}

# --- halogen knobs -----------------------------------------------------------

HALO_BINARY=ghcr.io/peonist-ai/halogen-flash-server:latest
# ngram.hgn trades the MTP head for an n-gram drafter; v2 is the 4.16 bpw export.
HALO_REPO=models--peonist-ai--halogen-qwen3.8-flash-next
HALO_CHECKPOINT_NAME=qwen38-flash-next-v2.hgn
HALO_SLOTS=4
HALO_CTX=262144
HALO_KV_POOL=262144
HALO_MAX_TOK=32768
HALO_MTP_DEPTH=1
HALO_NGRAM=3
HALO_NGRAM_CHAIN=3

halo_argv() {
  local snap host_ck
  snap=$(cat "/mnt/work/huggingface/hub/$HALO_REPO/refs/main")
  # The container mounts /mnt/work/huggingface/hub at /hub, so the in-container
  # path is what the engine needs.
  host_ck="/hub/$HALO_REPO/snapshots/$snap/$HALO_CHECKPOINT_NAME"
  printf '%s\n' docker run -d --rm \
    --name "ar-halo-$PORT" \
    -p "127.0.0.1:$PORT:8080" \
    --device /dev/kfd --device /dev/dri \
    --group-add "$(getent group video | cut -d: -f3)" \
    --group-add "$(getent group render | cut -d: -f3)" \
    --ipc=host --ulimit memlock=-1:-1 \
    -v /mnt/work/huggingface/hub:/hub:ro \
    -e HALOGEN_API_PORT=8080 \
    -e HALOGEN_MODEL_ID=ar-flash-next \
    -e "HALOGEN_CHECKPOINT=$host_ck" \
    -e "HALOGEN_TOKENIZER=/hub/$HALO_REPO/snapshots/$snap/tokenizer" \
    -e "HALOGEN_CTX=$HALO_CTX" \
    -e "HALOGEN_KV_POOL_POSITIONS=$HALO_KV_POOL" \
    -e "HALOGEN_MAX_TOKENS_DEFAULT=$HALO_MAX_TOK" \
    -e "HALOGEN_MTP_DEPTH=$HALO_MTP_DEPTH" \
    -e "HALOGEN_NGRAM=$HALO_NGRAM" \
    -e "HALOGEN_NGRAM_CHAIN=$HALO_NGRAM_CHAIN" \
    -e HALOGEN_TOP_P="$TOP_P" \
    -e HALOGEN_TOP_K="$TOP_K" \
    "$HALO_BINARY"
}

# One line describing the live config, used as the restart key.
engine_config_id() {
  if [ "$ENGINE" = gufo ]; then
    printf 'gufo|%s\n' "$(gufo_argv | md5sum | cut -c1-12)"
  else
    printf 'halogen|%s\n' "$(halo_argv | md5sum | cut -c1-12)"
  fi
}
