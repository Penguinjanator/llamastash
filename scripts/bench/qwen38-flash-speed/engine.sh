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
# Defaults here reproduce the live container's env exactly, read with
# `docker inspect llamastash-halogen-41103`, so a measurement against this file
# measures the shipping config rather than a guess at it.

HALO_BINARY=ghcr.io/peonist-ai/halogen-flash-server
# ngram.hgn is v2's lookup table, not an alternative checkpoint. The n-gram knob
# is HALO_NGRAM below.
HALO_REPO=models--peonist-ai--halogen-qwen3.8-flash-next
HALO_CHECKPOINT_NAME=qwen38-flash-next-v2.hgn
HALO_IMAGE_VERSION=0.15.1
HALO_SLOTS=4
HALO_CTX=262144
# Default would be 2x ctx. Live runs 1x on purpose: the pool takes RAM the 47 GiB
# n-gram table's page cache would otherwise keep.
HALO_KV_POOL=262144
HALO_MAX_TOK=32768
HALO_PREFILL_CHUNK=32768
HALO_MAX_TOKENS_DEFAULT=16384
HALO_MAX_TOKENS_CAP=65536
# Docs: 2 best all-round, 3 better for code, 5-8% worse on agent/prose.
HALO_MTP_DEPTH=3
# Prompt lookup as N,K. Vendor's headline decode number includes it.
HALO_NGRAM=3
HALO_NGRAM_CHAIN=3
# MTP adaptive policy W,floor,R: drop the head for R tokens when a W-round window
# falls under floor acceptance. 0 never adapts.
HALO_SPEC_ADAPT="32,0.35,64"
# Prompt-cache mode. 2 resumes anywhere; NUMERIC, so it is out of the speed sweep.
HALO_PROMPT_CACHE=2
# Kernel selection. The baked plan was tuned on the vendor's ~85 W reference box.
HALO_MATMUL_ALGOS=1
HALO_MATMUL_TUNING_FILE=/opt/halogen/flash-tune.plan
HALO_ATTN_FA=64

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
    -e "HALOGEN_KV_SLOTS=$HALO_SLOTS" \
    -e "HALOGEN_MAX_TOK=$HALO_MAX_TOK" \
    -e "HALOGEN_PREFILL_CHUNK=$HALO_PREFILL_CHUNK" \
    -e "HALOGEN_MAX_TOKENS_DEFAULT=$HALO_MAX_TOKENS_DEFAULT" \
    -e "HALOGEN_MAX_TOKENS_CAP=$HALO_MAX_TOKENS_CAP" \
    -e "HALOGEN_MTP_DEPTH=$HALO_MTP_DEPTH" \
    -e "HALOGEN_PLD=$HALO_NGRAM,$HALO_NGRAM_CHAIN" \
    -e "HALOGEN_SPEC_ADAPT=$HALO_SPEC_ADAPT" \
    -e "HALOGEN_PROMPT_CACHE=$HALO_PROMPT_CACHE" \
    -e "HALOGEN_MATMUL_ALGOS=$HALO_MATMUL_ALGOS" \
    -e "HALOGEN_MATMUL_TUNING_FILE=$HALO_MATMUL_TUNING_FILE" \
    -e "HALOGEN_ATTN_FA=$HALO_ATTN_FA" \
    -e HALOGEN_TOP_P="$TOP_P" \
    -e HALOGEN_TOP_K="$TOP_K" \
    "${HALO_BINARY}:${HALO_IMAGE_VERSION}"
}

# One line describing the live config, used as the restart key.
engine_config_id() {
  if [ "$ENGINE" = gufo ]; then
    printf 'gufo|%s\n' "$(gufo_argv | md5sum | cut -c1-12)"
  else
    printf 'halogen|%s\n' "$(halo_argv | md5sum | cut -c1-12)"
  fi
}
