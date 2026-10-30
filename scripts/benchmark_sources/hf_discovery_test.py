"""Unit tests for hf_discovery's projection / selection logic.

The tests stub out the whichllm import so they run on any Python 3.11+
without the pip dep. End-to-end discovery (whichllm + HF API) is
exercised by the daily CI regen run, not these tests.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from typing import Any, Dict, List, Optional

# Make ``scripts/`` importable so ``benchmark_sources.*`` resolves.
SCRIPTS_DIR = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(SCRIPTS_DIR))

from benchmark_sources import aa_index as _aa_index  # noqa: E402
from benchmark_sources import frontier as _frontier  # noqa: E402
from benchmark_sources import hf_discovery  # noqa: E402


REPO_ROOT = SCRIPTS_DIR.parent


def _fake_candidate(
    repo_id: str,
    parameter_count: int,
    gguf_variants: List[Dict[str, Any]],
    *,
    architecture: str = "qwen3",
    is_moe: bool = False,
    parameter_count_active: Optional[int] = None,
    base_model: Optional[str] = None,
    downloads: int = 1000,
    published_at: str = "2026-04-01",
) -> Dict[str, Any]:
    """Build a dict-shaped stand-in for whichllm's ModelInfo.

    Field names match whichllm 0.5.23's ModelInfo dataclass exactly so
    the test exercises hf_discovery's real consumption path."""
    return {
        "id": repo_id,
        "parameter_count": parameter_count,
        "parameter_count_active": parameter_count_active,
        "architecture": architecture,
        "is_moe": is_moe,
        "base_model": base_model,
        "downloads": downloads,
        "published_at": published_at,
        "gguf_variants": gguf_variants,
    }


def _variant(filename: str, quant_type: str, size: int = 0) -> Dict[str, Any]:
    return {
        "filename": filename,
        "quant_type": quant_type,
        "file_size_bytes": size,
    }


class AttachTaskHintsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.prefixes = {
            "Qwen/Qwen3-Coder": ["code"],
            "Qwen/Qwen3": ["general", "reasoning"],
        }
        self.defaults = ["general"]

    def test_longest_prefix_wins(self) -> None:
        # Qwen3-Coder must beat the shorter Qwen3 prefix.
        result = hf_discovery.attach_task_hints(
            "Qwen/Qwen3-Coder-30B-A3B-Instruct", self.prefixes, self.defaults
        )
        self.assertEqual(result, ["code"])

    def test_fallback_to_defaults_on_no_match(self) -> None:
        result = hf_discovery.attach_task_hints(
            "nonexistent/Model-9B", self.prefixes, self.defaults
        )
        self.assertEqual(result, ["general"])


class CollectVariantsTest(unittest.TestCase):
    def test_emits_one_row_per_preferred_quant_in_preferred_order(self) -> None:
        variants = [
            _variant("foo-Q5_K_M.gguf", "Q5_K_M", 6_500_000_000),
            _variant("foo-Q4_K_M.gguf", "Q4_K_M", 5_400_000_000),
            _variant("foo-Q8_0.gguf", "Q8_0", 8_000_000_000),
        ]
        result = hf_discovery._collect_variants(variants)
        quants = [q for q, _, _ in result]
        # Preferred order is (Q3_K_M, Q4_K_S, Q4_K_M, Q5_K_M, Q6_K, Q8_0);
        # input has Q4_K_M / Q5_K_M / Q8_0.
        self.assertEqual(quants, ["Q4_K_M", "Q5_K_M", "Q8_0"])
        self.assertEqual(result[0], ("Q4_K_M", "foo-Q4_K_M.gguf", 5_400_000_000))

    def test_skips_non_preferred_quants(self) -> None:
        variants = [
            _variant("bar-IQ2_M.gguf", "IQ2_M", 3_000_000_000),
            _variant("bar-Q5_K_M.gguf", "Q5_K_M", 6_500_000_000),
        ]
        result = hf_discovery._collect_variants(variants)
        self.assertEqual([q for q, _, _ in result], ["Q5_K_M"])

    def test_returns_empty_when_no_preferred_quant(self) -> None:
        variants = [_variant("baz-IQ2_M.gguf", "IQ2_M", 3_000_000_000)]
        self.assertEqual(hf_discovery._collect_variants(variants), [])

    def test_quant_lookup_is_case_insensitive(self) -> None:
        variants = [_variant("baz-q4_k_m.gguf", "q4_k_m", 5_000_000_000)]
        result = hf_discovery._collect_variants(variants)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0][0], "Q4_K_M")


