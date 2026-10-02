# Ideas backlog

Ordered by expected payoff. Delete entries as they are tried; note the outcome in
`.auto/prompt.md` § What's Been Tried.

## Phase 1 (head-to-head) — blocked

- gufo at its shipping config: `--speculative mtp --draft-tokens 7 --context
  131072 --sessions 1 --prefill-chunk 512`. Needs ~105 GB, so halogen must be
  unloaded first, which ends this session. Needs an operator decision.

## Halogen knobs (runnable without unloading anything else)

- `HALO_SLOTS`: 4 today. 4 slots reserve KV for four concurrent sessions; a
  single-user coding harness may pay for that in KV pressure and get nothing.
  Try 1 and 2.
- `HALO_KV_POOL` vs `HALO_CTX`: config.yaml notes a 786432 pool adds ~14.4 GiB
  and the pool must be >= ctx. Try a pool just above ctx rather than 4x it.
- `HALO_MTP_DEPTH`: 1 today. Depth 2 drafts deeper per round.
- `HALO_CHECKPOINT_NAME=qwen38-flash-next-ngram.hgn`: n-gram drafter instead of
  the MTP head. Different acceptance profile; cheap to try if the checkpoint
  loads in the same image.
- `HALO_NGRAM` / `HALO_NGRAM_CHAIN`: /health reports `prompt_lookup {ngram:3,
  chain:3}` applied to greedy requests alongside the MTP drafter. Sweep 1-5.
- `HALO_CTX` below native 262144: smaller window may shrink per-slot residency
  and free memory for KV pages, if it changes allocation and not just admission.

## gufo knobs (once the memory conflict is resolved)

- `--speculative off` as the control, then `mtp`, `dflash2`, `dspark`. Flash-Next
  ships a shared MTP head; `dspark` support is in the same binary.
- `--draft-tokens` 3 / 5 / 7 / 9 with `--draft-policy adaptive` vs `fixed`.
- `--prefill-chunk` (512 today) interleaves prefill between decode rounds, so it
  is a prefill/decode tradeoff, not just a prefill knob.
- `--sessions` 1 vs 2 vs 4.
- Whether gufo mmaps or loads, and `--context` size vs KV residency at 105 GB of
  weights on a 124 GB box.

## Cross-cutting, out of scope until the two phases are done

- TDP sweep (55 W now; `z13ctl tdp --set`) — a real axis but changes every
  number, so it gets its own session.
- gufo rebuild flags from `~/dotfiles/LLM-BENCH-NEXT.md` (`GGML_HIP_NO_VMM`,
  `GGML_HIP_MMQ_MFMA`, the `--amdgpu-unroll-threshold-local=600` workaround).
  Verify those claims against the current gufo tree first; that note is from May
  against llama.cpp b9165 and may be stale or already upstream.
- Prompt-cache sizing: halogen runs `prompt_cache cap_mb 110`. A coding agent
  re-sends a long growing prompt every turn, so cache policy may matter more to
  real `task_s` than any decode knob. Needs its own workload with multi-turn
  reuse, not this cold-prefill one.
