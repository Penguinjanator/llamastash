#!/usr/bin/env python3
"""Regenerate ``data/benchmark-snapshot.json`` from external sources.

This is the CI-loop owner of v2's recommender snapshot (R57). On a
successful run it produces a candidate snapshot, validates it against
the Rust recommender's 16/20 corpus check, and (when invoked under CI)
uploads the artefact to the rolling ``snapshot-latest`` GitHub Release.

The script runs in CI only — never as part of the cargo build. The
bundled ``data/benchmark-snapshot.json`` is committed to the source
tree; CI updates the *release asset* daily without auto-PR'ing a new
bundled snapshot. A maintainer-triggered PR refreshes the bundled copy
when prudent.

Partial-source-failure policy:
- If any source returns no data (timeout, parse error, upstream
  removal), the script does **not** publish — last-known-good stays
  live. ``doctor``'s ``RemoteSnapshotUnreachable`` finding surfaces
  prolonged outages through ``_init_snapshot.remote_fetch_failures``.
- The corpus gate (``cargo test --test recommender_corpus``) is
  release-blocking. A regressed snapshot exits non-zero so the CI
  workflow skips publication and auto-files a recalibration issue.

Vendored Python sources (Open LLM Leaderboard, Aider, etc.) live under
``scripts/benchmark_sources/`` and are documented in ``NOTICE``. The
sources are intentionally absent from the binary: the script runs in CI
to produce a JSON artefact the Rust binary reads (R45 single-binary
invariant).
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Tuple

REPO_ROOT = Path(__file__).resolve().parent.parent
SCHEMA_VERSION = 1
SNAPSHOT_PATH = REPO_ROOT / "data" / "benchmark-snapshot.json"


def _cargo_pkg_version() -> str:
    """Read the workspace's Cargo.toml package.version verbatim.

    ``min_version`` in the snapshot envelope must track Cargo.toml so
    every release binary accepts its own freshly-published snapshot
    asset. Hard-coding drifts the moment we cut a new release."""
    cargo_toml = REPO_ROOT / "Cargo.toml"
    in_package = False
    for line in cargo_toml.read_text().splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_package = stripped == "[package]"
            continue
        if in_package and stripped.startswith("version"):
            # version = "0.0.1"
            _, _, value = stripped.partition("=")
            return value.strip().strip("\"")
    raise SystemExit("failed to parse [package].version from Cargo.toml")


DEFAULT_MIN_VERSION = _cargo_pkg_version()
SOURCES_DIR = REPO_ROOT / "scripts" / "benchmark_sources"

# Make ``scripts/`` importable so the vendored adapters resolve under
# ``benchmark_sources.<name>``. The package itself ships under
# ``scripts/benchmark_sources/`` per R45 (CI-only, never in the binary).
sys.path.insert(0, str(REPO_ROOT / "scripts"))

from benchmark_sources import aa_index as _aa_index  # noqa: E402
from benchmark_sources import frontier as _frontier  # noqa: E402
from benchmark_sources import hf_discovery as _hf_discovery  # noqa: E402
from benchmark_sources import whichllm_combined as _whichllm_scores  # noqa: E402
from benchmark_sources.whichllm import SourceResult  # noqa: E402

# Maximum *unique source models* the bundled snapshot ships. Each
# source contributes up to 6 quant rows (Q3_K_M through Q8_0), so the
# row count is roughly 6× this. 250 was picked to land around ~1500
# rows / ~900 KiB — comfortably under Unit 6's 2 MiB ceiling — while
# being large enough that frontier releases with modest download
# counts (e.g. Qwen3-Next-80B-A3B-Instruct at ~350K downloads, which
# whichllm ranks #1) survive the budget pass.
SNAPSHOT_MODEL_LIMIT = 250

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Build the snapshot but do not write it or publish. Used "
        "by PRs touching data/benchmark-snapshot.json to validate "
        "the corpus gate before merge.",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=SNAPSHOT_PATH,
        help="Where to write the candidate snapshot.",
    )
    parser.add_argument(
        "--skip-corpus-gate",
        action="store_true",
        help=(
            "Skip the cargo test corpus gate. Intended for local "
            "debugging only — CI must run the gate."
        ),
    )
    args = parser.parse_args()

    _warn_if_likely_to_diverge_from_ci()
    sources = collect_sources()
    failed = [s for s in sources if not s.ok]
    if failed:
        for s in failed:
            print(f"[ERR] source `{s.name}` failed: {s.message}", file=sys.stderr)
        print(
            "[FAIL] partial source failure — refusing to publish; "
            "last-known-good snapshot stays live.",
            file=sys.stderr,
        )
        return 2

    candidate = build_snapshot(sources)

    if args.dry_run:
        print(json.dumps(candidate, indent=2))
    else:
        write_atomic(args.out, candidate)
        print(f"[OK] wrote {args.out}")

    if args.skip_corpus_gate:
        print("[WARN] corpus gate skipped (--skip-corpus-gate)", file=sys.stderr)
        return 0

    return run_corpus_gate()


def _warn_if_likely_to_diverge_from_ci() -> None:
    """Surface the most common reasons a local `make snapshot` run
    produces different ``source`` / ``score`` fields than the CI-
    published ``snapshot-latest`` asset.

    The regen pulls live data from HuggingFace + a basket of
    benchmark feeds; both move daily, so the bundled file in the repo
    is a frozen snapshot from whichever CI run last published. The
    big controllable knob is ``HF_TOKEN``: CI sets it from a
    repository secret so whichllm's HF queries don't trip the
    anonymous-tier rate limit (~5-7 queries per regen). Without it,
    whichllm may degrade gracefully — fewer rows surface, or the same
    row picks up a different score source on the next ranker pass —
    and that's exactly the divergence the snapshot-vs-CI complaint
    pattern matches.

    Print to stderr only; never gate the run. Users who want byte-
    stable regens can set HF_TOKEN locally too. CI hides the warning
    naturally because HF_TOKEN is always set there.
    """
    if os.environ.get("HF_TOKEN"):
        return
    print(
        "[WARN] HF_TOKEN not set — whichllm's HuggingFace queries will run\n"
        "       on the anonymous rate-limit tier. This is the most common\n"
        "       reason a local `make snapshot` produces different `source`\n"
        "       and `score` fields than the CI-published `snapshot-latest`\n"
        "       asset. Set HF_TOKEN to a read-only token to match CI.",
        file=sys.stderr,
    )


def collect_sources() -> List[SourceResult]:
    """Fetch every source. Each is independent so one upstream failure
    surfaces clearly rather than masquerading as a silent recommender
    regression."""
    results: List[SourceResult] = []
    results.append(load_hf_discovery())
    results.append(load_whichllm_combined_scores())
    # Future sources land here; each must return a SourceResult so the
    # partial-failure policy applies uniformly.
    return results


def load_hf_discovery() -> SourceResult:
    """Live catalog discovery via whichllm. Owns the row set the
    snapshot ships; ``whichllm-combined`` layers scores on top via
    ``source_hf_id`` joins."""
    return _hf_discovery.discover(REPO_ROOT, limit=SNAPSHOT_MODEL_LIMIT)


def load_whichllm_combined_scores() -> SourceResult:
    """Combined benchmark score lookup via
    ``whichllm.models.benchmark.fetch_benchmark_scores()``. Single
    adapter that wraps whichllm's six-source merge (Open LLM
    Leaderboard, Chatbot Arena, LiveBench, Artificial Analysis Index,
    Aider polyglot, Vision) plus the layered current-over-frozen
    precedence and lineage recency demotion. Replaces the per-source
    ``open_llm_leaderboard`` + ``aider`` adapters we used to vendor —
    they covered only 2/6 sources and lost the demotion logic, which
    is exactly why the wizard kept surfacing two-generation-old picks.
    """
    return _whichllm_scores.fetch()


def build_snapshot(sources: List[SourceResult]) -> Dict[str, Any]:
    """Merge live discovery + combined benchmark scores into a fresh
    ``models[]``.

    The HF-discovery source (Unit 3 of plan 2026-05-20-001) is the
    catalog *owner*: it produces the row set, including
    ``source_hf_id``, ``repo``, ``file``, ``params``, ``params_active``,
    ``is_moe``, ``weights_bytes``, ``task_hints``, and ``gguf_publisher``.
    The ``whichllm-combined`` source supplies ``benchmark_score.value``
    keyed by lowercased ``source_hf_id``; we join on that field.

    Policy:

    * Catalog rows come from live HF discovery and are capped at
      :data:`SNAPSHOT_MODEL_LIMIT`.
    * For each row, attach the combined score whichllm reports
      (already merged across OLLB / Arena / LiveBench / AA Index /
      Aider / Vision with current-over-frozen precedence and lineage
      recency demotion). Rows whichllm doesn't cover ship with
      ``score=0`` and source ``no-source`` so the recommender ranks
      them by params / speed / recency rather than tying them with a
      misleading constant floor.
    * ``recommender_weights`` (including ``overhead_band_bytes``) is
      preserved verbatim from the previous bundled snapshot — it's
      owned by a separate plan.
    """
    bundled_recommender_weights: Dict[str, Any] = {}
    bundled_remote_url: Optional[str] = None
    if SNAPSHOT_PATH.exists():
        try:
            with SNAPSHOT_PATH.open() as f:
                bundled = json.load(f)
        except (OSError, ValueError) as e:
            # A corrupt or unreadable bundled snapshot is fatal — the
            # script's promise includes preserving recommender_weights,
            # and we have no source to preserve from. Surface via the
            # exit-2 path rather than crashing with a raw traceback.
            print(
                f"[FAIL] bundled snapshot at {SNAPSHOT_PATH} unreadable: {e}",
                file=sys.stderr,
            )
            raise SystemExit(2) from e
        bundled_recommender_weights = bundled.get("recommender_weights", {})
        bundled_remote_url = bundled.get("remote_url")

    catalog_rows = _extract_catalog_rows(sources)
    scores_by_adapter = _index_adapter_scores(sources)
    lookup = _score_lookup(scores_by_adapter.get("whichllm-combined", {}), _aa_overrides())
    models = [_compose_model_entry(row, lookup) for row in catalog_rows]

    candidate: Dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "bundle_date": datetime.date.today().isoformat(),
        "min_version": DEFAULT_MIN_VERSION,
        "remote_url": bundled_remote_url,
        "recommender_weights": bundled_recommender_weights,
        "models": models,
        "regen_environment": _regen_environment(),
    }
    return candidate


def _regen_environment() -> Dict[str, Any]:
    """Reproducibility manifest. Captured in the snapshot envelope so a
    `diff` between a local regen and the CI-published asset surfaces
    the most common explanations (whichllm version drift, missing
    HF_TOKEN, Python version skew) without having to instrument the
    user's machine. Forward-compatible: the Rust loader ignores
    unknown top-level keys, so adding the manifest doesn't bump
    `schema_version`."""
    try:
        from importlib.metadata import version as _pkg_version
        whichllm_version = _pkg_version("whichllm")
    except Exception:  # noqa: BLE001 — best-effort manifest
        whichllm_version = None
    return {
        "python_version": (
            f"{sys.version_info.major}.{sys.version_info.minor}"
            f".{sys.version_info.micro}"
        ),
        "whichllm_version": whichllm_version,
        "hf_token_present": bool(os.environ.get("HF_TOKEN")),
        "ci": bool(os.environ.get("CI")),
    }


def _extract_catalog_rows(sources: List[SourceResult]) -> List[Dict[str, Any]]:
    """Pull the hf-discovery source's rows. Other sources are
    score-only adapters and don't contribute catalog identity."""
    for src in sources:
        if src.name == "hf-discovery" and src.ok:
            return list(src.rows)
    return []