class PublisherTrustedTest(unittest.TestCase):
    def test_allowlisted_publisher_passes(self) -> None:
        self.assertTrue(
            hf_discovery._publisher_trusted(
                publisher="bartowski",
                source_hf_id="meta-llama/Llama-4-9B",
                allowlist=["bartowski", "unsloth"],
            )
        )

    def test_official_org_passes_even_without_allowlist(self) -> None:
        # First-party GGUF from the model family's own org always
        # qualifies, even if that org isn't explicitly allowlisted.
        self.assertTrue(
            hf_discovery._publisher_trusted(
                publisher="Qwen",
                source_hf_id="Qwen/Qwen3-Coder-30B-A3B-Instruct",
                allowlist=["bartowski"],
            )
        )

    def test_unknown_publisher_rejected(self) -> None:
        self.assertFalse(
            hf_discovery._publisher_trusted(
                publisher="shady-org",
                source_hf_id="meta-llama/Llama-4-9B",
                allowlist=["bartowski"],
            )
        )


class ProjectCandidateTest(unittest.TestCase):
    def test_official_qwen_moe_candidate_carries_params_active(self) -> None:
        cand = _fake_candidate(
            repo_id="Qwen/Qwen3-Next-80B-A3B-Instruct-GGUF",
            base_model="Qwen/Qwen3-Next-80B-A3B-Instruct",
            parameter_count=80_000_000_000,
            parameter_count_active=3_000_000_000,
            is_moe=True,
            gguf_variants=[
                _variant("qwen3-next-80b-Q4_K_M.gguf", "Q4_K_M", 48_000_000_000)
            ],
        )
        prefixes = {"Qwen/Qwen3-Next": ["general", "reasoning"]}
        defaults = ["general"]
        rows = hf_discovery._project_candidate(cand, ["bartowski"], prefixes, defaults)
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertTrue(row.is_moe)
        self.assertEqual(row.params_active, 3_000_000_000)
        self.assertEqual(row.params, 80_000_000_000)
        self.assertEqual(row.gguf_publisher, "Qwen")
        self.assertEqual(row.source_hf_id, "Qwen/Qwen3-Next-80B-A3B-Instruct")
        self.assertEqual(row.quant, "Q4_K_M")
        self.assertEqual(row.weights_bytes, 48_000_000_000)
        self.assertEqual(row.task_hints, ["general", "reasoning"])

    def test_emits_one_row_per_available_preferred_quant(self) -> None:
        cand = _fake_candidate(
            repo_id="MaziyarPanahi/Qwen3-30B-A3B-GGUF",
            base_model="Qwen/Qwen3-30B-A3B",
            parameter_count=30_000_000_000,
            parameter_count_active=3_000_000_000,
            is_moe=True,
            gguf_variants=[
                _variant("qwen3-30b-Q4_K_M.gguf", "Q4_K_M", 18_000_000_000),
                _variant("qwen3-30b-Q6_K.gguf", "Q6_K", 24_000_000_000),
                _variant("qwen3-30b-Q8_0.gguf", "Q8_0", 31_000_000_000),
            ],
        )
        rows = hf_discovery._project_candidate(
            cand, ["MaziyarPanahi"], {}, ["general"]
        )
        quants = sorted(r.quant for r in rows)
        self.assertEqual(quants, ["Q4_K_M", "Q6_K", "Q8_0"])
        # Every row carries the same source_hf_id / params / repo,
        # differentiated only by quant + weights_bytes + file.
        ids = {r.source_hf_id for r in rows}
        self.assertEqual(ids, {"Qwen/Qwen3-30B-A3B"})
        weights = {r.quant: r.weights_bytes for r in rows}
        self.assertEqual(weights["Q4_K_M"], 18_000_000_000)
        self.assertEqual(weights["Q6_K"], 24_000_000_000)
        self.assertEqual(weights["Q8_0"], 31_000_000_000)

    def test_bartowski_quant_of_external_source_passes(self) -> None:
        cand = _fake_candidate(
            repo_id="bartowski/Llama-4-9B-Instruct-GGUF",
            base_model="meta-llama/Llama-4-9B-Instruct",
            parameter_count=9_000_000_000,
            gguf_variants=[
                _variant("llama-4-9b-Q4_K_M.gguf", "Q4_K_M", 5_400_000_000)
            ],
        )
        prefixes = {"meta-llama/Llama-4": ["general", "reasoning"]}
        defaults = ["general"]
        rows = hf_discovery._project_candidate(cand, ["bartowski"], prefixes, defaults)
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertEqual(row.gguf_publisher, "bartowski")
        self.assertEqual(row.source_hf_id, "meta-llama/Llama-4-9B-Instruct")
        self.assertEqual(row.task_hints, ["general", "reasoning"])

    def test_untrusted_publisher_dropped(self) -> None:
        cand = _fake_candidate(
            repo_id="shady-org/Llama-4-9B-GGUF",
            base_model="meta-llama/Llama-4-9B",
            parameter_count=9_000_000_000,
            gguf_variants=[
                _variant("llama-4-9b-Q4_K_M.gguf", "Q4_K_M", 5_400_000_000)
            ],
        )
        rows = hf_discovery._project_candidate(cand, ["bartowski"], {}, ["general"])
        self.assertEqual(rows, [])

    def test_candidate_without_gguf_variants_dropped(self) -> None:
        cand = _fake_candidate(
            repo_id="test/no-gguf-published",
            parameter_count=7_000_000_000,
            gguf_variants=[],
        )
        rows = hf_discovery._project_candidate(cand, ["bartowski"], {}, ["general"])
        self.assertEqual(rows, [])

    def test_falls_back_to_repo_id_when_base_model_missing(self) -> None:
        # GGUF-only publishers (no base_model link) must still produce
        # a row — source_hf_id falls back to the repo id.
        cand = _fake_candidate(
            repo_id="bartowski/foo-9B-GGUF",
            base_model=None,
            parameter_count=9_000_000_000,
            gguf_variants=[_variant("foo-9b-Q4_K_M.gguf", "Q4_K_M", 5_400_000_000)],
        )
        rows = hf_discovery._project_candidate(cand, ["bartowski"], {}, ["general"])
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0].source_hf_id, "bartowski/foo-9B-GGUF")

    def test_size_falls_back_to_quant_density_estimate(self) -> None:
        # whichllm sometimes returns file_size_bytes=0; the estimator
        # should plug a plausible size from params × density so the
        # recommender's fit predicate doesn't see a zero footprint.
        cand = _fake_candidate(
            repo_id="bartowski/foo-9B-GGUF",
            parameter_count=9_000_000_000,
            gguf_variants=[_variant("foo-9b-Q4_K_M.gguf", "Q4_K_M", 0)],
        )
        rows = hf_discovery._project_candidate(cand, ["bartowski"], {}, ["general"])
        self.assertEqual(len(rows), 1)
        # Q4_K_M density is 0.60 GB/Bparam → ~5.4 GB for a 9B model.
        self.assertGreater(rows[0].weights_bytes, 4_000_000_000)
        self.assertLess(rows[0].weights_bytes, 7_000_000_000)


