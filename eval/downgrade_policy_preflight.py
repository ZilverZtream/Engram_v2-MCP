"""Fail-closed operator preflight for the exact b762d3d4 old writer.

This is not an application minimum-version fence. Call only on an exclusively
owned/quiescent store; the old executable itself can bypass this procedure.
"""
import json
import re

import blake3

OLD_POLICY_PROFILE = "b762d3d4a10b2b3a722fa90be07ae957c9d72c99f78609b57319e02e4e366bdd"
MAX_INVENTORY = 4 * 1024**2
ROW_FIELDS = {"rule_id", "file_pattern", "rule_text", "priority", "updated_at_ms",
              "introduced_at", "provenance"}
INVENTORY_FIELDS = {"status", "registry_snapshot_blake3", "project_id", "project_raw",
    "project", "identity_epoch_raw", "identity_epoch", "identity_status",
    "corpus_revision_raw", "rows", "receipts", "sources", "enforcement_notes",
    "enforcement_basis", "unresolved_claim_keys", "coverage", "adoption_performed",
    "authored_files_written"}
RECEIPT_FIELDS = {"origin", "source_hash", "authored", "pending", "doc_ids", "rules",
                  "cleanup", "corpus_revision", "legacy_aliases"}


class PreflightRefusal(RuntimeError):
    pass


def need(condition, message):
    if not condition:
        raise PreflightRefusal(message)


def strict_json(text):
    def pairs(entries):
        result = {}
        for key, value in entries:
            need(key not in result, "Duplicate JSON key in ownership evidence")
            result[key] = value
        return result
    def constant(value):
        raise PreflightRefusal("Non-JSON numeric value in ownership evidence")
    try:
        return json.loads(text, object_pairs_hook=pairs, parse_constant=constant)
    except (ValueError, TypeError, RecursionError) as error:
        raise PreflightRefusal("Malformed ownership evidence") from error


def shape(value, keys, label):
    need(isinstance(value, dict) and set(value) == set(keys), f"Unknown/incomplete {label} shape")


def hash_string(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-fA-F]{64}", value) is not None


def raw_bytes(value):
    shape(value, {"bytes", "blake3", "utf8", "hex"}, "raw evidence")
    need(type(value["bytes"]) is int and 0 <= value["bytes"] <= MAX_INVENTORY,
         "Raw evidence byte budget invalid")
    need(isinstance(value["utf8"], str) and value["hex"] is None,
         "Non-UTF8 ownership evidence is unsupported")
    data = value["utf8"].encode("utf-8")
    need(len(data) == value["bytes"] and hash_string(value["blake3"])
         and blake3.blake3(data).hexdigest() == value["blake3"].lower(),
         "Raw ownership fingerprint mismatch")
    return data


def row(value):
    need(isinstance(value, dict) and set(value) <= ROW_FIELDS
         and ROW_FIELDS - {"introduced_at", "provenance"} <= set(value),
         "Unknown/incomplete old-compatible repository rule")
    value = {"introduced_at": None, "provenance": None, **value}
    need(all(isinstance(value[key], str) and value[key]
             for key in ("rule_id", "file_pattern", "rule_text")), "Invalid rule identity/text/scope")
    need(type(value["priority"]) is int and -(2**31) <= value["priority"] < 2**31
         and type(value["updated_at_ms"]) is int and 0 <= value["updated_at_ms"] < 2**64,
         "Invalid old-compatible rule integer fields")
    need(all(value[key] is None or isinstance(value[key], str)
             for key in ("introduced_at", "provenance")), "Invalid rule evidence/provenance type")
    return value


