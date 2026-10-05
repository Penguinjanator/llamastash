# Local Bench Scripts

`local/lib-power.sh` carries the shared `battery` / `on_ac` / `guard` helpers plus
`wait_for_charge`, which parks a long run with the model stopped until the pack
recovers (`PAUSE_BELOW`, `RESUME_AT`, `MIN_BATT`). Source it, do not run it. It
aborts rather than continuing if the pack sits flat for five minutes while idle,
because at a high TDP this machine can draw more than the charger supplies and
the resulting numbers look valid but are not.

## `preset-ab.sh` — compare named presets from the real config

```sh
scripts/bench/preset-ab.sh <llamastash-binary> <model-substring> <preset>...
ROUNDS=4 scripts/bench/preset-ab.sh ...                    # default 2
LS_REAL_CONFIG=~/cand.yaml scripts/bench/preset-ab.sh ...  # bench candidates
```

Answers "which preset should I code with?" by measuring the agent loop rather
than raw decode: one cold prefill, then follow-up turns that append the way a
coding session does. Takes any number of presets.

Two traps it exists to avoid, both of which silently produce wrong answers:

**`last_params` bleed.** `start --preset X` layers the model's last successful
launch _under_ the preset, so a back-to-back A/B contaminates itself — the
second preset inherits every knob the first set that it does not declare
itself. Observed live: a `pi-coding` run came up carrying `--threads 16
--cache-type-k q8_0 --flash-attn on --batch-size 2048` from the `pi-cache` run
before it. `state.json` is wiped between launches so each preset resolves from
its own definition alone, and the composed argv is printed per launch. Confirm
with `tr '\0' '\n' < /proc/<pid>/cmdline` whenever a number looks surprising.

**Order bias.** Whichever preset runs first gets the cool GPU. Rounds alternate
the order; an odd `ROUNDS` still leaves a bias, so use an even number. Note that
a `for round; for preset` loop does _not_ alternate on its own — it repeats the
same order every round.

Preset bodies are copied verbatim into a sandbox config/state/cache dir, so the
real `config.yaml` is never a write target.

## `nommap-ab.sh` — the `no-mmap` knob, one preset, both states

```sh
scripts/bench/local/nommap-ab.sh <llamastash-binary> <model-substring> <base-preset>
```

Same harness as `preset-ab.sh` (sandbox config, alternating rounds, power
watchdog) but the two arms are the base preset with `no-mmap` forced on and
forced off — a preset that already pins it gets a clean removed arm, instead of
trusting CLI override layering. Arms are generated as `ab-mmap` / `ab-nommap`
named presets via a PyYAML round-trip of the real config (comments dropped,
real file untouched).

Adds the two numbers preset-ab cannot see: cold load time (the `start --wait`
wall clock) and peak GTT residency, sampled from the amdgpu sysfs node every
2 s — RSS is useless on this UMA box, it omits GTT entirely. MemAvailable +
Cached are sampled alongside: mmap parks weights in reclaimable page cache,
no-mmap reads them into anon memory, and the pair tells you which happened.

Every launch prints the mmap-related argv flags read back from the live
process (`--no-mmap`, `--mlock`, `--lazy-mode`, `-lm`, `--direct-io`) — an arm
that silently lost its flag invalidates the comparison, so check that line
whenever a number looks surprising.

## `preset-coding-task.sh` — same presets, real work, answers kept

```sh
scripts/bench/preset-coding-task.sh <llamastash-binary> <model-substring> <presetA> <presetB>
```

`preset-ab.sh` says which preset is faster. This says which one you would rather
code with: real repo material as context (README + architecture doc + the
backend registry), real questions, and a token budget large enough that the
reasoning block finishes and an actual answer comes out.

Set the budget high enough. With a 300- or 600-token cap every turn on a model
at `xhigh` reasoning effort comes back with `finish_reason: length` and an
_empty_ `content` — the cap is consumed before the answer starts. That measures
the cap, not the model, and reads convincingly like the model degenerating.

Questions and full answers go to stdout and to `$ROOT/answers/<preset>-<n>.md`,
because throughput cannot tell you which preset reasons better.

## `run-phase3.sh` — turbo4 verdict, then the sampling question

```sh
scripts/bench/run-phase3.sh <llamastash-binary> <model-substring>
```

Runs the corrected cache-correctness gate, then a six-way sampling and
reasoning comparison. Both exist because of mistakes the earlier phases made.