class QuantFilenameMatchTest(unittest.TestCase):
    """Kept for the legacy helper that scrapes quant from filename.
    Unused in the current pipeline but exercised so a future re-add
    has a known-good reference."""

    def test_dash_separator(self) -> None:
        self.assertTrue(hf_discovery._quant_matches_filename("model-Q4_K_M.gguf", "Q4_K_M"))

    def test_dot_separator(self) -> None:
        self.assertTrue(hf_discovery._quant_matches_filename("model.Q4_K_M.gguf", "Q4_K_M"))

    def test_case_insensitive(self) -> None:
        self.assertTrue(hf_discovery._quant_matches_filename("model-q4_k_m.gguf", "Q4_K_M"))

    def test_rejects_substring_match(self) -> None:
        # Must be a full suffix match, not just contains.
        self.assertFalse(hf_discovery._quant_matches_filename("model-Q4_K_M-fp16.gguf", "Q4_K_M"))


class LowQuantAndProjectorTest(unittest.TestCase):
    def test_low_quants_only_for_large_models(self) -> None:
        variants = [_variant("m-Q2_K.gguf", "Q2_K", 1), _variant("m-Q4_K_M.gguf", "Q4_K_M", 2)]
        small = [q for q, _, _ in hf_discovery._collect_variants(variants, 9_000_000_000)]
        large = [q for q, _, _ in hf_discovery._collect_variants(variants, 30_000_000_000)]
        self.assertEqual(small, ["Q4_K_M"])
        self.assertEqual(large, ["Q2_K", "Q4_K_M"])

    def test_projector_file_is_not_a_weights_row(self) -> None:
        variants = [_variant("m.mmproj-Q8_0.gguf", "Q8_0", 1)]
        self.assertEqual(hf_discovery._collect_variants(variants, 30_000_000_000), [])


