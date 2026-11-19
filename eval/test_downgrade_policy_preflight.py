"""Navigation hints must not grant authority to the old-writer preflight."""
import json
import unittest

import blake3

from downgrade_policy_preflight import (
    OLD_POLICY_PROFILE, PreflightRefusal, inspect_inventory,
)

PROJECT = "00000000-0000-4000-8000-000000000018"


def raw(text):
    data = text.encode("utf-8")
    return dict(bytes=len(data), blake3=blake3.blake3(data).hexdigest(), utf8=text, hex=None)


def inventory(hints=False):
    project = dict(project_id=PROJECT, project_name="Owned preflight contract",
                   project_type="general", directory="C:/owned/source",
                   created_at_ms=1, updated_at_ms=1)
    value = dict(status="ownership_inventory_only_requires_explicit_decisions",
                 registry_snapshot_blake3="0" * 64, project_id=PROJECT,
                 project=project, project_raw=raw(json.dumps(project)),
                 identity_epoch=1, identity_epoch_raw=raw("1"),
                 identity_status="existing_epoch_observed", corpus_revision_raw=None,
                 rows=[], receipts=[], sources=[], enforcement_notes=[],
                 enforcement_basis="Complete captured rows", unresolved_claim_keys=[],
                 coverage="Bounded inventory", adoption_performed=False, authored_files_written=0)
    if hints:
        value.update(next_tools=[dict(tool="get_review_decisions",
                                     arguments=dict(project_id=PROJECT),
                                     reason="Read applicable decisions; this does not approve an edit.")],
                     next_tools_status="suggested; backend and project-data requirements still apply")
    return value


def inspect(value, old=OLD_POLICY_PROFILE):
    return inspect_inventory(json.dumps(value), PROJECT, old)


class AdvisoryOwnershipTests(unittest.TestCase):
    def test_original_inventory_still_accepted(self):
        value = inventory()
        self.assertEqual(inspect(value), value)

    def test_current_navigation_preserves_complete_inventory(self):
        value = inventory(hints=True)
        self.assertEqual(inspect(value), value)

    def test_navigation_cannot_approve_adoption(self):
        value = inventory(hints=True)
        value["adoption_performed"] = True
        with self.assertRaises(PreflightRefusal):
            inspect(value)

    def test_unknown_authority_field_still_refuses(self):
        value = inventory(hints=True)
        value["accepted_owner"] = "suggested by navigation"
        with self.assertRaises(PreflightRefusal):
            inspect(value)

    def test_incomplete_navigation_refuses(self):
        value = inventory(hints=True)
        del value["next_tools_status"]
        with self.assertRaises(PreflightRefusal):
            inspect(value)

    def test_authority_status_in_navigation_refuses(self):
        value = inventory(hints=True)
        value["next_tools_status"] = "approved_owner"
        with self.assertRaises(PreflightRefusal):
            inspect(value)

    def test_authority_extension_in_action_refuses(self):
        value = inventory(hints=True)
        value["next_tools"][0]["accepted_owner"] = True
        with self.assertRaises(PreflightRefusal):
            inspect(value)

    def test_navigation_does_not_bypass_binary_identity(self):
        with self.assertRaises(PreflightRefusal):
            inspect(inventory(hints=True), old="0" * 64)


if __name__ == "__main__":
    unittest.main()
