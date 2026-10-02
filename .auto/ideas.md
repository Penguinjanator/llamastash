# Ideas backlog — shortlist from research (2026-10-02)

Baseline: halogen 0.15.1 at the live config, `tg 39.63`, `pp 1082.9` at ~10k
depth, **55 W**. Live env read from the container: `MTP_DEPTH=3`, `CTX=262144`,
`KV_POOL_POSITIONS=262144` (not the 2x default), `KV_SLOTS=4`, `MAX_TOK=32768`,
`PREFILL_CHUNK=32768`, `PROMPT_CACHE=2`, `ATTN_FA=64`, `MATMUL_ALGOS=1`,
`MATMUL_TUNING_FILE=/opt/halogen/flash-tune.plan`, image 0.15.1.

Two facts frame everything below. The vendor's reference machine measures
**~85 W sustained package power** (README §Measured) and calls out only two
cross-machine differences worth reading before comparing numbers: **the power
envelope and the IOMMU setting.** This box is at 55 W with IOMMU on.

## Tier 0 — system levers, largest measured effect, operator's call

Not benchmark knobs. Each is a machine change with a quality-of-life cost, so they
get decided, not discovered.

1. **Package power 55 W → 75 W** (`z13ctl tdp --set 75`). PL1 is capped at 75 W
   by default, 93 W only with `--force` plus a fan-curve floor. Vendor reference
   is ~85 W at 46.0 decode / 1,584 prefill; we are at 39.6 / 1,083 at 55 W.
   Cost: heat, fan, battery. This is plausibly deliberate — ask before touching.
2. **CPU governor `schedutil` → `performance`.** All 32 cores are on schedutil.
   The level1techs tuning pass named the power profile plus governor the
   "dominant factor", moving GPU clocks ~2000 → ~2850 MHz. Cheap and reversible.
3. **IOMMU.** `ivhd0` is present, so AMD IOMMU is active, and the kernel cmdline
   has no `amd_iommu=` flag. Vendor README measures it as a first-order
   difference; the forum pass credits `amd_iommu=off` with ~7-15%. Requires a
   reboot and gives up DMA isolation. **Conflict: `docs/NPU.md` says the NPU
   needs IOMMU on**, so this and the NPU path are mutually exclusive. Needs
   kernel-doc verification before recommending, not inference.

## Tier 1 — halogen knobs, in-process, cheap to A/B

4. **Image bump 0.15.1 → 0.16.1.** Changelog: "Decode is faster on both
   checkpoints, most with several conversations generating at once." Cheapest
   real win on the list; measure it first so later deltas are against current
   upstream.
5. **Re-tune the GEMM plan locally.** `flash-tune.plan` is baked into the image,
   tuned on the reference machine at ~85 W; the docs say a different SKU loads it
   with a warning and give the recipe: point `HALOGEN_MATMUL_TUNING_FILE` at a
   path that does not exist, run `HALOGEN_MATMUL_ALGOS=8`, stop cleanly. Our
   power envelope differs from the box it was tuned on, which is exactly the case
   for retuning. Do not set `ALGOS=8` without a file — cross-restart identity is
   lost.
6. **`HALOGEN_MTP_DEPTH` 3 → 2.** Live runs 3. Docs: 2 is best all-round, 3 is
   better for code (61 vs 57 on their box) but 5-8 % slower on agent/prose. Our
   benchmark is code-heavy, which flatters 3; measure both and weight by what
   Deepu actually runs.
7. **`HALOGEN_SPEC_ADAPT` `32,0.35,64` → `0`.** Default switches the draft head
   off for 64 tokens when a 32-round window falls under 0.35 acceptance. Byte-
   identical either way, speed only. On high-acceptance code it may cut drafting
   it shouldn't.
8. **`HALOGEN_PLD` (prompt lookup) `3,3` → `0`, and try other `N,K`.** Vendor's
   own headline (55.7-56.3 t/s) is draft head **plus prompt lookup** on a
   coding-agent turn. `K` above 3 needs verify rows the image may not reserve.