LADDER = hf_discovery.PREFERRED_QUANTS


class FrontierQuantLabelTest(unittest.TestCase):
    def test_labels(self) -> None:
        cases = {
            "Qwen3.6-35B-A3B-UD-Q2_K_XL.gguf": ("Q2_K_XL", True),
            "Q4_K_M/Qwen3.8-27B-Q4_K_M-00001-of-00002.gguf": ("Q4_K_M", False),
            "gpt-oss-20b-mxfp4.gguf": ("MXFP4", False),
            "Qwen3.6-35B-A3B-MXFP4_MOE.gguf": ("MXFP4", False),
            "Qwen3.8-Flash-Next.IQ4_XS.gguf": ("IQ4_XS", False),
        }
        for path, want in cases.items():
            self.assertEqual(_frontier.quant_label(path, LADDER), want, path)

    def test_rejects_non_weights_and_off_ladder(self) -> None:
        for path in (
            "Qwen_Qwen3.5-27B-Q2_K_L.gguf",
            "gemma-4-31B-it-Q6_K_XL.gguf",
            "mmproj-Qwen3.8-27B-Q8_0.gguf",
            "dspark-DeepSeek-V4-Flash-0731-Q8_0.gguf",
            "mtp-Qwen3.8-Flash-Next-Q8_0.gguf",
            "README.md",
        ):
            self.assertIsNone(_frontier.quant_label(path, LADDER), path)

    def test_collect_sums_shards_and_prefers_plain_over_ud(self) -> None:
        tree = [
            {"type": "file", "path": "Q4_K_M/m-Q4_K_M-00002-of-00002.gguf", "size": 5},
            {"type": "file", "path": "Q4_K_M/m-Q4_K_M-00001-of-00002.gguf", "size": 7},
            {"type": "file", "path": "m-UD-Q3_K_M.gguf", "size": 9},
            {"type": "file", "path": "m-Q3_K_M.gguf", "lfs": {"size": 8}, "size": 1},
            {"type": "directory", "path": "Q4_K_M"},
        ]
        got = _frontier.collect_quants(tree, LADDER)
        self.assertEqual(got["Q4_K_M"], ("Q4_K_M/m-Q4_K_M-00001-of-00002.gguf", 12))
        self.assertEqual(got["Q3_K_M"], ("m-Q3_K_M.gguf", 8))


