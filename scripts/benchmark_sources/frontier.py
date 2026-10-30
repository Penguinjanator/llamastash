"""Curated frontier models, fetched by repo id from HuggingFace.

whichllm's discovery only lists ``text-generation`` repos plus its own
frontier list, so vision-tagged chat models and fresh releases never reach
the snapshot. ``data/frontier-models.yaml`` names them; this module reads
each source repo for params, then the trusted GGUF repos' file trees for the
real quant files and sizes. Rows come out shaped like ``hf_discovery``'s.

CI-only (R45): none of this runs in the Rust binary.
"""

from __future__ import annotations

import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Sequence, Tuple

try:
    import yaml  # type: ignore[import-not-found]
except ImportError:  # pragma: no cover — PyYAML is a regen-time CI dep
    yaml = None  # type: ignore[assignment]

HF_API = os.environ.get("HF_ENDPOINT", "https://huggingface.co").rstrip("/") + "/api"

# Probe order when an entry has no `gguf:` list. `{org}` / `{name}` come from
# the source id; the model's own org is appended last.
DEFAULT_GGUF_REPOS: Tuple[str, ...] = (
    "unsloth/{name}-GGUF",
    "bartowski/{org}_{name}-GGUF",
    "bartowski/{name}-GGUF",
    "ggml-org/{name}-GGUF",
    "lmstudio-community/{name}-GGUF",
    "mradermacher/{name}-GGUF",
)

# Files in GGUF repos that are not the model weights.
_NON_WEIGHT_FILE = re.compile(
    r"mmproj|imatrix|(^|[-_/])(mtp|dflash|dspark|draft|eagle)[-_]", re.I
)
_SHARD = re.compile(r"-\d{5}-of-\d{5}$", re.I)
_MOE_SUFFIX = re.compile(r"-A(\d+(?:\.\d+)?)B(?:$|-)", re.I)

Fetch = Callable[[str], Any]


def load_frontier_list(repo_root: Path) -> List[Dict[str, Any]]:
    """Parse ``data/frontier-models.yaml``."""
    if yaml is None:
        raise RuntimeError("PyYAML must be installed; see scripts/requirements.txt")
    with (repo_root / "data" / "frontier-models.yaml").open() as f:
        body = yaml.safe_load(f) or {}
    return list(body.get("models") or [])


def quant_label(path: str, ladder: Sequence[str]) -> Optional[Tuple[str, bool]]:
    """Return ``(label, is_unsloth_dynamic)`` for a GGUF weights file, or
    ``None`` when it is not a weights file or not on the ``ladder``."""
    base = path.rsplit("/", 1)[-1]
    if not base.lower().endswith(".gguf") or _NON_WEIGHT_FILE.search(base):
        return None
    stem = _SHARD.sub("", base[: -len(".gguf")]).upper()
    # Longest labels first so Q2_K_XL is not read as Q2_K. The lookahead
    # leaves out `_`, so Q2_K_L / Q6_K_XL never match Q2_K / Q6_K.
    for label in sorted(ladder, key=len, reverse=True):
        forms = (label, "MXFP4_MOE") if label == "MXFP4" else (label,)
        for form in forms:
            m = re.search(rf"(?:^|[-._])(UD-)?{re.escape(form)}(?=$|[-.])", stem)
            if m:
                return label, bool(m.group(1))
    return None


def collect_quants(
    tree: Sequence[Dict[str, Any]], ladder: Sequence[str]
) -> Dict[str, Tuple[str, int]]:
    """Map each ladder quant in a repo tree to ``(first_shard_path,
    summed_bytes)``. Plain files win over unsloth `UD-` files of the same
    label."""
    groups: Dict[Tuple[str, bool], List[Tuple[str, int]]] = {}
    for item in tree:
        if item.get("type") != "file":
            continue
        path = item.get("path") or ""
        hit = quant_label(path, ladder)
        if hit is None:
            continue
        size = int((item.get("lfs") or {}).get("size") or item.get("size") or 0)
        groups.setdefault(hit, []).append((path, size))
    out: Dict[str, Tuple[str, int]] = {}
    for (label, dynamic), files in sorted(groups.items(), key=lambda kv: kv[0][1]):
        if label in out:
            continue
        # A quant can ship twice under one label (e.g. root and a subfolder);
        # group by stem so only one copy's shards are summed.
        by_stem: Dict[str, List[Tuple[str, int]]] = {}
        for path, size in files:
            by_stem.setdefault(_SHARD.sub("", path[: -len(".gguf")]), []).append((path, size))
        _, chosen = sorted(by_stem.items())[0]
        chosen.sort()
        out[label] = (chosen[0][0], sum(s for _, s in chosen))
    return out


def active_params(entry: Dict[str, Any], source_id: str) -> Optional[int]:
    if entry.get("active") is not None:
        return int(float(entry["active"]) * 1e9)
    m = _MOE_SUFFIX.search(source_id.rsplit("/", 1)[-1])
    return int(float(m.group(1)) * 1e9) if m else None


