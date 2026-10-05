#!/usr/bin/env python3
"""Definitive run: --parallel N vs N launches, with the ctx-size confound removed.

PART A  Does MTP draft acceptance collapse because of n_parallel, or because of
        the --ctx-size value? Upstream issue #23658 reports acceptance dropping
        to ~0 at particular ctx sizes regardless of slots, and my earlier runs
        varied -np and total -c together, so they cannot tell these apart.
        Single short request per config; the number that matters is acceptance.

PART B  Realistic coding task at ~30k context. Per-slot window is held at 32768
        for EVERY arm, so a batched server allocates N*32768 total. Single-slot
        controls at those same totals are included, so any acceptance collapse
        in a batched arm can be attributed to -np rather than to -c.

Acceptance is parsed from the server's own log per request. finish_reason and
answer length are recorded so a truncated generation is visible rather than
silently treated as a throughput sample.
"""
import glob
import json
import os
import re
import subprocess
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

MODEL = "/mnt/work/huggingface/hub/models--unsloth--Qwen3.8-27B-GGUF/snapshots/4ca720788d1e01f1bff70c033e0d0028fd02e502/Qwen3.8-27B-UD-Q6_K.gguf"
BIN = "/mnt/work/Workspace/llms/llama.cpp/build-hip/bin/llama-server"
SLOT_CTX_B = 32768          # per-slot window for every Part B arm
MAX_TOKENS_A = 120
MAX_TOKENS_B = 1024
PORT = 47100

ACC_RE = re.compile(r"draft acceptance = ([0-9.]+)")


def power():
    try:
        return (open("/sys/class/power_supply/BAT0/status").read().strip(),
                int(open("/sys/class/power_supply/BAT0/capacity").read().strip()))
    except Exception:
        return ("?", -1)


def guard():
    st, cap = power()
    if st == "Discharging" and cap < 60:
        raise SystemExit(f"ABORT: pack discharging at {cap}% (charger cannot carry the load)")
    return f"{st} {cap}%"


def gtt():
    m = 0
    for p in glob.glob("/sys/class/drm/card*/device/mem_info_gtt_used"):
        try:
            m = max(m, int(open(p).read().strip()) // 1048576)
        except Exception:
            pass
    return m


def mods(n, seed=0):
    return "\n".join(
        f"// module {i+seed}\npub struct Unit{i+seed} {{ pub id: u64, pub label: String }}\n"
        f"impl Unit{i+seed} {{\n"
        f"    pub fn step(&mut self, b: u32) -> Result<u32, Error> {{\n"
        f"        if b < {i % 13} {{ return Err(Error::Budget); }}\n"
        f"        Ok(b - {i % 13})\n    }}\n}}\n" for i in range(n))


BODY_30K = mods(430)
TASKS = ["Name the three functions most likely to overflow and explain why.",
         "Propose one trait that unifies these structs, with a signature.",
         "Describe how you would add a cancellation token to step()."]
ASSIST = "Understood. I have read the code and am ready for follow-ups."
FOLLOWUP = "Now which one would you refactor first, and why?"


def launch(port, np_, ctx_total, tag):
    import socket
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.bind(("127.0.0.1", port))
    except OSError as e:
        raise RuntimeError(f"port {port} busy: {e}")
    finally:
        s.close()
    args = [BIN, "-m", MODEL, "--host", "127.0.0.1", "--port", str(port),
            "-c", str(ctx_total), "-np", str(np_),
            "-ngl", "99", "-fa", "on", "--jinja",
            "--spec-type", "draft-mtp", "--spec-draft-n-max", "5",
            "--spec-draft-p-min", "0.0",
            "--reasoning-format", "deepseek", "--reasoning-effort", "medium",
            "--cont-batching", "--temperature", "0.0",
            "--presence-penalty", "0.0", "--repeat-penalty", "1.0"]
    path = f"/tmp/claude-1000/def_{tag}_{port}.log"
    log = open(path, "w")
    p = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT)
    for _ in range(1200):
        time.sleep(1)
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2) as r:
                if r.status == 200:
                    slot = None
                    for line in open(path):
                        if "n_ctx_slot" in line:
                            slot = int(line.split("n_ctx_slot = ")[1].split(",")[0])
                            break
                    return {"proc": p, "log": log, "path": path, "n_ctx_slot": slot}
        except Exception:
            pass
        if p.poll() is not None:
            raise RuntimeError(f"server {port} died, see {path}")
    raise RuntimeError("unhealthy")