def _fake_hf(responses: Dict[str, Any]):
    def fetch(path: str) -> Any:
        return responses.get(path.split("?", 1)[0])
    return fetch


class FrontierRowsTest(unittest.TestCase):
    def _rows(self, entries: List[Dict[str, Any]], responses: Dict[str, Any]) -> List[Dict[str, Any]]:
        allowlist = ["unsloth", "bartowski"]
        return _frontier.fetch_frontier_rows(
            entries,
            ladder=LADDER,
            low_quants=hf_discovery.LOW_QUANTS,
            low_quant_min_params=hf_discovery.LOW_QUANT_MIN_PARAMS,
            trusted=lambda pub, src: hf_discovery._publisher_trusted(pub, src, allowlist),
            task_hints=lambda src: ["general"],
            fetch=_fake_hf(responses),
        )

    def _source(self, params: int) -> Dict[str, Any]:
        return {"safetensors": {"total": params}, "config": {"model_type": "x"},
                "downloads": 5, "createdAt": "2026-10-01"}

    def test_reads_trusted_repo_files_and_moe_suffix(self) -> None:
        rows = self._rows(
            [{"id": "Org/M-35B-A3B"}],
            {
                "models/Org/M-35B-A3B": self._source(35_000_000_000),
                "models/unsloth/M-35B-A3B-GGUF": {"cardData": {"base_model": "Org/M-35B-A3B"}},
                "models/unsloth/M-35B-A3B-GGUF/tree/main": [
                    {"type": "file", "path": "M-UD-Q2_K_XL.gguf", "size": 11},
                    {"type": "file", "path": "M-Q4_K_M.gguf", "size": 20},
                ],
            },
        )
        self.assertEqual([r["quant"] for r in rows], ["Q2_K_XL", "Q4_K_M"])
        self.assertEqual(rows[0]["params_active"], 3_000_000_000)
        self.assertTrue(rows[0]["is_moe"] and rows[0]["curated"])
        self.assertEqual(rows[1]["weights_bytes"], 20)

    def test_skips_other_bases_untrusted_orgs_and_small_low_quants(self) -> None:
        rows = self._rows(
            [{"id": "Org/Small-8B", "active": None}],
            {
                "models/Org/Small-8B": self._source(8_000_000_000),
                # A fine-tune's quants under the same repo name.
                "models/unsloth/Small-8B-GGUF": {"cardData": {"base_model": "Other/Tune-8B"}},
                "models/bartowski/Org_Small-8B-GGUF": {"cardData": {}},
                "models/bartowski/Org_Small-8B-GGUF/tree/main": [
                    {"type": "file", "path": "Org_Small-8B-Q2_K.gguf", "size": 3},
                    {"type": "file", "path": "Org_Small-8B-Q8_0.gguf", "size": 9},
                ],
                "models/mradermacher/Small-8B-GGUF": {"cardData": {}},
                "models/mradermacher/Small-8B-GGUF/tree/main": [
                    {"type": "file", "path": "Small-8B.Q4_K_M.gguf", "size": 5},
                ],
            },
        )
        self.assertEqual([(r["quant"], r["repo"]) for r in rows],
                         [("Q8_0", "bartowski/Org_Small-8B-GGUF")])
        self.assertFalse(rows[0]["is_moe"])

    def test_curated_list_file_parses(self) -> None:
        entries = _frontier.load_frontier_list(REPO_ROOT)
        self.assertGreater(len(entries), 20)
        self.assertTrue(all("/" in e["id"] for e in entries))


