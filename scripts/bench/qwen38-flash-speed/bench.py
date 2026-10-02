#!/usr/bin/env python3
"""Measure Qwen3.8-Flash-Next serving speed through the OpenAI surface.

Engine-agnostic on purpose: gufo (GGUF) and halogen (.hgn) report timings under
different keys, and the study's whole point is comparing the two, so the only
fair instrument is the one both are seen through.

WHY THE ENGINE'S OWN t/s AND NOT WALL CLOCK. The generated text is identical
across configurations that are correct, so any difference we chase is a few
percent. Wall clock carries HTTP, tokenisation and detokenisation on top of the
decode window, which at a few hundred tokens is itself a few percent, the same
size as the effect. Both engines report a decode-window rate measured inside
the engine; we take that and keep wall clock only as a secondary column, where a
gap against the engine number is itself the finding (the front end is the cost).

DEPTH IS NOT OPTIONAL. Decode at an empty context and decode at 32K are different
machines: the KV reads grow and draft acceptance changes. Every rep therefore
runs behind a real corpus prefix, and the prefix's measured token count is
reported alongside, never the requested one.

THE PREFIX IS REAL CODE, AND THAT IS A LOAD-BEARING CHOICE. Padding with repeated
text or random ids makes the future trivially predictable, which inflates
speculative acceptance and flatters exactly the knob under study. Prefixes come
from this repository's own sources, are disjoint between reps, and their sha256
goes into the output so a later reader can tell which corpus a number came from.

One rep per configuration is indicative, not quotable; pass --reps 3 or more and
read the spread before believing a delta.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import statistics
import subprocess
import sys
import time
import uuid
import urllib.request

QUESTION = (
    "A new requirement arrives for the crate above. Add a module that tracks a "
    "rate limiter over the same state these modules already own: token bucket, "
    "per-key map, eviction under memory pressure, unit tests for each branch, "
    "and the error types. Write the whole file in Rust, compilable, with rustdoc "
    "on every public item. Do not stop until the file is complete."
)


def repo_prefixes(reps: int, chars: int) -> list[tuple[str, int]]:
    """`reps` disjoint slices of real repository source, each ~chars long."""
    files = subprocess.run(
        ["git", "ls-files", "src/**/*.rs"],
        capture_output=True, text=True, check=True,
    ).stdout.split()
    files = [f for f in files if os.path.getsize(f) > 2000]
    out: list[tuple[str, int]] = []
    buf: list[str] = []
    size = 0
    idx = 0
    for path in files:
        if size >= chars * reps:
            break
        try:
            text = open(path, encoding="utf-8", errors="replace").read()
        except OSError:
            continue
        buf.append(f"// ===== {path} =====\n{text}\n")
        size += len(text) + 30
        if size >= chars:
            out.append(("".join(buf), idx))
            buf, size, idx = [], 0, idx + 1
    if len(out) < reps:
        raise SystemExit(f"corpus too small: built {len(out)} of {reps} prefixes")
    return out[:reps]


def ask(base: str, model: str, engine: str, prompt: str, max_tokens: int,
        effort: str, nonce: str = "") -> dict:
    if nonce:
        # Both engines keep prompt prefixes across requests, and that cache
        # outlives a benchmark process. A rep whose prefix is already resident
        # reports a prefill rate that has nothing to do with the configuration,
        # so every rep gets a unique first line and is measured cold.
        prompt = f"session {nonce}\n\n{prompt}"
    body: dict = {
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0,
        # Effort is pinned to the production default for the whole study. It is
        # a quality dial: measuring "faster" at a lower effort would be
        # measuring a shorter answer, and reasoning_content is inside the timed
        # decode window. Depth and draft knobs are what this study may move.
        "reasoning_effort": effort,
        "stream": False,
    }
    req = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=3600) as resp:
        payload = json.loads(resp.read())
    wall = time.perf_counter() - t0

    usage = payload.get("usage") or {}
    timing = payload.get("timings") or {}
    if engine == "halogen":
        pp = timing.get("prompt_per_second", 0.0)
        tg = timing.get("predicted_per_second", 0.0)
        draft, accepted = timing.get("draft_n", 0), timing.get("draft_n_accepted", 0)
        prefix_cached = timing.get("prefix_n", 0)
    else:
        pp = usage.get("prompt_tokens_per_second", 0.0)
        tg = usage.get("completion_tokens_per_second", 0.0)
        draft = usage.get("draft_tokens", 0)
        accepted = usage.get("draft_tokens_accepted", 0)
        prefix_cached = usage.get("cached_tokens", 0)
    choice = (payload.get("choices") or [{}])[0]
    message = choice.get("message") or {}
    text = (message.get("reasoning_content") or "") + (message.get("content") or "")
    return {
        "prompt_tokens": usage.get("prompt_tokens", 0),
        "completion_tokens": usage.get("completion_tokens", 0),
        "pp_tps": round(pp, 2),
        "tg_tps": round(tg, 3),
        "draft_tokens": draft,
        "draft_accepted": accepted,
        # A prefix cache hit inside a rep would report a fast pp that has nothing
        # to do with the configuration, so surface it rather than trust the knob.
        "prefix_cached_tokens": prefix_cached,
        "wall_s": round(wall, 2),
        "finish": choice.get("finish_reason"),
        "text": text,
    }


def host_memory() -> dict:
    mem = {}
    for line in open("/proc/meminfo"):
        key, _, rest = line.partition(":")
        mem[key] = int(rest.strip().split()[0])
    rss = 0
    for pid in subprocess.run(["pgrep", "-x", "flash_serve"],
                              capture_output=True, text=True).stdout.split():
        try:
            for line in open(f"/proc/{pid}/status"):
                if line.startswith("VmRSS"):
                    rss += int(line.split()[1])
        except OSError:
            pass
    for pid in subprocess.run(["pgrep", "-x", "gufo"],
                              capture_output=True, text=True).stdout.split():
        try:
            for line in open(f"/proc/{pid}/status"):
                if line.startswith("VmRSS"):
                    rss += int(line.split()[1])
        except OSError:
            pass
    return {"mem_available_gib": round(mem.get("MemAvailable", 0) / 1048576, 1),
            "engine_rss_gib": round(rss / 1048576, 1)}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--engine", choices=["gufo", "halogen"], required=True)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--depth-tokens", type=int, default=8192)
    ap.add_argument("--max-tokens", type=int, default=2048)
    ap.add_argument("--effort", default="xhigh")
    ap.add_argument("--deep-tokens", type=int, default=0,
                    help="also run one rep at this depth (slower, for confirmations)")
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    # ~3.2 chars/token on Rust; the measured count is what we report.
    chars = int(args.depth_tokens * 3.2)
    # reps + warm-up slice + two deep slices. Disjoint, so no rep reads another's
    # prefix and the deep rep cannot hit a cached short prefix.
    prefixes = repo_prefixes(args.reps + 3, chars)
    corpus_sha = hashlib.sha256(
        "".join(p[0] for p in prefixes).encode()).hexdigest()[:12]

    # Untimed. Ramps clocks and pages before rep0, on its own slice so it cannot
    # leave a prefix cached that a rep would then read instead of prefills.
    ask(args.base, args.model, args.engine,
        prefixes[args.reps][0] + "\n\n" + QUESTION, 64, args.effort)

    reps = []
    for i in range(args.reps):
        prompt = prefixes[i][0] + "\n\n" + QUESTION
        r = ask(args.base, args.model, args.engine, prompt, args.max_tokens,
                args.effort, nonce=uuid.uuid4().hex)
        r["rep"] = i
        reps.append(r)
        print(f"  rep{i} pp={r['pp_tps']} tg={r['tg_tps']} "
              f"prompt={r['prompt_tokens']} out={r['completion_tokens']} "
              f"draft={r['draft_tokens']}/{r['draft_accepted']} "
              f"cached={r['prefix_cached_tokens']} wall={r['wall_s']}s "
              f"finish={r['finish']}", file=sys.stderr)

    deep = None
    if args.deep_tokens:
        prompt = prefixes[-2][0] + prefixes[-1][0] + "\n\n" + QUESTION
        deep = ask(args.base, args.model, args.engine, prompt, args.max_tokens,
                   args.effort, nonce=uuid.uuid4().hex)
        print(f"  deep tg={deep['tg_tps']} at prompt={deep['prompt_tokens']}",
              file=sys.stderr)

    med = lambda k: statistics.median(r[k] for r in reps)  # noqa: E731
    draft = sum(r["draft_tokens"] for r in reps)
    accepted = sum(r["draft_accepted"] for r in reps)
    out_tok = int(med("completion_tokens"))
    metrics = {
        "tg_tps": round(med("tg_tps"), 2),
        "pp_tps": round(med("pp_tps"), 2),
        "task_s": round(med("wall_s"), 2),
        "out_tok": out_tok,
        "all_length": int(all(r["finish"] == "length" for r in reps)),
        "prompt_tok": int(med("prompt_tokens")),
        "draft_accept": round(accepted / draft, 3) if draft else 0.0,
        "tg_spread_pct": round(
            100.0 * (max(r["tg_tps"] for r in reps) - min(r["tg_tps"] for r in reps))
            / med("tg_tps"), 1),
    }
    if deep:
        metrics["tg_tps_deep"] = deep["tg_tps"]
        metrics["deep_prompt_tok"] = deep["prompt_tokens"]
    metrics.update(host_memory())

    for k, v in metrics.items():
        print(f"METRIC {k}={v}")

    if args.out:
        os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
        with open(args.out, "w") as fh:
            json.dump({"engine": args.engine, "corpus_sha": corpus_sha,
                       "depth_target": args.depth_tokens,
                       "max_tokens": args.max_tokens, "effort": args.effort,
                       "metrics": metrics, "reps": reps, "deep": deep},
                      fh, indent=1)
    # Rep 0's text is the identity witness: a config that decodes faster by
    # decoding different text is a bug, not a win.
    with open(os.environ.get("IDENTITY_OUT", "/tmp/ar-identity.txt"), "w") as fh:
        fh.write(reps[0]["text"])
    print(f"# corpus={corpus_sha} engine={args.engine}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