def ask(srv, port, msgs, max_tokens):
    off = os.path.getsize(srv["path"])
    payload = json.dumps({"messages": msgs, "max_tokens": max_tokens,
                          "temperature": 0.0, "stream": False}).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}/v1/chat/completions",
                                 data=payload,
                                 headers={"Content-Type": "application/json"})
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=7200) as r:
        out = json.loads(r.read())
    wall = round(time.time() - t0, 1)
    with open(srv["path"]) as f:
        f.seek(off)
        accs = [float(x) for x in ACC_RE.findall(f.read())]
    t = out.get("timings", {})
    ch = out["choices"][0]
    msg = ch.get("message", {})
    return {"wall": wall,
            "prefilled": t.get("prompt_n"),
            "prefill_ms": round(t.get("prompt_ms", 0)),
            "prefill_tps": round(t.get("prompt_per_second", 0), 1),
            "decode_tps": round(t.get("predicted_per_second", 0), 2),
            "n_decode": t.get("predicted_n"),
            "acceptance": round(sum(accs) / len(accs), 3) if accs else None,
            "finish": ch.get("finish_reason"),
            "answer_chars": len(msg.get("content") or ""),
            "think_chars": len(msg.get("reasoning_content") or "")}


def stop(srvs):
    for s in srvs:
        s["proc"].terminate()
        try:
            s["proc"].wait(timeout=120)
        except subprocess.TimeoutExpired:
            s["proc"].kill()
        s["log"].close()
    time.sleep(12)


res = {"partA": {}, "partB": {}}
print(f"start [{guard()}] idle GTT {gtt()} MiB", flush=True)

# ---------------- PART A : acceptance vs (ctx, np) ----------------
SHORT = "Write a Rust function that parses a semver string. Explain briefly."
for ctx, np_ in ((16384, 1), (32768, 1), (49152, 1), (32768, 2), (49152, 3)):
    tag = f"A_c{ctx}_np{np_}"
    guard()
    srv = launch(PORT, np_, ctx, tag)
    r = ask(srv, PORT, [{"role": "user", "content": SHORT}], MAX_TOKENS_A)
    stop([srv])
    res["partA"][tag] = {"ctx": ctx, "np": np_, "n_ctx_slot": srv["n_ctx_slot"], **r}
    print(f"  {tag:18} n_ctx_slot={srv['n_ctx_slot']:6}  acceptance={r['acceptance']}"
          f"  decode={r['decode_tps']} t/s", flush=True)

# ---------------- PART B : 30k coding task ----------------
def convo(sess, turn):
    m = [{"role": "user",
          "content": f"Codebase for session S{sess}.\n\n{BODY_30K}\n\n{TASKS[sess]}"}]
    if turn == 2:
        m += [{"role": "assistant", "content": ASSIST},
              {"role": "user", "content": FOLLOWUP}]
    return m


def armB(name, n, batched, ctx_override=None, turns=(1, 2)):
    guard()
    print(f"\n### {name} [{guard()}]", flush=True)
    if batched:
        srvs = [launch(PORT, n, ctx_override or SLOT_CTX_B * n, name)]
        ports = [PORT] * n
        handles = [srvs[0]] * n
    else:
        srvs = [launch(PORT + 10 + i, 1, SLOT_CTX_B, f"{name}{i}") for i in range(n)]
        ports = [PORT + 10 + i for i in range(n)]
        handles = srvs
    g = gtt()
    out = {"n_ctx_slot": srvs[0]["n_ctx_slot"], "gtt_mib": g, "turns": []}
    for turn in turns:
        guard()
        t0 = time.time()
        with ThreadPoolExecutor(max_workers=n) as ex:
            fs = [ex.submit(ask, handles[i], ports[i], convo(i, turn), MAX_TOKENS_B)
                  for i in range(n)]
            rows = [f.result() for f in fs]
        wall = round(time.time() - t0, 1)
        out["turns"].append({"turn": turn, "wall_s": wall, "streams": rows})
        print(f"  turn{turn}: wall {wall}s | prefilled {[r['prefilled'] for r in rows]}"
              f" | prefill_ms {[r['prefill_ms'] for r in rows]}"
              f" | decode {[r['decode_tps'] for r in rows]}"
              f" | acc {[r['acceptance'] for r in rows]}"
              f" | finish {[r['finish'] for r in rows]}"
              f" | ans_chars {[r['answer_chars'] for r in rows]}", flush=True)
    stop(srvs)
    res["partB"][name] = out
    print(f"  n_ctx_slot={out['n_ctx_slot']} GTT {g} MiB", flush=True)


armB("solo_1stream", 1, True)
# np=1 at the exact totals the batched arms use: if acceptance is healthy here
# but zero at the same -c with more slots, the cause is -np, not -c.
armB("ctl_np1_c65536", 1, True, ctx_override=65536, turns=(1,))
armB("ctl_np1_c98304", 1, True, ctx_override=98304, turns=(1,))
armB("np2_one_server", 2, True)
armB("two_launches", 2, False)
armB("np3_one_server", 3, True)
armB("three_launches", 3, False)

json.dump(res, open("/tmp/claude-1000/definitive.json", "w"), indent=2)
print(f"\ndone [{guard()}] wrote /tmp/claude-1000/definitive.json")
