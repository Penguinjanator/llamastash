# Autoresearch: fastest Qwen3.8-Flash-Next config on Strix Halo (128 GB)

## Objective

Find the launch configuration that serves `Qwen3.8-Flash-Next` fastest on this
box, for interactive coding-agent use: long real-code prompts, long greedy
reasoning+answer completions. Two runtimes can serve this model here and the
study runs in two phases: a head-to-head at each engine's shipping config, then a
knob sweep inside whichever engine wins.

Hardware: ASUS ROG Flow Z13 (GZ302), Ryzen AI Max+ 395, Radeon 8060S iGPU
(`gfx1151`), 128 GB unified. Kernel-managed 512 MiB VRAM carve plus GTT; the GTT
window is the real budget, not `mem_info_vram_total`.

## Metrics

- **Primary**: `tg_tps` (decode tokens/sec, engine-reported, **higher is better**)
- **Secondary**: `pp_tps` (prefill t/s), `tg_tps_deep` (decode at ~22k prompt),
  `draft_accept` (accepted/draft ratio — the mechanism behind every tg delta),
  `tg_spread_pct` (own noise floor), `task_s` (wall for the workload),
  `engine_rss_gib` / `mem_available_gib` (memory headroom)

Why rate and not duration: two engines emit different token counts for the same
prompt, so wall-clock time is not comparable across them. A rate is. Within one
engine, `task_s` is a valid secondary because output length is pinned.

`tg_tps` is the engine's own decode-window rate, not wall clock. Wall carries
HTTP + tokenisation + detokenisation, which at this workload is the same order
as the deltas being chased.

## How to Run

```sh
REPS=3 ./.auto/measure.sh          # normal measurement
REPS=3 DEEP=32768 ./.auto/measure.sh   # confirmation run, adds a ~22k depth rep
./.auto/checks.sh                  # runs automatically after a passing measurement
```

`measure.sh` brings the engine up if needed and restarts it only when
`scripts/bench/qwen38-flash-speed/engine.sh` actually changed — a knob edit costs
one model load, a no-op edit costs nothing. A measurement is ~3 min once the
engine is warm; an engine switch is far more.

Baseline, halogen 0.15.1 at the live config (`ctx 262144`, `kv_pool 262144`,
`slots 4`, `max_tok 32768`, `prefill_chunk 32768`, **`mtp_depth 3`**, prompt
lookup `3,3`, spec-adapt `32,0.35,64`, prompt_cache 2, matmul plan baked in),
power state 55 W / `performance`:

| metric | value |
|---|---|
| `tg_tps` | **39.63** (spread 3.0 %) |
| `pp_tps` | 1082.9 (±1.5 %) |
| `tg_tps_deep` | 38.19 at 22129 prompt tokens |
| `draft_accept` | 1.317 |
| `task_s` | 62.5 for 10307 in / 2048 out |
| `engine_rss_gib` | 68.7, `mem_available_gib` 90.7 |

## Workload (fixed; changing it invalidates every number above)

~10k tokens of real `src/**/*.rs` from this repo as the prefix, then a request to
write a complete new Rust module with tests and rustdoc, `temperature 0`,
`reasoning_effort xhigh`, `max_tokens 2048`. Every rep uses a disjoint corpus
slice and a unique first line, so it decodes cold.

## Files in Scope

- `scripts/bench/qwen38-flash-speed/engine.sh` — **the knob surface.** Engine
  choice plus each engine's launch args. This is what an experiment edits.
- `scripts/bench/qwen38-flash-speed/bench.py` — the instrument. Edit only to add
  signal, never to change the workload or what is timed; if you do, the baseline
  above stops applying and must be re-established.
- `scripts/bench/qwen38-flash-speed/serve.sh` — engine lifecycle. Edit only if a
  launch mechanism is broken.
- `.auto/measure.sh`, `.auto/checks.sh` — wiring and gates.

## Off Limits

- **Do not stop the halogen container on port 41103** (or anything the llamastash
  daemon on `~/.local/state/llamastash` manages) without the operator's explicit
  go-ahead in this session. `PI_MODEL=flash-next-halogen@solo-256k`: **the agent
  running this study is served by that container.** Killing it ends the session
  mid-experiment.
