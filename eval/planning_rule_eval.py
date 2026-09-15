"""Validate and score hot-loaded planning rules without rebuilding Engram.

The matcher deliberately mirrors planning_contract_rules.rs. It is an offline
development gate, not production code: tune organization or repository rule
packs here, then spend one build/replay only after the pack is valid, matched,
historically admissible, and measurably closes held-out evidence gaps.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import re
import sys
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from typing import Any

import yaml


CONTRACT_EVAL_PATH = Path(__file__).with_name("contract_packet_eval.py")
SPEC = importlib.util.spec_from_file_location("contract_packet_eval", CONTRACT_EVAL_PATH)
assert SPEC and SPEC.loader
contract_eval = importlib.util.module_from_spec(SPEC)
sys.modules.setdefault(SPEC.name, contract_eval)
SPEC.loader.exec_module(contract_eval)

MAX_BYTES = 256 * 1024
MAX_RULES = 128
VALID_SEVERITIES = {
    "release_blocking_if_applicable", "required_if_applicable", "advisory"
}
SELECTORS = ("story_all", "story_any", "story_none", "path_all", "path_any", "path_none")


@dataclass(frozen=True)
class Rule:
    id: str
    title: str
    requirement: str
    severity: str
    story_all: tuple[str, ...]
    story_any: tuple[str, ...]
    story_none: tuple[str, ...]
    path_all: tuple[str, ...]
    path_any: tuple[str, ...]
    path_none: tuple[str, ...]
    oracle_guard: str | None
    introduced_at: str | None
    provenance: str | None
    source: str


def clean_text(value: Any, maximum: int) -> str:
    if not isinstance(value, str):
        raise ValueError("expected text")
    cleaned = value.strip()
    if not cleaned or len(cleaned) > maximum or any(ord(ch) < 32 for ch in cleaned):
        raise ValueError(f"text must be 1-{maximum} visible characters")
    return cleaned


def clean_date(value: Any) -> str:
    if isinstance(value, date):
        return value.isoformat()
    cleaned = clean_text(value, 10)
    try:
        parsed = date.fromisoformat(cleaned)
    except ValueError as error:
        raise ValueError("introduced_at must be a real YYYY-MM-DD date") from error
    if parsed.isoformat() != cleaned:
        raise ValueError("introduced_at must be YYYY-MM-DD")
    return cleaned


def clean_selector(value: Any, name: str) -> tuple[str, ...]:
    if value is None:
        return ()
    if not isinstance(value, list) or len(value) > 32:
        raise ValueError(f"{name} must be a list of at most 32 terms")
    return tuple(clean_text(item, 128).replace("\\", "/").casefold() for item in value)


def load_pack(path: Path, source: str) -> list[Rule]:
    if not path.is_file():
        raise ValueError(f"{source} rule pack is not a file: {path}")
    if path.stat().st_size > MAX_BYTES:
        raise ValueError(f"{source} rule pack exceeds {MAX_BYTES} bytes")
    raw = yaml.safe_load(path.read_text(encoding="utf-8-sig"))
    if not isinstance(raw, dict) or raw.get("version") != 1:
        raise ValueError(f"{source} rule pack version must be 1")
    allowed_file = {"version", "introduced_at", "provenance", "rules"}
    unknown_file = set(raw) - allowed_file
    if unknown_file:
        raise ValueError(f"{source} rule pack has unknown fields: {sorted(unknown_file)}")
    rows = raw.get("rules", [])
    if not isinstance(rows, list) or len(rows) > MAX_RULES:
        raise ValueError(f"{source} rules must be a list of at most {MAX_RULES}")
    default_date = clean_date(raw["introduced_at"]) if raw.get("introduced_at") else None
    default_provenance = clean_text(raw["provenance"], 300) if raw.get("provenance") else None
    allowed_rule = {
        "id", "title", "requirement", "severity", *SELECTORS,
        "oracle_guard", "introduced_at", "provenance",
    }
    seen: set[str] = set()
    result: list[Rule] = []
    for index, row in enumerate(rows, 1):
        if not isinstance(row, dict):
            raise ValueError(f"{source} rule {index} must be an object")
        unknown = set(row) - allowed_rule
        if unknown:
            raise ValueError(f"{source} rule {index} has unknown fields: {sorted(unknown)}")
        rule_id = clean_text(row.get("id"), 64).casefold()
        if not re.fullmatch(r"[a-z0-9_.-]+", rule_id):
            raise ValueError(f"{source} rule {index} id contains unsupported characters")
        if rule_id in seen:
            raise ValueError(f"{source} rules repeat id {rule_id}")
        seen.add(rule_id)
        selectors = {name: clean_selector(row.get(name), name) for name in SELECTORS}
        if (not selectors["story_all"] and not selectors["story_any"]
                and not selectors["path_all"] and not selectors["path_any"]):
            raise ValueError(f"{source} rule {rule_id} needs a positive predicate")
        severity = str(row.get("severity", "required_if_applicable")).strip().casefold()
        if severity not in VALID_SEVERITIES:
            raise ValueError(f"{source} rule {rule_id} has invalid severity {severity!r}")
        introduced = clean_date(row["introduced_at"]) if row.get("introduced_at") else default_date
        provenance = clean_text(row["provenance"], 300) if row.get("provenance") else default_provenance
        oracle = clean_text(row["oracle_guard"], 1500) if row.get("oracle_guard") else None
        result.append(Rule(
            id=rule_id,
            title=clean_text(row.get("title"), 160),
            requirement=clean_text(row.get("requirement"), 1500),
            severity=severity,
            story_all=selectors["story_all"],
            story_any=selectors["story_any"],
            story_none=selectors["story_none"],
            path_all=selectors["path_all"],
            path_any=selectors["path_any"],
            path_none=selectors["path_none"],
            oracle_guard=oracle,
            introduced_at=introduced,
            provenance=provenance,
            source=source,
        ))
    return result


def matches(rule: Rule, story: str, paths: list[str]) -> bool:
    story = story.casefold()
    paths = [path.replace("\\", "/").casefold() for path in paths]
    return (
        all(term in story for term in rule.story_all)
        and (not rule.story_any or any(term in story for term in rule.story_any))
        and all(term not in story for term in rule.story_none)
        and all(any(term in path for path in paths) for term in rule.path_all)
        and (not rule.path_any or any(term in path for term in rule.path_any for path in paths))
        and all(all(term not in path for path in paths) for term in rule.path_none)
    )


def evaluate_packs(
    packs: list[tuple[str, Path]], story: str, paths: list[str], cutoff: str | None
) -> tuple[list[Rule], list[dict[str, str]]]:
    if cutoff:
        cutoff = clean_date(cutoff)
    merged: dict[str, Rule] = {}
    notes: list[dict[str, str]] = []
    for source, path in packs:
        for rule in load_pack(path, source):
            if cutoff and (not rule.introduced_at or rule.introduced_at >= cutoff):
                reason = "no introduced_at provenance" if not rule.introduced_at else f"introduced_at {rule.introduced_at} is not before cutoff"
                notes.append({"rule_id": f"RULE-{rule.id.upper()}", "source": source, "reason": reason})
                continue
            merged[rule.id] = rule
    return [rule for rule in merged.values() if matches(rule, story, paths)], notes


def rule_candidates(rules: list[Rule]) -> list[Any]:
    return [contract_eval.Candidate(
        id=f"RULE-{rule.id.upper()}",
        kind=f"configured_{rule.severity}",
        text=" ".join(filter(None, [rule.title, rule.requirement, rule.oracle_guard or ""])),
    ) for rule in rules]


def evidence_paths(evidence: dict[str, Any]) -> list[str]:
    paths: list[str] = []
    for family in ("files", "asset_dependencies", "caller_dependencies"):
        for row in evidence.get(family, []):
            if row.get("path"):
                paths.append(str(row["path"]))
    return list(dict.fromkeys(paths))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--pack", action="append", type=Path, required=True,
                        help="Rule pack in precedence order; later packs override earlier ids")
    story = parser.add_mutually_exclusive_group(required=True)
    story.add_argument("--story")
    story.add_argument("--story-file", type=Path)
    parser.add_argument("--path", action="append", default=[])
    parser.add_argument("--evidence", type=Path)
    parser.add_argument("--knowledge-before")
    parser.add_argument("--fixture", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    story_text = args.story if args.story is not None else args.story_file.read_text(encoding="utf-8-sig")
    evidence = contract_eval.read_json(args.evidence) if args.evidence else {}
    paths = list(dict.fromkeys([*args.path, *evidence_paths(evidence)]))
    packs = [(f"pack-{index + 1}", path) for index, path in enumerate(args.pack)]
    matched, notes = evaluate_packs(packs, story_text, paths, args.knowledge_before)
    candidates = rule_candidates(matched)
    report: dict[str, Any] = {
        "packs": [str(path) for path in args.pack],
        "paths_considered": len(paths),
        "matched_rules": [
            {
                "id": candidate.id,
                "source": rule.source,
                "severity": rule.severity,
                "introduced_at": rule.introduced_at,
                "provenance": rule.provenance,
                "chars": len(candidate.text),
            }
            for rule, candidate in zip(matched, candidates)
        ],
        "historical_exclusions": notes,
    }
    if args.fixture:
        fixture = contract_eval.read_json(args.fixture)
        signals = contract_eval.compile_signals(fixture.get("signals", []))
        all_candidates = candidates
        if evidence:
            all_candidates = contract_eval.evidence_candidates(evidence, "") + candidates
        report["matched_rule_packet"] = contract_eval.packet_metrics(candidates, signals)
        report["combined_evidence_packet"] = contract_eval.packet_metrics(all_candidates, signals)
        report["unavailable_after_rules"] = contract_eval.signal_diagnostics(all_candidates, signals)
    rendered = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