**An empty answer is not a wrong answer.** The first correctness run scored
turbo4 FAIL on three empty strings at a 300-token budget and concluded the KV
cache was corrupting values. Corruption emits a _different_ token; an exhausted
budget emits none. Upstream's `speed` profile ships TurboQuant KV _with_ prompt
checkpoints and reports a divergent-tail follow-up restoring in 4-5s on v1.5.2+,
so turbo4 is endorsed for this exact case -- the `cache` profile's q8_0 is
conservatism for stable shared prefixes, not a prohibition. The gate now records `finish_reason` and, decisively,
`reasoning_content` — with `--reasoning-format deepseek` the thinking is split
out there, so a token recalled while thinking proves the cache read correctly
even when no answer was emitted. Verdicts are `PASS` / `PASS*` (recalled in the
thinking) / `WRONG:<token>` / `NO-ANSWER` / `INCONCLUSIVE`, and only `WRONG` is
a cache failure.

**Vendor guidance does not transfer unchanged to this stack.** Qwen specifies
temperature 1.0 / top_k 20 / top_p 0.95 / min_p 0.0 for thinking mode, and this
preset runs full greedy at 0.0. But the model here is a ROCmFP4 quant on a fork
running MTP speculative decoding, and upstream measured draft acceptance at
~88% at temp 0 falling to ~25% at temp 0.8, which returns decode to unassisted
speed — roughly 2.5x slower. The vendor optimises for output quality, the fork
for speculative throughput, and they disagree. Both get measured; temp 0.2 is
included because it is the top of upstream's stated throughput band.

Reasoning levels come from the GGUF's own chat template, not the docs: only
`xhigh`, `medium` and `low` are accepted (`none` raises), and only `xhigh` and
`low` inject instruction text — `medium` is the _absence_ of steering, not a
tuned middle. Unsloth's page listing `none` is wrong for this template.

`preset-reasoning-ab.sh` reports MTP acceptance per turn, since that is the
number that decides whether a temperature raise is affordable at all.

## `parallel-vs-launches.py` — one server with N slots, or N named launches?

```sh
scripts/bench/local/parallel-vs-launches.py     # edit MODEL/BIN at the top
```

Answers "should a second concurrent session get `--parallel 2`, or its own
launch?" Runs both shapes over one identical workload and reports prefill,
decode, follow-up prefill, wall clock, GTT and **MTP draft acceptance** per
request. Acceptance is the reason this harness exists: on a batched server with
two or more slots decoding at once, `draft acceptance` was measured at exactly
`0.00000` while a single-slot launch on the same build held ~0.41-0.80. MTP is
worth 2.3-3.1x on this model, so an arm that silently loses it looks like
"batching is slow" unless acceptance is on the sheet.

Two traps this harness exists to avoid, both of which produced wrong answers
first time round:

- **Do not vary `-np` and total `-c` together.** A batched server needs
  `N * per-slot` to give each stream the same window, so the naive comparison
  changes two things at once. Upstream issue #23658 reports acceptance collapsing
  at particular `--ctx-size` values independently of slots, so the two are easy
  to confuse. Part A pins this down: at `-np 1`, ctx 16384 / 32768 / 49152 all
  gave identical acceptance, and `-np 2` / `-np 3` with a single active request
  did too. Only concurrent batched decode moves it.
- **Client `max_tokens` below ~8k measures nothing on this model.** At `xhigh`
  the whole budget goes to thinking and the answer comes back empty, which is a
  truncation artefact rather than a throughput sample. Every row records
  `finish_reason` and `answer_chars`; treat a short `n_decode` as invalid, not
  as a data point. `preset-reasoning-ab.sh` above makes the same point.

Scope: one model, one quant, one machine, one llama.cpp build. The MTP-batching
interaction is build-dependent (PR #22838 reworked parallel drafting in May
2026), so re-run it after a llama.cpp bump rather than trusting old numbers.

## `parallel-followup-prefill.py` — does a slot keep its prefix across turns?

```sh
scripts/bench/local/parallel-followup-prefill.py
```

Three sessions, three turns each, all three firing every turn concurrently.
Reports `prefilled` and `prefill_ms` per turn. Both shapes kept every session's
prefix (41 tokens re-prefilled on turn 2 rather than the full 2.2k), so slot
affinity survives other sessions running in between; the difference is in
follow-up prefill *time*, not in whether the cache survives.
