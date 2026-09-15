import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("planning_rule_eval.py")
SPEC = importlib.util.spec_from_file_location("planning_rule_eval", MODULE_PATH)
assert SPEC and SPEC.loader
subject = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = subject
SPEC.loader.exec_module(subject)


class PlanningRuleEvalTests(unittest.TestCase):
    def write_pack(self, root: Path, name: str, text: str) -> Path:
        path = root / name
        path.write_text(text, encoding="utf-8")
        return path

    def test_project_pack_overrides_and_historical_cutoff_matches_rust_contract(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            global_pack = self.write_pack(root, "global.yaml", """
version: 1
rules:
  - id: continuity
    title: old
    requirement: old requirement
    introduced_at: 2020-01-01
    story_any: [password]
  - id: future
    title: future
    requirement: future requirement
    introduced_at: 2022-01-01
    story_any: [password]
""")
            project_pack = self.write_pack(root, "project.yaml", """
version: 1
introduced_at: 2020-02-01
rules:
  - id: CONTINUITY
    title: project
    requirement: restored project requirement
    severity: release_blocking_if_applicable
    story_any: [password]
""")
            matched, notes = subject.evaluate_packs(
                [("global", global_pack), ("project", project_pack)],
                "change password", [], "2021-01-01"
            )
            self.assertEqual(len(matched), 1)
            self.assertEqual(matched[0].requirement, "restored project requirement")
            self.assertEqual(matched[0].source, "project")
            self.assertEqual([note["rule_id"] for note in notes], ["RULE-FUTURE"])

    def test_story_and_path_predicates_must_both_match(self):
        with tempfile.TemporaryDirectory() as folder:
            pack = self.write_pack(Path(folder), "rules.yaml", """
version: 1
introduced_at: 2020-01-01
rules:
  - id: api-session
    title: API session
    requirement: preserve API behavior
    story_all: [session]
    story_none: [public]
    path_all: [site/, auth]
    path_any: [api/]
    path_none: [fixtures/]
""")
            hit, _ = subject.evaluate_packs(
                [("test", pack)], "session expiry", ["Site/API/Auth.vb"], None
            )
            story_miss, _ = subject.evaluate_packs(
                [("test", pack)], "public session expiry", ["Site/API/Auth.vb"], None
            )
            path_miss, _ = subject.evaluate_packs(
                [("test", pack)], "session expiry", ["Site/Page.aspx"], None
            )
            excluded_path, _ = subject.evaluate_packs(
                [("test", pack)], "session expiry", ["Site/API/Auth/fixtures/User.vb"], None
            )
            mixed_paths, _ = subject.evaluate_packs(
                [("test", pack)], "session expiry",
                ["Site/API/Auth.vb", "Site/API/Auth/fixtures/User.vb"], None
            )
            self.assertEqual(len(hit), 1)
            self.assertFalse(story_miss)
            self.assertFalse(path_miss)
            self.assertFalse(excluded_path)
            self.assertEqual(len(mixed_paths), 1)

    def test_invalid_calendar_date_and_unknown_field_are_rejected(self):
        with tempfile.TemporaryDirectory() as folder:
            pack = self.write_pack(Path(folder), "rules.yaml", """
version: 1
introduced_at: 2020-02-31
unexpected: true
rules: []
""")
            with self.assertRaises(ValueError):
                subject.load_pack(pack, "test")


if __name__ == "__main__":
    unittest.main()