def _compose_model_entry(
    row: Dict[str, Any],
    lookup: Callable[[str, int], Tuple[float, str]],
) -> Dict[str, Any]:
    """Build one ``models[]`` entry. The hf-discovery source supplies
    everything except ``benchmark_score``, ``tok_s_factor``, and
    ``recency`` — those are derived here."""
    source_hf_id = row.get("source_hf_id") or ""
    quant = row.get("quant") or "Q4_K_M"
    file_basename = row.get("file") or ""
    slug = _slug(source_hf_id, quant)
    family_score, score_source = lookup(source_hf_id, int(row.get("params") or 0))
    # Apply per-quant quality discount so Q8_0 outranks Q4_K_M of the
    # same model when both fit — matches inference reality where Q3
    # is ~5% off the family score and Q8 is essentially lossless.
    quality_mult = _hf_discovery._QUANT_QUALITY_MULT.get(quant.upper(), 1.0)
    score_value = round(family_score * quality_mult, 2)
    params_total = int(row.get("params") or 0)
    params_active = row.get("params_active")
    is_moe = bool(row.get("is_moe"))
    # Speed scales with the params actually used per token, not the
    # full weights footprint. For MoE models with a declared active-
    # param count, key the tok/s curve off that — otherwise a 30B-A3B
    # gets the same speed factor as a dense 30B and the recommender
    # never surfaces sparse models.
    params_for_speed = (
        int(params_active)
        if is_moe and params_active
        else params_total
    )
    # Within a family, smaller quants are faster (less memory
    # bandwidth per token). Scale the params-based baseline by the
    # quant multiplier so the recommender sees Q3 > Q4 > Q5 > Q8 in
    # tok_s ordering, mirroring whichllm's bandwidth-derived speed.
    speed_mult = _hf_discovery._QUANT_SPEED_MULT.get(quant.upper(), 1.0)
    tok_s_factor = round(_tok_s_factor_for_params(params_for_speed) * speed_mult, 3)
    return {
        "id": slug,
        "repo": row.get("repo") or "",
        "file": file_basename,
        "architecture": row.get("architecture") or "unknown",
        "quant": quant,
        "params": params_total,
        "weights_bytes": int(row.get("weights_bytes") or 0),
        "task_hints": list(row.get("task_hints") or []),
        "benchmark_score": {"value": score_value, "source": score_source},
        "tok_s_factor": tok_s_factor,
        "recency": _recency_for_last_modified(row.get("last_modified") or ""),
        "source_hf_id": source_hf_id,
        "params_active": params_active,
        "is_moe": is_moe,
        "gguf_publisher": row.get("gguf_publisher") or "",
    }


