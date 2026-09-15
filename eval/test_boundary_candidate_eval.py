import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("boundary_candidate_eval.py")
SPEC = importlib.util.spec_from_file_location("boundary_candidate_eval", MODULE_PATH)
assert SPEC and SPEC.loader
subject = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = subject
SPEC.loader.exec_module(subject)


class BoundaryCandidateEvalTests(unittest.TestCase):
    def test_precision_filter_keeps_source_lead_and_rejects_noise(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            for relative in [
                "src/LoginController.vb", "src/InvoiceService.vb", "web/login.min.js",
                "Bin/Auth.dll.refresh", "node_modules/pkg/login.js", "azure-pipelines.yml",
            ]:
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("fixture", encoding="utf-8")
            report = subject.evaluate(root, {"files": []}, True, False, {
                "required_leads": ["LoginController.vb"],
                "forbidden_leads": ["login.min.js", "Auth.dll.refresh", "InvoiceService.vb"],
                "max_unretrieved_candidates": 1,
            })
            self.assertTrue(report["ready"])
            self.assertEqual(report["candidates"], ["src/LoginController.vb"])

    def test_retrieved_candidate_is_not_reported_as_uninspected(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            path = root / "src/LoginController.cs"
            path.parent.mkdir(parents=True)
            path.write_text("fixture", encoding="utf-8")
            report = subject.evaluate(
                root, {"files": [{"path": "SRC\\logincontroller.cs"}]}, True, False
            )
            self.assertEqual(report["unretrieved_candidates"], 0)


if __name__ == "__main__":
    unittest.main()