def inspect_inventory(text, project_id, old_sha256):
    """Validate the complete exact inventory; return its unchanged parsed value.

    Accepted manual rows retain all old RepoRule fields. Source claims must use
    the ordinary old-compatible ID/receipt shape with zero legacy aliases.
    Unknown, malformed, pending, stale or partially observed evidence refuses.
    """
    need(isinstance(text, str) and len(text.encode("utf-8")) <= MAX_INVENTORY,
         "Ownership response exceeds exact inventory budget")
    inventory = strict_json(text)
    shape(inventory, INVENTORY_FIELDS, "ownership inventory")
    need(old_sha256.lower() == OLD_POLICY_PROFILE, "Old binary policy semantics are unproven")
    need(isinstance(inventory["enforcement_notes"], list)
         and all(isinstance(note, str) for note in inventory["enforcement_notes"])
         , "Malformed enforcement coverage notes")
    need(inventory["status"] == "ownership_inventory_only_requires_explicit_decisions"
         and inventory["project_id"] == project_id
         and inventory["adoption_performed"] is False
         and type(inventory["authored_files_written"]) is int
         and inventory["authored_files_written"] == 0
         and inventory["unresolved_claim_keys"] == []
         and inventory["identity_status"] == "existing_epoch_observed"
         and type(inventory["identity_epoch"]) is int and inventory["identity_epoch"] > 0
         and hash_string(inventory["registry_snapshot_blake3"]),
         "Unresolved/incomplete ownership inventory")
    project = strict_json(raw_bytes(inventory["project_raw"]).decode("utf-8"))
    need(isinstance(project, dict) and set(project) <= {"project_id", "project_name", "project_type",
         "directory", "created_at_ms", "updated_at_ms", "reindex_required_since_ms"}
         and {"project_id", "project_name", "project_type", "directory", "created_at_ms", "updated_at_ms"} <= set(project),
         "Unknown/incomplete project identity schema")
    need(isinstance(project, dict) and project == inventory["project"] and project.get("project_id") == project_id,
         "Project raw/decoded identity mismatch")
    epoch = raw_bytes(inventory["identity_epoch_raw"]).decode("ascii")
    need(epoch == str(inventory["identity_epoch"]), "Project epoch mismatch")
    need(isinstance(inventory["rows"], list) and len(inventory["rows"]) <= 2048
         and isinstance(inventory["receipts"], list) and len(inventory["receipts"]) <= 256
         and isinstance(inventory["sources"], list) and len(inventory["sources"]) <= 64,
         "Inventory count/budget is incomplete")
    rows = {}
    for observed in inventory["rows"]:
        shape(observed, {"rule_id", "raw", "decoded", "status", "authority",
            "registry_enforcement", "ownership_status", "claim_status", "receipt_claims"}, "rule observation")
        parsed = row(strict_json(raw_bytes(observed["raw"]).decode("utf-8")))
        need(parsed == observed["decoded"] and parsed["rule_id"] == observed["rule_id"]
             and parsed["rule_id"] not in rows
             and observed["status"] == "decoded_without_rewriting_raw_row",
             "Unresolved/duplicate raw repository row")
        rows[parsed["rule_id"]] = (parsed, observed)
    sources = {}
    for source in inventory["sources"]:
        shape(source, {"path", "status", "blake3", "bytes"}, "source observation")
        need(source["path"] not in sources and hash_string(source["blake3"])
             and source["status"] == "complete_raw_fingerprint"
             and type(source["bytes"]) is int and 0 <= source["bytes"] <= MAX_INVENTORY,
             "Source evidence unavailable, stale or omitted")
        sources[source["path"]] = source
    claims = {}
    receipt_keys = set()
    corpus = None if inventory["corpus_revision_raw"] is None else raw_bytes(
        inventory["corpus_revision_raw"]).decode("utf-8")
    for observed in inventory["receipts"]:
        shape(observed, {"metadata_key", "raw", "status", "origin", "pending", "authored",
            "declared_source_blake3", "source_fingerprint_status", "claimed_rule_ids"}, "receipt observation")
        need(observed["metadata_key"] not in receipt_keys, "Duplicate source receipt observation")
        receipt_keys.add(observed["metadata_key"])
        receipt = strict_json(raw_bytes(observed["raw"]).decode("utf-8"))
        # Empty alias arrays are a harmless current serialization extension.
        need(isinstance(receipt, dict) and set(receipt) <= RECEIPT_FIELDS
             and RECEIPT_FIELDS - {"cleanup", "corpus_revision", "legacy_aliases"} <= set(receipt),
             "Unknown/incomplete source receipt")
        receipt = {"cleanup": [], "corpus_revision": "", "legacy_aliases": [], **receipt}
        need(receipt["legacy_aliases"] == [], "Adopted legacy alias authority is unsupported by the old writer")
        origin = receipt["origin"]
        need(isinstance(origin, str) and 0 < len(origin) <= 512
             and "\\" not in origin and ":" not in origin
             and all(part not in ("", ".", "..") for part in origin.split("/")),
             "Unsafe source receipt origin")
        need(observed["status"] == "metadata_claims_not_ownership_proof"
             and observed["metadata_key"] == "quality_source:" + blake3.blake3(origin.encode()).hexdigest()
             and observed["origin"] == origin and observed["pending"] is False
             and receipt["pending"] is False and type(receipt["authored"]) is bool
             and observed["authored"] == receipt["authored"]
             and receipt["cleanup"] == [] and isinstance(receipt["corpus_revision"], str)
             and receipt["corpus_revision"] == (corpus or "")
             and hash_string(receipt["source_hash"])
             and observed["declared_source_blake3"] == receipt["source_hash"]
             and observed["source_fingerprint_status"] == "matches_declared_bytes_not_ownership"
             and origin in sources and sources[origin]["blake3"] == receipt["source_hash"],
             "Pending, malformed, stale or unobserved source receipt")
        need(isinstance(receipt["doc_ids"], list)
             and all(isinstance(value, str) and value for value in receipt["doc_ids"])
             and len(set(receipt["doc_ids"])) == len(receipt["doc_ids"])
             and isinstance(receipt["rules"], list), "Malformed source document/rule inventory")
        ids = []
        for current in receipt["rules"]:
            current = row(current); identifier = current["rule_id"]
            need(identifier not in claims and identifier in rows and current == rows[identifier][0]
                 and any(re.fullmatch("qg-" + category + ":" + re.escape(origin) + r":[0-9]+", identifier)
                         for category in ("distilled", "copilot", "coding_rule"))
                 and current["provenance"] == "quality-gate source: " + origin,
                 "Opaque, competing or noncanonical source ownership")
            claims[identifier] = (observed["metadata_key"], origin, len(ids))
            ids.append(identifier)
        need(ids == observed["claimed_rule_ids"], "Receipt rule inventory mismatch")
    for identifier, (current, observed) in rows.items():
        if identifier in claims:
            key, origin, index = claims[identifier]
            expected = observed["receipt_claims"]
            need(isinstance(expected, list) and len(expected) == 1, "Incomplete source claim list")
            shape(expected[0], {"receipt_key", "origin", "pending", "claim_index",
                "matches_current_rule_fields"}, "source row claim")
            need(observed["authority"] == "source_reference"
                 and observed["ownership_status"] == "single_claim_requires_source_and_row_review"
                 and observed["claim_status"] == "one_declared_claim_not_ownership_proof" and len(expected) == 1
                 and expected[0]["receipt_key"] == key and expected[0]["origin"] == origin
                 and expected[0]["pending"] is False and expected[0]["claim_index"] == index
                 and expected[0]["matches_current_rule_fields"] is True,
                 "Source row claim/authority mismatch")
        else:
            need(current["provenance"] in ("manual", "user-authored", "manual repository rule")
                 and observed["authority"] == "accepted_manual"
                 and observed["ownership_status"] == "no_receipt_claim_explicit_decision_required"
                 and observed["claim_status"] == "no_parsed_claim_occurrences" and observed["receipt_claims"] == [],
                 "Unverified or unknown non-source rule authority")
        need(observed["registry_enforcement"] == "retained_unfiltered_registry_constraint",
             "Current source/manual enforcement is unresolved")
    need(inventory["enforcement_notes"] == [], "Unresolved enforcement coverage notes")
    return inventory