def _base_models(info: Dict[str, Any]) -> List[str]:
    base = (info.get("cardData") or {}).get("base_model")
    if isinstance(base, str):
        return [base]
    return [b for b in base or [] if isinstance(b, str)]


def _quantizes(info: Dict[str, Any], source_id: str) -> bool:
    """A GGUF repo counts when it declares no base model, or one with the
    same model name (unsloth often points at its own mirror)."""
    name = source_id.rsplit("/", 1)[-1].lower()
    bases = _base_models(info)
    return not bases or any(b.rsplit("/", 1)[-1].lower() == name for b in bases)


def fetch_frontier_rows(
    entries: Sequence[Dict[str, Any]],
    *,
    ladder: Sequence[str],
    low_quants: Sequence[str],
    low_quant_min_params: int,
    trusted: Callable[[str, str], bool],
    task_hints: Callable[[str], List[str]],
    fetch: Optional[Fetch] = None,
) -> List[Dict[str, Any]]:
    """One row per (curated model, ladder quant) a trusted repo ships.

    A model whose source repo or GGUF repos can't be read is skipped with a
    warning; the caller decides what an empty result means.
    """
    get = fetch or _http_get_json
    rows: List[Dict[str, Any]] = []
    for entry in entries:
        source_id = entry["id"]
        try:
            rows.extend(
                _rows_for(entry, source_id, get, ladder, low_quants,
                          low_quant_min_params, trusted, task_hints)
            )
        except Exception as exc:  # noqa: BLE001 — one model must not sink the list
            print(f"[WARN] frontier: {source_id}: {type(exc).__name__}: {exc}", file=sys.stderr)
    return rows


def _rows_for(
    entry: Dict[str, Any],
    source_id: str,
    get: Fetch,
    ladder: Sequence[str],
    low_quants: Sequence[str],
    low_quant_min_params: int,
    trusted: Callable[[str, str], bool],
    task_hints: Callable[[str], List[str]],
) -> List[Dict[str, Any]]:
    info = get(f"models/{source_id}?expand[]=safetensors&expand[]=config"
               "&expand[]=downloads&expand[]=createdAt")
    if info is None:
        print(f"[WARN] frontier: {source_id}: repo not found", file=sys.stderr)
        return []
    source_id = info.get("id") or source_id
    params = int((info.get("safetensors") or {}).get("total") or 0)
    if params <= 0:
        print(f"[WARN] frontier: {source_id}: no safetensors param count", file=sys.stderr)
        return []
    active = active_params(entry, source_id)
    allowed = [q for q in ladder if params >= low_quant_min_params or q not in low_quants]

    org, _, name = source_id.partition("/")
    templates = list(entry.get("gguf") or [t.format(org=org, name=name) for t in DEFAULT_GGUF_REPOS])
    templates.append(f"{org}/{name}-GGUF")

    chosen: Dict[str, Tuple[str, str, int]] = {}
    for repo in dict.fromkeys(templates):
        publisher = repo.split("/", 1)[0]
        if not trusted(publisher, source_id):
            continue
        repo_info = get(f"models/{repo}")
        if repo_info is None or not _quantizes(repo_info, source_id):
            continue
        repo = repo_info.get("id") or repo
        tree = get(f"models/{repo}/tree/main?recursive=true") or []
        for label, (path, size) in collect_quants(tree, allowed).items():
            if label not in chosen and size > 0:
                chosen[label] = (repo, path, size)
        if len(chosen) == len(allowed):
            break

    if not chosen:
        print(f"[WARN] frontier: {source_id}: no trusted GGUF quant found", file=sys.stderr)
        return []
    arch = (info.get("config") or {}).get("model_type") or "unknown"
    hints = task_hints(source_id)
    return [
        {
            "source_hf_id": source_id,
            "repo": repo,
            "file": path,
            "architecture": arch,
            "quant": label,
            "params": params,
            "weights_bytes": size,
            "task_hints": list(hints),
            "is_moe": active is not None,
            "params_active": active,
            "gguf_publisher": repo.split("/", 1)[0],
            "downloads": int(info.get("downloads") or 0),
            "last_modified": str(info.get("createdAt") or ""),
            "curated": True,
        }
        for label in allowed
        if label in chosen
        for repo, path, size in [chosen[label]]
    ]


def _http_get_json(path: str) -> Any:
    """GET ``{HF_API}/{path}``. ``None`` on 404/401; one retry after a 429."""
    headers = {"User-Agent": "llamastash-snapshot-regen"}
    token = os.environ.get("HF_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    url = f"{HF_API}/{urllib.parse.quote(path, safe='/?=&[]')}"
    for attempt in range(2):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=60) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code in (401, 404):
                return None
            if e.code == 429 and attempt == 0:
                time.sleep(float(e.headers.get("Retry-After") or 60))
                continue
            raise
    return None
