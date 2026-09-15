import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("contract_packet_eval.py")
SPEC = importlib.util.spec_from_file_location("contract_packet_eval", MODULE_PATH)
assert SPEC and SPEC.loader
subject = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = subject
SPEC.loader.exec_module(subject)


class ContractPacketEvalTests(unittest.TestCase):
    def test_signal_requires_each_regex_group(self):
        signal = subject.compile_signals([{
            "id": "two-part-contract",
            "groups": [["password|credential"], ["rollback|restore"]],
        }])[0]
        self.assertFalse(subject.signal_covered(signal, "password changed"))
        self.assertTrue(subject.signal_covered(signal, "credential restore path"))

    def test_diff_paths_and_exclusions_feed_artifact_recall(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "accepted.patch").write_text(
                "diff --git a/src/A.vb b/src/A.vb\n"
                "diff --git a/generated.js b/generated.js\n",
                encoding="utf-8",
            )
            artifacts = root / "artifacts"
            artifacts.mkdir()
            (artifacts / "feature-contract.md").write_text("Inspect src/A.vb", encoding="utf-8")
            fixture = {
                "artifact_files": ["feature-contract.md"],
                "reference_diff": str(root / "accepted.patch"),
                "exclude_paths": ["generated.js"],
            }
            result = subject.artifact_metrics(fixture, [], artifacts)
            self.assertEqual(result["accepted_paths"], 1)
            self.assertEqual(result["accepted_paths_mentioned"], 1)
            self.assertEqual(result["accepted_path_recall"], 1.0)

    def test_packet_optimizer_combines_partial_candidates_and_stops_when_saturated(self):
        signals = subject.compile_signals([{
            "id": "continuity",
            "weight": 2,
            "groups": [["password"], ["restore"]],
        }])
        candidates = [
            subject.Candidate("A", "obligation", "password mutation"),
            subject.Candidate("B", "hypothesis", "restore on failure"),
            subject.Candidate("C", "caller", "unrelated text"),
        ]
        result = subject.optimize_packets(candidates, signals, [3], 50, 7)["3"]
        self.assertEqual(result["weighted_signal_recall"], 1.0)
        self.assertEqual(result["selected_count"], 2)
        self.assertEqual({row["id"] for row in result["selected"]}, {"A", "B"})

    def test_evidence_candidates_are_unique_and_matrix_items_are_split(self):
        evidence = {
            "story": "approved behavior",
            "contract_checkpoint": {"hard_items": [
                {"id": "OBL-HARD", "kind": "obligation", "requirement": "hard"}
            ]},
            "cross_cutting_obligations": [{
                "obligation": "mutation",
                "advisory_contract_items": [{"check_id": "OBL-01", "requirement": "advance"}],
            }],
            "row_guidance": {"entries": [
                {"id": "G001", "field": "impact_question", "text": "resolved guidance"}
            ]},
            "files": [{"row_id": "P001", "path": "A.vb", "impact_question_ref": "G001"}],
            "boundary_audit": {"categories": [{"boundary": "request_pipeline", "paths": ["A.vb"]}]},
            "configured_contract_rules": {"rules": [{"id": "RULE-CFG", "requirement": "configured"}]},
            "applicable_repository_rules": {"rules": [{"rule_id": "STYLE-1", "requirement": "style"}]},
        }
        matrix = (
            "- **TM-INTENT-01** first\n  - outcome A\n"
            "- **TM-INTENT-02** second\n  - outcome B\n"
            "Historical knowledge cutoff: 2020-01-01\n"
        )
        candidates = subject.evidence_candidates(evidence, matrix)
        self.assertEqual(
            {item.id for item in candidates},
            {
                "STORY", "OBL-HARD", "OBL-01", "P001", "BOUNDARY-REQUEST_PIPELINE",
                "RULE-CFG", "REPO-RULE-STYLE-1", "TM-INTENT-01", "TM-INTENT-02",
            },
        )
        primary = next(item for item in candidates if item.id == "P001")
        self.assertIn("resolved guidance", primary.text)
        hard = next(item for item in candidates if item.id == "OBL-HARD")
        self.assertEqual(hard.kind, "hard_obligation")

    def test_packet_metrics_scores_combined_multi_item_evidence(self):
        signals = subject.compile_signals([{
            "id": "combined",
            "weight": 3,
            "groups": [["password"], ["restore"]],
        }])
        result = subject.packet_metrics([
            subject.Candidate("A", "hard_obligation", "password change"),
            subject.Candidate("B", "hard_hypothesis", "restore after failure"),
        ], signals)
        self.assertEqual(result["weighted_signal_recall"], 1.0)
        self.assertEqual(result["items"], 2)

    def test_payload_comparison_proves_dictionary_compaction_is_lossless(self):
        receipt = subject.hashlib.sha256(b"P001\0A.vb\n").hexdigest()
        contract_receipt = subject.hashlib.sha256(b"OBL-01-X-C01\n").hexdigest()
        baseline = {
            "files": [{"row_id": "P001", "path": "A.vb", "impact_question": "same guidance text long enough to compact"}],
            "asset_dependencies": [],
            "caller_dependencies": [],
            "reconciliation": {"receipt_id": f"sha256:{receipt}"},
            "contract_checkpoint": {
                "receipt": {"receipt_id": f"sha256:{contract_receipt}"},
                "obligation_check_ids": ["OBL-01-X-C01"],
                "hypothesis_evidence_ids": [],
            },
            "cross_cutting_obligations": [{"contract_items": [
                {"check_id": "OBL-01-X-C01", "requirement": "preserve behavior"}
            ]}],
            "component_hypotheses": [],
        }
        candidate = {
            **baseline,
            "files": [{"row_id": "P001", "path": "A.vb", "impact_question_ref": "G001"}],
            "row_guidance": {"entries": [{
                "id": "G001", "field": "impact_question", "text": "same guidance text long enough to compact"
            }]},
            "contract_checkpoint": {
                **baseline["contract_checkpoint"],
                "hard_items": [{"id": "OBL-01-X-C01", "requirement": "preserve behavior"}],
                "advisory_items_total": 0,
            },
            "cross_cutting_obligations": [],
        }
        result = subject.compare_evidence_payloads(baseline, candidate)
        self.assertTrue(result["gates"]["all_rows_lossless"])
        self.assertTrue(result["gates"]["contract_requirements_lossless"])
        self.assertTrue(result["candidate"]["contract_receipt_valid"])

    def test_contract_requirements_include_configured_advisory_rules(self):
        evidence = {"configured_contract_rules": {"rules": [
            {"id": "RULE-ADVISORY", "requirement": "inspect the configured behavior"}
        ]}}
        self.assertEqual(
            subject.contract_requirements(evidence),
            {"RULE-ADVISORY": "inspect the configured behavior"},
        )

    def test_hydrated_rows_rejects_missing_or_wrong_dictionary_entries(self):
        with self.assertRaisesRegex(ValueError, "unresolved impact_question_ref"):
            subject.hydrated_rows({"files": [{"impact_question_ref": "G404"}]}, "files")

    def test_character_optimizer_respects_payload_budget(self):
        signals = subject.compile_signals([
            {"id": "one", "groups": [["alpha"], ["beta"]]},
            {"id": "two", "groups": [["gamma"]]},
        ])
        candidates = [
            subject.Candidate("A", "row", "alpha"),
            subject.Candidate("B", "row", "beta"),
            subject.Candidate("C", "row", "gamma filler"),
        ]
        result = subject.optimize_packets_by_chars(candidates, signals, [21], 100, 4)["21"]
        self.assertLessEqual(result["packet_chars"], 21)
        self.assertEqual(result["weighted_signal_recall"], 1.0)

    def test_signal_diagnostics_names_only_unavailable_regex_groups(self):
        signals = subject.compile_signals([{
            "id": "partial", "weight": 3, "groups": [["password"], ["rollback|restore"]]
        }])
        diagnostics = subject.signal_diagnostics(
            [subject.Candidate("A", "row", "password change")], signals
        )
        self.assertEqual(diagnostics[0]["signal_id"], "partial")
        self.assertEqual(diagnostics[0]["available_groups"], 1)
        self.assertEqual(diagnostics[0]["missing_groups"][0]["patterns"], ["rollback|restore"])

    def test_supplements_are_strict_and_measure_material_gain(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            supplement = root / "supplement.json"
            supplement.write_text(
                '{"version":1,"candidates":[{"id":"NEW","kind":"obligation","text":"gamma"}]}',
                encoding="utf-8",
            )
            loaded = subject.read_supplements([supplement])
            self.assertEqual(loaded, [subject.Candidate("NEW", "obligation", "gamma")])
            self.assertFalse(subject.supplement_source_bindings([supplement])[0]["bound"])
            source = root / "production.rs"
            source.write_text("required gamma behavior", encoding="utf-8")
            supplement.write_text(
                '{"version":1,"candidates":[{"id":"NEW","kind":"obligation","text":"gamma",'
                '"source_path":"production.rs","source_contains":"gamma behavior"}]}',
                encoding="utf-8",
            )
            self.assertTrue(subject.supplement_source_bindings([supplement])[0]["bound"])
            supplement.write_text(
                '{"version":1,"candidates":[{"id":"NEW","text":""}]}', encoding="utf-8"
            )
            with self.assertRaisesRegex(ValueError, "blank id, kind, or text"):
                subject.read_supplements([supplement])


if __name__ == "__main__":
    unittest.main()
