"""Artificial Analysis Intelligence Index scores for the curated models.

whichllm reads AA's live leaderboard but normalizes it with bounds fit to an
older index, and AA has reworked the index several times since (Qwen3.8 27B
reads 52.0 on 2026-09-02, 33.7 on 2026-10-16, 20.1 on 2026-10-30). The live
page now scores only ~80 current models; older ones read null.

So the reference is a pinned capture, ``data/aa-index.json`` (2026-10-16, 694
models). The live page fills names the capture lacks, mapped onto the
capture's scale by a least-squares fit over the names both share.

CI-only (R45). Refresh the capture with::

    python3 scripts/benchmark_sources/aa_index.py <leaderboard-url> > data/aa-index.json
"""

from __future__ import annotations

import gzip
import json
import math
import re
import sys
import urllib.request
from pathlib import Path
from typing import Dict, Optional, Tuple

AA_LIVE_URL = "https://artificialanalysis.ai/leaderboards/models"

# Capture scale -> whichllm 0-100. Least-squares fit of the 2026-10-16 capture
# against the 2026-09-02 one (485 shared names, r^2 0.95), then whichllm
# 0.5.23's AA bounds (-19.4..47.6), so these scores sit on the same scale as
# whichllm's for the rest of the catalog.
_SCALE = 2.571
_OFFSET = 23.32

_MIN_SHARED = 20
_MIN_R2 = 0.8

_RSC_CHUNK = re.compile(r'self\.__next_f\.push\(\[\d+,("(?:[^"\\]|\\.)*")\]\)')
# A record's "name" followed by its "intelligenceIndex" with no other "name"
# in between, so a record without an index can't borrow its neighbour's.
_RECORD = re.compile(
    r'"name":"((?:[^"\\]|\\.)*)"(?:(?!"name":").)*?"intelligenceIndex":(-?\d+(?:\.\d+)?)',
    re.DOTALL,
)
_PAREN = re.compile(r"\([^)]*\)")


def canonical(name: str) -> str:
    """``"Qwen3.8 27B (Xhigh)"`` and ``"qwen3.8-27b"`` -> ``"qwen3.8 27b"``."""
    name = _PAREN.sub("", name).lower().replace("-", " ").replace("_", " ")
    return re.sub(r"\s+", " ", name).strip()


def parse_leaderboard(html: str) -> Dict[str, float]:
    """``{canonical name: best raw index}`` from the leaderboard page. Effort
    and reasoning variants collapse to one name; the best one wins."""
    blob = "".join(json.loads(m.group(1)) for m in _RSC_CHUNK.finditer(html))
    out: Dict[str, float] = {}
    for m in _RECORD.finditer(blob):
        name = canonical(json.loads(f'"{m.group(1)}"'))
        raw = float(m.group(2))
        if name and math.isfinite(raw) and raw > out.get(name, -math.inf):
            out[name] = raw
    return out


def fit(src: Dict[str, float], ref: Dict[str, float]) -> Optional[Tuple[float, float]]:
    """Least-squares ``ref ~ k*src + c`` over shared names, or ``None`` when
    too few names are shared or the fit is poor."""
    shared = [n for n in src if n in ref]
    if len(shared) < _MIN_SHARED:
        return None
    xs = [src[n] for n in shared]
    ys = [ref[n] for n in shared]
    mx, my = sum(xs) / len(xs), sum(ys) / len(ys)
    sxx = sum((x - mx) ** 2 for x in xs)
    syy = sum((y - my) ** 2 for y in ys)
    sxy = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    if sxx == 0 or syy == 0 or sxy * sxy / (sxx * syy) < _MIN_R2:
        return None
    k = sxy / sxx
    return k, my - k * mx


def normalize(raw: float) -> float:
    """Capture-scale raw index -> whichllm's 0-100 scale."""
    return max(0.0, min(100.0, round(_SCALE * raw + _OFFSET, 1)))


def load_scores(repo_root: Path, live_html: Optional[str]) -> Dict[str, float]:
    """``{canonical name: 0-100 score}``: the capture, plus live-only names
    mapped onto the capture's scale. A live page that can't be fit is
    skipped with a warning."""
    ref = json.loads((repo_root / "data" / "aa-index.json").read_text())["scores"]
    merged = dict(ref)
    if live_html:
        live = parse_leaderboard(live_html)
        line = fit(live, ref)
        if line is None:
            print(f"[WARN] aa-index: live page ({len(live)} names) does not fit the capture; "
                  "using the capture only", file=sys.stderr)
        else:
            k, c = line
            for name, raw in live.items():
                merged.setdefault(name, k * raw + c)
    return {name: normalize(raw) for name, raw in merged.items()}


def fetch_html(url: str) -> Optional[str]:
    """GET a leaderboard page; ``None`` (with a warning) on any failure."""
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0", "Accept-Encoding": "gzip"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            body = r.read()
    except Exception as exc:  # noqa: BLE001 — the live page is optional
        print(f"[WARN] aa-index: {url}: {type(exc).__name__}: {exc}", file=sys.stderr)
        return None
    if body[:2] == b"\x1f\x8b":
        body = gzip.decompress(body)
    return body.decode("utf-8", "replace")


if __name__ == "__main__":
    url = sys.argv[1]
    html = fetch_html(url)
    if not html:
        raise SystemExit(1)
    scores = parse_leaderboard(html)
    if not scores:
        raise SystemExit("no scores parsed")
    json.dump({"source": url, "scores": {k: round(v, 2) for k, v in sorted(scores.items())}}, sys.stdout, indent=1)
    print()