def _slug(source_hf_id: str, quant: str) -> str:
    """Stable, hyphen-delimited id derived from HF id + quant."""
    base = source_hf_id.lower().replace("/", "-")
    return f"{base}-{quant.lower()}"


def _aa_overrides() -> Dict[str, float]:
    """``{lowercased source id: AA score}`` for curated models with an
    ``aa:`` name in ``data/frontier-models.yaml``."""
    scores = _aa_index.load_scores(REPO_ROOT, _aa_index.fetch_html(_aa_index.AA_LIVE_URL))
    out: Dict[str, float] = {}
    for entry in _frontier.load_frontier_list(REPO_ROOT):
        name = entry.get("aa")
        if not name:
            continue
        score = scores.get(_aa_index.canonical(str(name)))
        if score is None:
            print(f"[WARN] aa-index: {entry['id']}: no AA score for {name!r}", file=sys.stderr)
        else:
            out[entry["id"].lower()] = score
    return out


def _score_lookup(
    combined: Dict[str, float],
    aa: Dict[str, float],
) -> Callable[[str, int], Tuple[float, str]]:
    """Return ``(source_hf_id, params) -> (score, source)`` over whichllm's
    combined, case-insensitive index.

    A curated model's AA score (``aa``, keyed by lowercased id) wins, with
    source ``aa-index``: whichllm scales AA's current index with bounds fit to
    an older one. Otherwise a direct whichllm hit is source ``whichllm``.
    Otherwise whichllm's own fallback
    (``lookup_benchmark_evidence``: an `-Instruct` / name variant, then a
    size-interpolated score from the same model line) applies, discounted
    by its confidence the way whichllm's ranker does, with source
    ``whichllm-<kind>``. Rows with no evidence get 0.0 and ``no-source``:
    they still ship and rank by params / speed / recency, without a
    constant floor that would tie them with real scored models.
    """
    from whichllm.models.benchmark import (  # type: ignore[import-not-found]
        build_line_bucket_index,
        build_score_index,
        lookup_benchmark_evidence,
    )

    ci_index, line_index = build_score_index(combined)
    buckets = build_line_bucket_index(combined)

    def lookup(source_hf_id: str, params: int) -> Tuple[float, str]:
        if source_hf_id.lower() in aa:
            return aa[source_hf_id.lower()], "aa-index"
        direct = combined.get(source_hf_id.lower())
        if direct is not None:
            return direct, "whichllm"
        evidence = lookup_benchmark_evidence(
            source_hf_id,
            None,
            combined,
            ci_index=ci_index,
            line_index=line_index,
            line_bucket_index=buckets,
            actual_params_b=params / 1e9 if params else None,
        )
        if evidence.score is None:
            return 0.0, "no-source"
        conf = max(0.0, min(1.0, evidence.confidence))
        return round(evidence.score * (0.75 + 0.25 * conf), 2), f"whichllm-{evidence.source}"

    return lookup


