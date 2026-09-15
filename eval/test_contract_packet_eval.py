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
            "cross_cutting_obligations": [{
                "obligation": "mutation",
                "contract_items": [{"check_id": "OBL-01", "requirement": "advance"}],
            }],
            "files": [{"row_id": "P001", "path": "A.vb"}],
        }
        matrix = (
            "- **TM-INTENT-01** first\n  - outcome A\n"
            "- **TM-INTENT-02** second\n  - outcome B\n"
            "Historical knowledge cutoff: 2020-01-01\n"
        )
        candidates = subject.evidence_candidates(evidence, matrix)
        self.assertEqual({item.id for item in candidates}, {"OBL-01", "P001", "TM-INTENT-01", "TM-INTENT-02"})


if __name__ == "__main__":
    unittest.main()