9. **`HALOGEN_KV_SLOTS` 4 → 1.** A slot is ~115 MB, but 4 slots exist to trade
   per-stream speed for admitting concurrent clients: four streams total ~76
   t/s. Single-user harness pays that for nothing.
10. **`HALOGEN_CTX` + `KV_POOL_POSITIONS` 262144 → 131072.** Documented recipe:
    ~74 GiB instead of ~89 GiB. The pool takes RAM the 47 GiB n-gram lookup
    table's page cache would otherwise keep, and that table is streamed through
    the file cache on every request. Decode speed follows conversation length,
    not pool size, so the gain is cache pressure, not attention.
11. **`HALOGEN_HOST_RESERVE_GIB` 20 → sweep.** Same trade from the other side:
    how much RAM the pool sizing must leave free for the lookup table.
12. **`HALOGEN_MAX_TOK` 32768 → 16384.** Halves the ~9 GiB prefill scratch,
    costs ~9 % prefill. Only a win if we are cache-thrashing.

## Tier 2 — engine alternatives, needs the RAM / session-model decision

13. **gufo head-to-head** (harness already staged). gufo tree is `d707143`,
    2026-10-01, reports `version development (unknown)`.
14. **Check gufo for the HIP depth cliff.** llama.cpp issue #27856: on
    HIP/gfx1151, decode fell 19-21 → 5.5-6.1 t/s past ~1K context because
    `ggml_top_k` fell back to CPU once `ne[0] > 1024` (QSA indexer), a 3.5-4x
    cliff. Fixed by PR #27466 (radix TOP_K for long rows), merged 2026-08-31.
    Vulkan/RADV was never affected. Verify with `gufo bench -d` at depth before
    believing any gufo decode number.
15. **Vulkan/RADV instead of ROCm.** Several independent Strix Halo reports put
    Vulkan ahead on decode, and it sidesteps #27856 entirely.
16. **Draft-head quant: Q5_K beat Q8_0** on llama.cpp discussion #27950 — +0.6 %
    to +8.7 % across five workloads, and a cleanly-rebuilt head was worse than a
    requantized one (58.5 vs 61.0 short code). Live uses
    `mtp-Qwen3.8-Flash-Next-shared-Q8_0.gguf` on the gufo row. Only applies to
    the GGUF route.
17. **halogen BYOGGUF**: `HALOGEN_CHECKPOINT` may name a llama.cpp GGUF since
    0.7.0 (repacked in RAM at start, needs `HALOGEN_MTP_HEAD`). Lets one engine
    compare weight sources. A GGUF trunk costs ~70 GiB repack.
18. **llama.cpp-side flags worth porting if the GGUF route wins**: `-ub 1024 -b
    2048 -t 4 -tb 16` (`-ub 2048` unlocks the `GGML_VK_MMID_M128` tile), Q8_0
    KV, `--spec-draft-n-max 6 --spec-draft-p-min 0.75`, `taskset -c 0-7`,
    `GGML_VK_MAX_MB_PER_SUBMIT=2048`. From julianmb/haloq38flash, which runs a
    91 GiB IQ4_XS-PLE quant at 27.9 MTP tg at 8k depth — well below our 39.6.

## Deliberately not on the list

- **NPU offload for the Flash model.** `HALOGEN_NPU_MODELS` serves small sidecars
  (decider/embeddings/reranker) on the NPU, not the Flash model, and the Flash
  model runs slower while the NPU works. Also incompatible with candidate 3.
- **`HALOGEN_INDEXER_BUDGET` above 2048.** Costs prefill and is a different model
  configuration, not a cache setting.
- **`qwen38-flash-next-ht43.hgn`** (0.16.0's smaller checkpoint): ~8 GiB less
  RAM, slower prefill and a few percent slower decode. A memory lever, not speed.
- **YaRN to 1M context**: more RAM, same speed per the forum pass.

## Overfit warning carried from the instrument

Vendor's own numbers say draft-head gains depend on workload class (3 beats 2 on
code, loses 5-8 % on prose). Every candidate that works through draft acceptance
must be re-measured on a prose-shaped task before it is called a win.