class RankAndCapCuratedTest(unittest.TestCase):
    def test_curated_replaces_discovered_rows_and_skips_the_cap(self) -> None:
        rows = [
            {"source_hf_id": "Org/A", "quant": "Q4_K_M", "downloads": 9},
            {"source_hf_id": "Org/A", "quant": "Q5_K_M", "downloads": 9},
            {"source_hf_id": "org/a", "quant": "Q4_K_M", "downloads": 1, "curated": True},
            {"source_hf_id": "Org/B", "quant": "Q4_K_M", "downloads": 5},
            {"source_hf_id": "Org/C", "quant": "Q4_K_M", "downloads": 4},
        ]
        got = hf_discovery._rank_and_cap(rows, limit=1)
        self.assertEqual([(r["source_hf_id"], r["quant"]) for r in got],
                         [("org/a", "Q4_K_M"), ("Org/B", "Q4_K_M")])


def _aa_page(records: List[Dict[str, Any]]) -> str:
    """A leaderboard page with ``records`` in one RSC chunk."""
    import json
    blob = json.dumps(records, separators=(",", ":"))
    return f"<script>self.__next_f.push([1,{json.dumps(blob)}])</script>"


class AaIndexTest(unittest.TestCase):
    def test_parse_keeps_the_best_variant_and_skips_unscored_records(self) -> None:
        html = _aa_page([
            {"name": "Qwen3.8 27B (Xhigh)", "intelligenceIndex": 33.7},
            {"name": "Qwen3.8 27B (Low)", "intelligenceIndex": 26.2},
            {"name": "Old Model", "intelligenceIndex": None},
            {"name": "Gemma 4 E2B (Reasoning)", "intelligenceIndex": 7.8},
        ])
        self.assertEqual(_aa_index.parse_leaderboard(html),
                         {"qwen3.8 27b": 33.7, "gemma 4 e2b": 7.8})

    def test_fit_recovers_a_line_and_refuses_small_overlaps(self) -> None:
        ref = {f"m{i}": float(i) for i in range(30)}
        src = {f"m{i}": (i - 4.0) / 2 for i in range(30)}
        k, c = _aa_index.fit(src, ref)
        self.assertAlmostEqual(k, 2.0)
        self.assertAlmostEqual(c, 4.0)
        self.assertIsNone(_aa_index.fit(dict(list(src.items())[:5]), ref))

    def test_live_page_fills_only_names_the_capture_lacks(self) -> None:
        import json
        ref = json.loads((REPO_ROOT / "data" / "aa-index.json").read_text())["scores"]
        live = [{"name": n, "intelligenceIndex": s / 2} for n, s in list(ref.items())[:40]]
        live.append({"name": "Brand New 9B", "intelligenceIndex": 10.0})
        got = _aa_index.load_scores(REPO_ROOT, _aa_page(live))
        first = next(iter(ref))
        self.assertEqual(got[first], _aa_index.normalize(ref[first]))
        self.assertEqual(got["brand new 9b"], _aa_index.normalize(20.0))

    def test_every_curated_aa_name_is_in_the_capture(self) -> None:
        scores = _aa_index.load_scores(REPO_ROOT, None)
        missing = [e["id"] for e in _frontier.load_frontier_list(REPO_ROOT)
                   if e.get("aa") and _aa_index.canonical(str(e["aa"])) not in scores]
        self.assertEqual(missing, [])


class TaskHintsAndAllowlistFilesTest(unittest.TestCase):
    """Smoke-test the YAML loaders against the real side-files."""

    def test_load_task_hints_returns_non_empty_prefixes(self) -> None:
        prefixes, defaults = hf_discovery.load_task_hints(REPO_ROOT)
        self.assertGreater(len(prefixes), 0)
        self.assertEqual(defaults, ["general"])
        # Spot-check: Qwen3-Coder must be tagged code.
        self.assertEqual(prefixes.get("Qwen/Qwen3-Coder"), ["code"])

    def test_load_publisher_allowlist_contains_known_orgs(self) -> None:
        allowlist = hf_discovery.load_publisher_allowlist(REPO_ROOT)
        self.assertIn("bartowski", allowlist)
        self.assertIn("unsloth", allowlist)


if __name__ == "__main__":
    unittest.main()