- The developer's `~/.config/llamastash/config.yaml` (stow-managed symlink) and
  the live daemon's state.
- `/sys/class/drm/card*/device/power_dpm_force_performance_level` — writing this
  caused a hard hang on this hardware requiring a power cycle. See
  `~/dotfiles/LLM-BENCH-NEXT-RESULTS.md`. Do not retry.
- `docs/benchmarks/**` — published evidence with a reproducibility contract.

## Constraints

- **System TDP stays at 55 W / `platform_profile=performance` for the whole
  study.** Same binary and prompt measured 9.5 vs 13.9 t/s across power states
  on this machine. Record `z13ctl tdp --get` and battery with any number you
  write down; a t/s figure without power context is not comparable.
- Only one engine loaded at a time. gufo's `UD-Q4_K_XL` is ~105 GB and halogen
  holds ~68 GB resident; 124 GB total, no swap. Anything that would exceed that
  thrashes page cache and produces garbage numbers, not OOM. `serve.sh` refuses
  when another engine holds the GPU.
- `checks.sh` must pass before a `keep`. It rejects: a prefix-cache hit inside a
  rep, any rep that stopped before `max_tokens`, degenerate output, and any change
  to the generated text for a given engine (per-engine golden, because a
  speculative path that accepts a wrong draft is a bug that looks like a win).
- No new Python deps; stdlib only. Two-space indent and `set -euo pipefail` in
  shell, matching repo style.
- **Do not overfit the benchmark.** `draft_accept` on an enumerative or
  self-referential prompt is inflated relative to real prose, so a knob that only
  wins there may lose in production. Confirm any winner the mechanism explains
  (`draft_accept` should move with `tg_tps`; a tg gain at flat `draft_accept` is
  a timing artifact) and re-check the best two at `DEEP=32768`.
- Reasoning effort, sampling, and `max_tokens` are not speed knobs. They change
  what is generated. Held fixed.

## What's Been Tried

Baseline recorded: halogen shipping config, `tg_tps 39.63`, `pp_tps 1082.9`.
gufo head-to-head is **blocked** — needs the operator's decision, see Off Limits.

Things the harness already had to learn the hard way, so they are not re-learned:

- **Decode rate falls with depth**: 41.9 t/s at 60 prompt tokens, 31.1 at 2.7k,
  ~33 at 10k with a short answer, 39.6 at 10k with a long one, 38.2 at 22k. Any
  configuration tuned at an empty context is tuned for a workload nobody runs.
- **A shallow warm-up is worse than none.** A 16-token warm-up left rep0 at
  `pp 187` against a steady 1080 — clocks and pages never ramped. Warm-up now
  runs at depth, on its own corpus slice.
- **The engines' prompt caches outlive the benchmark process.** Re-running the
  same corpus gave `pp 0.0` with `cached == prompt`: every rep was a cache read.
  Each rep now sends a unique first line.
- **Reasoning-effort off makes the model answer in ~300 tokens**, so `finish=stop`
  and the decode denominator wanders 184-377 across reps. xhigh at
  `max_tokens 2048` pins `finish=length` and cut spread from 12 % to 3 %.
- **An easy question is noise.** Asking for a module list yielded 447-725 tokens
  and 12 % spread; asking for a whole new file yielded exactly 2048 and 3 %.

## Ideas not yet tried

Shortlist from web research, with sources, is in `.auto/ideas.md`. Three things
there change how to read every number in this file:

- The vendor's reference machine is measured at **~85 W sustained** and names the
  power envelope and the IOMMU setting as the two cross-machine differences.
  This box is at 55 W with IOMMU on, which is most of the gap to the published
  1,584 prefill / 46.0 decode.
- Upstream halogen is **0.16.1** and ships "decode is faster on both
  checkpoints". Any sweep should re-baseline after an image bump.
- llama.cpp issue #27856: on HIP/gfx1151 decode fell 19-21 to 5.5-6.1 t/s past
  ~1K context (CPU fallback for `ggml_top_k` above 1024 columns), fixed by PR
  #27466 on 2026-08-31; Vulkan was never affected. Any gufo decode number needs
  a depth curve before it is believed.
