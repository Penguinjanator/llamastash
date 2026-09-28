"""Concurrent load against the LlamaStash proxy.

The overhead suite (`orchestrator.py`) sends one request at a time, which
cannot show contention between proxy threads. This drives N concurrent clients
and reports throughput and latency percentiles, for comparing two daemon
builds on the same upstream.

Workloads:
  chat     streaming chat completions; reports output chunks/s and TTFT
  models   GET /v1/models, answered by the proxy itself (no upstream)
  bigbody  a chat request padded with an unused 2 MiB field and
           max_tokens 1, so the proxy buffers and parses a large body

Usage:
  python3 scripts/bench/proxy/concurrency.py --url http://127.0.0.1:11435 \\
      --model <id> --workload chat --concurrency 16 --requests 160

Prints one JSON object on stdout.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import statistics
import time

import httpx


def pct(values: list[float], p: float) -> float:
  if not values:
    return float("nan")
  ordered = sorted(values)
  return ordered[min(len(ordered) - 1, int(round(p / 100 * (len(ordered) - 1))))]


async def one_chat(client: httpx.AsyncClient, url: str, model: str, max_tokens: int):
  body = {
    "model": model,
    "stream": True,
    "max_tokens": max_tokens,
    "messages": [{"role": "user", "content": "Count from one upward in words."}],
  }
  start = time.perf_counter()
  ttft = None
  chunks = 0
  async with client.stream("POST", f"{url}/v1/chat/completions", json=body) as r:
    r.raise_for_status()
    async for line in r.aiter_lines():
      if not line.startswith("data: ") or line == "data: [DONE]":
        continue
      if ttft is None:
        ttft = time.perf_counter() - start
      chunks += 1
  return time.perf_counter() - start, ttft, chunks


async def one_models(client: httpx.AsyncClient, url: str, _model: str, _n: int):
  start = time.perf_counter()
  r = await client.get(f"{url}/v1/models")
  r.raise_for_status()
  return time.perf_counter() - start, None, 0


async def one_bigbody(client: httpx.AsyncClient, url: str, body: bytes):
  start = time.perf_counter()
  r = await client.post(
    f"{url}/v1/chat/completions",
    content=body,
    headers={"content-type": "application/json"},
  )
  r.raise_for_status()
  return time.perf_counter() - start, None, 0


async def run(args) -> dict:
  limits = httpx.Limits(max_connections=args.concurrency, max_keepalive_connections=args.concurrency)
  big = json.dumps(
    {
      "model": args.model,
      "max_tokens": 1,
      "messages": [{"role": "user", "content": "hi"}],
      "x_pad": "x" * (2 << 20),
    }
  ).encode()
  queue: asyncio.Queue[int] = asyncio.Queue()
  for i in range(args.requests):
    queue.put_nowait(i)
  latencies: list[float] = []
  ttfts: list[float] = []
  chunks_total = 0
  errors = 0

  async with httpx.AsyncClient(timeout=args.timeout, limits=limits) as client:

    async def worker():
      nonlocal chunks_total, errors
      while True:
        try:
          queue.get_nowait()
        except asyncio.QueueEmpty:
          return
        try:
          if args.workload == "chat":
            lat, ttft, chunks = await one_chat(client, args.url, args.model, args.max_tokens)
          elif args.workload == "models":
            lat, ttft, chunks = await one_models(client, args.url, args.model, 0)
          else:
            lat, ttft, chunks = await one_bigbody(client, args.url, big)
        except (httpx.HTTPError, OSError):
          errors += 1
          continue
        latencies.append(lat)
        if ttft is not None:
          ttfts.append(ttft)
        chunks_total += chunks

    start = time.perf_counter()
    await asyncio.gather(*(worker() for _ in range(args.concurrency)))
    wall = time.perf_counter() - start

  ms = lambda v: round(v * 1000, 2)
  out = {
    "workload": args.workload,
    "concurrency": args.concurrency,
    "requests": args.requests,
    "errors": errors,
    "wall_s": round(wall, 3),
    "req_per_s": round(len(latencies) / wall, 1),
    "latency_ms": {
      "p50": ms(pct(latencies, 50)),
      "p99": ms(pct(latencies, 99)),
      "mean": ms(statistics.fmean(latencies)) if latencies else None,
    },
  }
  if args.workload == "chat":
    out["chunks_per_s"] = round(chunks_total / wall, 1)
    out["ttft_ms"] = {"p50": ms(pct(ttfts, 50)), "p99": ms(pct(ttfts, 99))}
  return out


def main() -> None:
  ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
  ap.add_argument("--url", default="http://127.0.0.1:11435")
  ap.add_argument("--model", required=True)
  ap.add_argument("--workload", choices=["chat", "models", "bigbody"], default="chat")
  ap.add_argument("--concurrency", type=int, default=16)
  ap.add_argument("--requests", type=int, default=160)
  ap.add_argument("--max-tokens", type=int, default=64)
  ap.add_argument("--timeout", type=float, default=120.0)
  args = ap.parse_args()
  print(json.dumps(asyncio.run(run(args))))


if __name__ == "__main__":
  main()
