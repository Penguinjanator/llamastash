#!/usr/bin/env python3
"""Follow-up prefill (cache reuse): --parallel N vs N separate launches.

The throughput run cold-prefilled every request, so it said nothing about cache.
Here three sessions each run three turns, all three sessions firing each turn
concurrently, which is the real parallel-sessions shape. Assistant turns are
synthetic so prefill is measured independently of generation.

What matters is `prefilled` on turns 2 and 3: small means the slot kept the
session's prefix, large means it was lost and re-prefilled.
"""
import json
import subprocess
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

MODEL = "/mnt/work/huggingface/hub/models--unsloth--Qwen3.8-27B-GGUF/snapshots/4ca720788d1e01f1bff70c033e0d0028fd02e502/Qwen3.8-27B-UD-Q6_K.gguf"
BIN = "/mnt/work/Workspace/llms/llama.cpp/build-hip/bin/llama-server"
SLOT_CTX = 16384
MAX_TOKENS = 64
BASE = 46300
N = 3

ASSIST = "Understood. I have read the code and I am ready for follow-up questions."
FOLLOWUPS = ["Which module allocates most?", "Now add a timeout to step()."]


def body(seed):
    return "\n".join(
        f"// module {i + seed * 1000}\npub struct Unit{i + seed * 1000} {{ pub id: u64 }}\n"
        f"impl Unit{i + seed * 1000} {{ pub fn step(&self, b: u32) -> u32 {{ b + {i % 17} }} }}\n"
        for i in range(45))


def convo(sess, turn):
    """turn 1 = cold context. turn 2,3 = prior exchange + a short follow-up."""
    msgs = [{"role": "user",
             "content": f"Session S{sess}. Here is the codebase.\n\n{body(sess)}\n\nSummarise it."}]
    for k in range(turn - 1):
        msgs.append({"role": "assistant", "content": ASSIST})
        msgs.append({"role": "user", "content": FOLLOWUPS[k]})
    return msgs


def launch(port, np_):
    import socket
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.bind(("127.0.0.1", port))
    except OSError as e:
        raise RuntimeError(f"port {port} busy: {e}")
    finally:
        s.close()
    args = [BIN, "-m", MODEL, "--host", "127.0.0.1", "--port", str(port),
            "-c", str(SLOT_CTX * np_), "-np", str(np_),
            "-ngl", "99", "-fa", "on", "--jinja",
            "--spec-type", "draft-mtp", "--spec-draft-n-max", "5",
            "--spec-draft-p-min", "0.0",
            "--reasoning-format", "deepseek", "--reasoning-effort", "medium",
            "--cont-batching", "--temperature", "0.0",
            "--presence-penalty", "0.0", "--repeat-penalty", "1.0"]
    log = open(f"/tmp/claude-1000/cnl_{port}.log", "w")
    p = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT)
    for _ in range(900):
        time.sleep(1)
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2) as r:
                if r.status == 200:
                    return p, log
        except Exception:
            pass
        if p.poll() is not None:
            raise RuntimeError(f"server {port} died")
    raise RuntimeError("unhealthy")


def ask(port, msgs):
    payload = json.dumps({"messages": msgs, "max_tokens": MAX_TOKENS,
                          "temperature": 0.0, "stream": False}).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}/v1/chat/completions",
                                 data=payload,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=3600) as r:
        out = json.loads(r.read())
    t = out.get("timings", {})
    return {"prefilled": t.get("prompt_n"),
            "prefill_tps": round(t.get("prompt_per_second", 0), 1),
            "prefill_ms": round(t.get("prompt_ms", 0))}


def stop(pl):
    for p, log in pl:
        p.terminate()
        try:
            p.wait(timeout=90)
        except subprocess.TimeoutExpired:
            p.kill()
        log.close()
    time.sleep(12)


res = {}


def arm(name, batched):
    print(f"\n##### {name}", flush=True)
    if batched:
        procs = [launch(BASE, N)]
        ports = [BASE] * N
    else:
        procs = [launch(BASE + 10 + i, 1) for i in range(N)]
        ports = [BASE + 10 + i for i in range(N)]
    turns = []
    for turn in (1, 2, 3):
        t0 = time.time()
        with ThreadPoolExecutor(max_workers=N) as ex:
            fs = [ex.submit(ask, ports[i], convo(i, turn)) for i in range(N)]
            rows = [f.result() for f in fs]
        wall = round(time.time() - t0, 1)
        turns.append({"turn": turn, "wall_s": wall, "streams": rows})
        print(f"  turn {turn}: wall {wall}s | prefilled {[r['prefilled'] for r in rows]}"
              f" | prefill_ms {[r['prefill_ms'] for r in rows]}"
              f" | t/s {[r['prefill_tps'] for r in rows]}")
        sys.stdout.flush()
    stop(procs)
    res[name] = turns


arm("np3_one_server", True)
arm("three_separate_launches", False)

json.dump(res, open("/tmp/claude-1000/cache_np_vs_launches.json", "w"), indent=2)
print("\nwrote /tmp/claude-1000/cache_np_vs_launches.json")