def _tok_s_factor_for_params(params: int) -> float:
    """Coarse tok/s proxy by parameter count. Mirrors the bundled
    snapshot's per-row factor so the recommender's speed term stays
    well-behaved without per-arch benchmarking inside CI."""
    if params <= 0:
        return 1.0
    if params <= 2_000_000_000:
        return 1.6
    if params <= 5_000_000_000:
        return 1.3
    if params <= 9_000_000_000:
        return 1.0
    if params <= 16_000_000_000:
        return 0.6
    if params <= 40_000_000_000:
        return 0.4
    return 0.2


def _recency_for_last_modified(last_modified: str) -> float:
    """Decay multiplier based on HF ``last_modified``. Recent (≤180 d)
    keeps full 1.0; older models decay to a 0.7 floor."""
    if not last_modified:
        return 0.9
    try:
        modified = datetime.date.fromisoformat(last_modified[:10])
    except ValueError:
        return 0.9
    days = (datetime.date.today() - modified).days
    if days <= 180:
        return 1.0
    if days <= 365:
        return 0.9
    if days <= 730:
        return 0.8
    return 0.7


def _index_adapter_scores(
    sources: List[SourceResult],
) -> Dict[str, Dict[str, float]]:
    """Index successful adapter results as ``adapter_name -> {hf_id: score}``."""
    by_source: Dict[str, Dict[str, float]] = {}
    for src in sources:
        if not src.ok:
            continue
        scores: Dict[str, float] = {}
        for row in src.rows:
            hf_id = row.get("hf_id")
            score = row.get("score")
            # Exclude bool explicitly — it's a subclass of int and would
            # otherwise sneak past the isinstance guard.
            if (
                not isinstance(hf_id, str)
                or isinstance(score, bool)
                or not isinstance(score, (int, float))
            ):
                continue
            scores[hf_id] = float(score)
        by_source[src.name] = scores
    return by_source


def write_atomic(path: Path, body: Dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + f".tmp.{os.getpid()}")
    with tmp.open("w") as f:
        json.dump(body, f, indent=2)
        f.write("\n")
    os.replace(tmp, path)


def run_corpus_gate() -> int:
    """Invoke ``cargo test`` against the recommender corpus integration
    test. Non-zero exit blocks publication. CI's workflow auto-files a
    recalibration issue on regression."""
    cargo = shutil.which("cargo")
    if cargo is None:
        print("[WARN] cargo not on $PATH; skipping corpus gate", file=sys.stderr)
        return 0
    cmd = [
        cargo,
        "test",
        "--features",
        "test-fixtures",
        "--test",
        "recommender_corpus",
        "--",
        "--nocapture",
    ]
    print(f"[gate] {' '.join(cmd)}", flush=True)
    result = subprocess.run(cmd, cwd=REPO_ROOT)
    if result.returncode == 0:
        print("[gate] PASS")
        return 0
    print(
        "[gate] FAIL — corpus regressed; not publishing snapshot. "
        "CI workflow will open an issue with the recommender-regression label.",
        file=sys.stderr,
    )
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
