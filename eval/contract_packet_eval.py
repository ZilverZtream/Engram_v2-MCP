"""Fast, deterministic evaluation for planning evidence and intake artifacts.

The production server is intentionally absent from this loop. A held-out fixture
describes reference signals and an accepted diff; saved MCP responses supply the
candidate evidence. This lets ranking and packet-size experiments run thousands
of times before a Rust rebuild or an agent replay is justified.

Reference fixtures belong under eval/data and must never be passed to the agent
that creates the candidate artifacts.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable


GUIDANCE_FIELDS = (
    "mechanism_role", "evidence_class", "impact_question", "exclusion_evidence_required"
)


@dataclass(frozen=True)
class Signal:
    id: str
    weight: float
    groups: tuple[tuple[re.Pattern[str], ...], ...]
    artifact_scopes: tuple[str, ...]


@dataclass(frozen=True)
class Candidate:
    id: str
    kind: str
    text: str


def read_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8-sig"))


def read_supplements(paths: list[Path]) -> list[Candidate]:
    candidates: list[Candidate] = []
    seen: set[str] = set()
    for path in paths:
        value = read_json(path)
        if not isinstance(value, dict) or value.get("version") != 1:
            raise ValueError(f"supplement {path} version must be 1")
        rows = value.get("candidates")
        if not isinstance(rows, list):
            raise ValueError(f"supplement {path} candidates must be a list")
        for index, row in enumerate(rows, 1):
            if not isinstance(row, dict):
                raise ValueError(f"supplement {path} candidate {index} must be an object")
            candidate_id = str(row.get("id", "")).strip()
            kind = str(row.get("kind", "supplement")).strip()
            text = str(row.get("text", "")).strip()
            if not candidate_id or not kind or not text:
                raise ValueError(f"supplement {path} candidate {index} has blank id, kind, or text")
            if candidate_id in seen:
                raise ValueError(f"supplements repeat candidate id {candidate_id}")
            seen.add(candidate_id)
            candidates.append(Candidate(candidate_id, kind, text))
    return candidates


def supplement_source_bindings(paths: list[Path]) -> list[dict[str, Any]]:
    """Prove proposed evidence is present in the staged production source."""
    bindings: list[dict[str, Any]] = []
    for path in paths:
        value = read_json(path)
        for index, row in enumerate(value.get("candidates", []), 1):
            candidate_id = str(row.get("id", f"candidate-{index}"))
            raw_source = str(row.get("source_path", "")).strip()
            needle = str(row.get("source_contains", "")).strip()
            source = Path(raw_source)
            if raw_source and not source.is_absolute():
                source = (path.parent / source).resolve()
            present = bool(raw_source and needle and source.is_file())
            if present:
                present = needle in source.read_text(encoding="utf-8", errors="replace")
            bindings.append({
                "candidate_id": candidate_id,
                "source_path": str(source) if raw_source else None,
                "source_contains": needle or None,
                "bound": present,
            })
    return bindings


def compile_signals(raw: Iterable[dict[str, Any]]) -> list[Signal]:
    signals: list[Signal] = []
    seen: set[str] = set()
    for item in raw:
        signal_id = str(item.get("id", "")).strip()
        if not signal_id or signal_id in seen:
            raise ValueError(f"signal id is blank or repeated: {signal_id!r}")
        seen.add(signal_id)
        raw_groups = item.get("groups") or []
        if not raw_groups or not all(isinstance(group, list) and group for group in raw_groups):
            raise ValueError(f"signal {signal_id} needs one or more non-empty regex groups")
        groups = tuple(
            tuple(re.compile(str(pattern), re.IGNORECASE | re.MULTILINE) for pattern in group)
            for group in raw_groups
        )
        signals.append(Signal(
            id=signal_id,
            weight=float(item.get("weight", 1.0)),
            groups=groups,
            artifact_scopes=tuple(str(scope) for scope in item.get("artifact_scopes", [])),
        ))
    return signals


def diff_paths(path: Path) -> list[str]:
    found: list[str] = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        match = re.match(r"^diff --git a/(.+?) b/(.+)$", line)
        if match and match.group(2) not in found:
            found.append(match.group(2))
    return found


def path_mentioned(text: str, path: str) -> bool:
    normalized = path.replace("\\", "/")
    basename = normalized.rsplit("/", 1)[-1]
    return normalized.casefold() in text.casefold() or bool(
        re.search(rf"(?<![A-Za-z0-9_.-]){re.escape(basename)}(?![A-Za-z0-9_.-])", text, re.IGNORECASE)
    )


def signal_covered(signal: Signal, text: str) -> bool:
    return all(any(pattern.search(text) for pattern in group) for group in signal.groups)


def artifact_metrics(fixture: dict[str, Any], signals: list[Signal], artifacts: Path) -> dict[str, Any]:
    configured = fixture.get("artifact_files") or [
        "feature-contract.md", "scenarios.md", "dossier.md", "intake-summary.md", "proposal.md"
    ]
    texts: dict[str, str] = {}
    missing_files: list[str] = []
    for name in configured:
        file_path = artifacts / str(name)
        if file_path.is_file():
            texts[str(name)] = file_path.read_text(encoding="utf-8", errors="replace")
        else:
            missing_files.append(str(name))
    combined = "\n".join(texts.values())

    accepted = list(fixture.get("accepted_paths") or [])
    if fixture.get("reference_diff"):
        reference = Path(str(fixture["reference_diff"]))
        if not reference.is_absolute():
            reference = artifacts.parent / reference
        accepted.extend(diff_paths(reference))
    excluded = {str(value).replace("\\", "/").casefold() for value in fixture.get("exclude_paths", [])}
    accepted = sorted({p.replace("\\", "/") for p in accepted if p.replace("\\", "/").casefold() not in excluded})
    mentioned = [path for path in accepted if path_mentioned(combined, path)]
    missed = [path for path in accepted if path not in mentioned]

    covered: list[str] = []
    missed_signals: list[str] = []
    weighted_hit = 0.0
    weighted_total = sum(signal.weight for signal in signals)
    for signal in signals:
        scoped = "\n".join(texts.get(name, "") for name in signal.artifact_scopes) if signal.artifact_scopes else combined
        if signal_covered(signal, scoped):
            covered.append(signal.id)
            weighted_hit += signal.weight
        else:
            missed_signals.append(signal.id)

    scenario_text = texts.get("scenarios.md", "")
    scenario_ids = sorted(set(re.findall(r"\b(?:AC|REG|EDGE|FORB)-\d+\b", scenario_text)))
    question_text = texts.get("feature-contract.md", "")
    blocking_questions = len(re.findall(r"^\|\s*\d+\s*\|.*?\|\s*BLOCKING\s*\|", question_text, re.MULTILINE))
    total_bytes = sum(len(value.encode("utf-8")) for value in texts.values())
    return {
        "missing_artifact_files": missing_files,
        "bytes": total_bytes,
        "accepted_paths": len(accepted),
        "accepted_paths_mentioned": len(mentioned),
        "accepted_path_recall": round(len(mentioned) / len(accepted), 4) if accepted else None,
        "missed_paths": missed,
        "signals": len(signals),
        "signals_covered": len(covered),
        "weighted_signal_recall": round(weighted_hit / weighted_total, 4) if weighted_total else None,
        "covered_signal_ids": covered,
        "missed_signal_ids": missed_signals,
        "scenario_ids": len(scenario_ids),
        "blocking_questions": blocking_questions,
    }


def value_text(value: Any) -> str:
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False, sort_keys=True)


def hydrated_rows(evidence: dict[str, Any], family: str) -> list[dict[str, Any]]:
    guidance = {
        str(entry.get("id")): entry
        for entry in evidence.get("row_guidance", {}).get("entries", [])
    }
    hydrated: list[dict[str, Any]] = []
    for original in evidence.get(family, []):
        row = dict(original)
        for field in GUIDANCE_FIELDS:
            reference = row.pop(f"{field}_ref", None)
            if reference is not None:
                entry = guidance.get(str(reference))
                if not entry or entry.get("field") != field:
                    raise ValueError(f"{family} row has unresolved {field}_ref {reference!r}")
                row[field] = entry.get("text", "")
        hydrated.append(row)
    return hydrated


def contract_requirements(evidence: dict[str, Any]) -> dict[str, str]:
    result: dict[str, str] = {}
    for item in evidence.get("contract_checkpoint", {}).get("hard_items", []):
        result[str(item.get("id"))] = str(item.get("requirement", ""))
    for obligation in evidence.get("cross_cutting_obligations", []):
        items = list(obligation.get("contract_items", []))
        items.extend(obligation.get("advisory_contract_items", []))
        for item in items:
            result[str(item.get("check_id"))] = str(item.get("requirement", ""))
    for hypothesis in evidence.get("component_hypotheses", []):
        items = list(hypothesis.get("contract_evidence_items", []))
        items.extend(hypothesis.get("advisory_contract_evidence", []))
        for item in items:
            result[str(item.get("evidence_id"))] = str(item.get("requirement", ""))
    return {key: value for key, value in result.items() if key and key != "None"}


def evidence_payload_metrics(evidence: dict[str, Any]) -> dict[str, Any]:
    checkpoint = evidence.get("contract_checkpoint", {})
    receipt = checkpoint.get("receipt", {})
    checkpoint_ids = (
        list(checkpoint.get("obligation_check_ids", []))
        + list(checkpoint.get("hypothesis_evidence_ids", []))
        + list(checkpoint.get("configured_rule_ids", []))
    )
    checkpoint_digest = hashlib.sha256(
        "".join(f"{item_id}\n" for item_id in checkpoint_ids).encode("utf-8")
    ).hexdigest()
    row_ids: list[tuple[str, str]] = []
    for family in ("files", "asset_dependencies", "caller_dependencies"):
        for row in hydrated_rows(evidence, family):
            row_id = row.get("row_id")
            path = row.get("path")
            if row_id and path:
                row_ids.append((str(row_id), str(path)))
    row_digest = hashlib.sha256(
        "".join(f"{row_id}\0{path}\n" for row_id, path in row_ids).encode("utf-8")
    ).hexdigest()
    requirements = contract_requirements(evidence)
    hard_count = len(checkpoint.get("hard_items", []))
    return {
        "compact_json_chars": len(json.dumps(evidence, ensure_ascii=False, separators=(",", ":"))),
        "rows": {
            family: len(evidence.get(family, []))
            for family in ("files", "asset_dependencies", "caller_dependencies")
        },
        "row_guidance_entries": len(evidence.get("row_guidance", {}).get("entries", [])),
        "contract_requirements": len(requirements),
        "hard_items": hard_count,
        "advisory_items": checkpoint.get("advisory_items_total"),
        "hard_share": round(hard_count / len(requirements), 4) if requirements else None,
        "contract_receipt_valid": receipt.get("receipt_id") == f"sha256:{checkpoint_digest}",
        "reconciliation_receipt_valid": evidence.get("reconciliation", {}).get("receipt_id") == f"sha256:{row_digest}",
    }


def compare_evidence_payloads(
    baseline: dict[str, Any], candidate: dict[str, Any], max_hard_items: int = 24
) -> dict[str, Any]:
    baseline_metrics = evidence_payload_metrics(baseline)
    candidate_metrics = evidence_payload_metrics(candidate)
    row_equality = {
        family: hydrated_rows(baseline, family) == hydrated_rows(candidate, family)
        for family in ("files", "asset_dependencies", "caller_dependencies")
    }
    baseline_requirements = contract_requirements(baseline)
    candidate_requirements = contract_requirements(candidate)
    baseline_chars = baseline_metrics["compact_json_chars"]
    candidate_chars = candidate_metrics["compact_json_chars"]
    gates = {
        "all_rows_lossless": all(row_equality.values()),
        "contract_requirements_lossless": baseline_requirements == candidate_requirements,
        "contract_receipt_valid": candidate_metrics["contract_receipt_valid"],
        "reconciliation_receipt_valid": candidate_metrics["reconciliation_receipt_valid"],
        "hard_item_budget": candidate_metrics["hard_items"] <= max_hard_items,
        "response_not_larger": candidate_chars <= baseline_chars,
    }
    return {
        "baseline": baseline_metrics,
        "candidate": candidate_metrics,
        "saved_chars": baseline_chars - candidate_chars,
        "saved_percent": round(100 * (1 - candidate_chars / baseline_chars), 2) if baseline_chars else None,
        "row_exact_equality_after_hydration": row_equality,
        "gates": gates,
        "ready_for_single_agent_validation": all(gates.values()),
    }


def evidence_candidates(evidence: dict[str, Any], matrix_text: str) -> list[Candidate]:
    candidates: list[Candidate] = []
    if str(evidence.get("story", "")).strip():
        candidates.append(Candidate(id="STORY", kind="approved_story", text=str(evidence["story"])))
    for item in evidence.get("contract_checkpoint", {}).get("hard_items", []):
        candidates.append(Candidate(
            id=str(item.get("id")),
            kind=f"hard_{item.get('kind', 'contract')}",
            text=value_text(item),
        ))
    for obligation in evidence.get("cross_cutting_obligations", []):
        prefix = " ".join(value_text(obligation.get(key, "")) for key in (
            "obligation", "trigger", "candidate_mechanism_roles"
        ))
        items = list(obligation.get("contract_items", []))
        items.extend(obligation.get("advisory_contract_items", []))
        for item in items:
            candidates.append(Candidate(
                id=str(item.get("check_id")), kind="obligation",
                text=prefix + " " + value_text(item),
            ))
    for hypothesis in evidence.get("component_hypotheses", []):
        prefix = " ".join(value_text(hypothesis.get(key, "")) for key in (
            "responsibility", "verify_against", "decision", "required_surface_categories", "concrete_surfaces"
        ))
        items = list(hypothesis.get("contract_evidence_items", []))
        items.extend(hypothesis.get("advisory_contract_evidence", []))
        for item in items:
            candidates.append(Candidate(
                id=str(item.get("evidence_id")), kind="hypothesis",
                text=prefix + " " + value_text(item),
            ))
    for family, kind in (("files", "primary"), ("asset_dependencies", "asset"), ("caller_dependencies", "caller")):
        for row in hydrated_rows(evidence, family):
            row_id = str(row.get("row_id") or f"{kind}:{row.get('path', '')}")
            candidates.append(Candidate(id=row_id, kind=kind, text=value_text(row)))

    for category in evidence.get("boundary_audit", {}).get("categories", []):
        boundary = str(category.get("boundary", "")).strip()
        if boundary:
            candidates.append(Candidate(
                id=f"BOUNDARY-{boundary.upper()}", kind="boundary", text=value_text(category)
            ))
    for rule in evidence.get("configured_contract_rules", {}).get("rules", []):
        rule_id = str(rule.get("id", "")).strip()
        if rule_id:
            candidates.append(Candidate(id=rule_id, kind="configured_rule", text=value_text(rule)))
    for rule in evidence.get("applicable_repository_rules", {}).get("rules", []):
        rule_id = str(rule.get("rule_id") or rule.get("id") or "").strip()
        if rule_id:
            candidates.append(Candidate(
                id=f"REPO-RULE-{rule_id}", kind="repository_rule", text=value_text(rule)
            ))

    matrix_matches = list(re.finditer(
        r"^- \*\*(TM-INTENT-\d+)\*\*.*?(?=^- \*\*TM-INTENT-|^Historical knowledge cutoff:|\Z)",
        matrix_text,
        re.IGNORECASE | re.MULTILINE | re.DOTALL,
    ))
    for match in matrix_matches:
        candidates.append(Candidate(id=match.group(1).upper(), kind="test_intent", text=match.group(0)))

    unique: dict[str, Candidate] = {}
    for candidate in candidates:
        if candidate.id and candidate.id not in unique:
            unique[candidate.id] = candidate
    return list(unique.values())


def packet_metrics(candidates: list[Candidate], signals: list[Signal]) -> dict[str, Any]:
    combined = "\n".join(candidate.text for candidate in candidates)
    covered = [signal.id for signal in signals if signal_covered(signal, combined)]
    total = sum(signal.weight for signal in signals)
    hit = sum(signal.weight for signal in signals if signal.id in covered)
    return {
        "items": len(candidates),
        "chars": sum(len(candidate.text) for candidate in candidates),
        "weighted_signal_recall": round(hit / total, 4) if total else None,
        "covered_signal_ids": covered,
        "missed_signal_ids": [signal.id for signal in signals if signal.id not in covered],
    }


def candidate_masks(candidates: list[Candidate], signals: list[Signal]) -> list[tuple[int, ...]]:
    masks: list[tuple[int, ...]] = []
    for candidate in candidates:
        per_signal: list[int] = []
        for signal in signals:
            mask = 0
            for index, group in enumerate(signal.groups):
                if any(pattern.search(candidate.text) for pattern in group):
                    mask |= 1 << index
            per_signal.append(mask)
        masks.append(tuple(per_signal))
    return masks


def signal_diagnostics(candidates: list[Candidate], signals: list[Signal]) -> list[dict[str, Any]]:
    """Explain evidence gaps without asking an agent to rediscover them."""
    masks = candidate_masks(candidates, signals)
    diagnostics: list[dict[str, Any]] = []
    for signal_index, signal in enumerate(signals):
        available_mask = 0
        for mask in masks:
            available_mask |= mask[signal_index]
        required_mask = (1 << len(signal.groups)) - 1
        if available_mask == required_mask:
            continue
        missing_groups = []
        for group_index, patterns in enumerate(signal.groups):
            if available_mask & (1 << group_index):
                continue
            missing_groups.append({
                "group": group_index + 1,
                "patterns": [pattern.pattern for pattern in patterns],
            })
        diagnostics.append({
            "signal_id": signal.id,
            "weight": signal.weight,
            "available_groups": available_mask.bit_count(),
            "required_groups": len(signal.groups),
            "missing_groups": missing_groups,
        })
    return diagnostics


def covered_from_state(state: list[int], signals: list[Signal]) -> tuple[float, list[str]]:
    score = 0.0
    covered: list[str] = []
    for index, signal in enumerate(signals):
        required = (1 << len(signal.groups)) - 1
        if state[index] == required:
            score += signal.weight
            covered.append(signal.id)
    return score, covered


def progress_from_state(state: list[int], signals: list[Signal]) -> float:
    """Score partial group coverage so greedy selection can complete multi-row signals."""
    return sum(
        signal.weight * state[index].bit_count() / len(signal.groups)
        for index, signal in enumerate(signals)
    )


def add_mask(state: list[int], mask: tuple[int, ...]) -> list[int]:
    return [left | right for left, right in zip(state, mask)]


def optimize_packets(
    candidates: list[Candidate], signals: list[Signal], budgets: list[int], iterations: int, seed: int
) -> dict[str, Any]:
    masks = candidate_masks(candidates, signals)
    rng = random.Random(seed)
    total_weight = sum(signal.weight for signal in signals)
    results: dict[str, Any] = {}
    for budget in budgets:
        budget = min(max(1, budget), len(candidates))
        state = [0] * len(signals)
        selected: list[int] = []
        remaining = set(range(len(candidates)))
        while remaining and len(selected) < budget:
            current_score, _ = covered_from_state(state, signals)
            current_progress = progress_from_state(state, signals)
            best = max(
                remaining,
                key=lambda index: (
                    covered_from_state(add_mask(state, masks[index]), signals)[0] - current_score,
                    progress_from_state(add_mask(state, masks[index]), signals) - current_progress,
                    -len(candidates[index].text),
                    candidates[index].id,
                ),
            )
            next_state = add_mask(state, masks[best])
            if progress_from_state(next_state, signals) <= current_progress:
                break
            selected.append(best)
            remaining.remove(best)
            state = next_state

        best_selected = tuple(selected)
        best_score, best_covered = covered_from_state(state, signals)
        best_progress = progress_from_state(state, signals)
        best_chars = sum(len(candidates[i].text) for i in best_selected)
        for _ in range(iterations):
            if not best_selected:
                break
            selected_set = set(best_selected)
            outside = [index for index in range(len(candidates)) if index not in selected_set]
            if not outside:
                break
            trial_list = list(best_selected)
            if len(trial_list) < budget and rng.random() < 0.35:
                trial_list.append(rng.choice(outside))
            else:
                trial_list[rng.randrange(len(trial_list))] = rng.choice(outside)
            trial = tuple(dict.fromkeys(trial_list))
            trial_state = [0] * len(signals)
            for index in trial:
                trial_state = add_mask(trial_state, masks[index])
            trial_score, trial_covered = covered_from_state(trial_state, signals)
            trial_progress = progress_from_state(trial_state, signals)
            trial_chars = sum(len(candidates[i].text) for i in trial)
            if (trial_score, trial_progress, -trial_chars) > (
                best_score, best_progress, -best_chars
            ):
                best_selected, best_score, best_covered = trial, trial_score, trial_covered
                best_progress, best_chars = trial_progress, trial_chars
        selected_candidates = [candidates[index] for index in best_selected]
        results[str(budget)] = {
            "budget": budget,
            "selected_count": len(selected_candidates),
            "weighted_signal_recall": round(best_score / total_weight, 4) if total_weight else None,
            "covered_signal_ids": best_covered,
            "selected": [
                {"id": candidate.id, "kind": candidate.kind, "chars": len(candidate.text)}
                for candidate in selected_candidates
            ],
            "packet_chars": best_chars,
        }
    return results


def optimize_packets_by_chars(
    candidates: list[Candidate], signals: list[Signal], budgets: list[int], iterations: int, seed: int
) -> dict[str, Any]:
    """Find a compact evidence packet under actual payload budgets.

    Count budgets are useful for workflow gates, but MCP cost and model attention
    follow characters. This optimizer makes that tradeoff testable offline.
    """
    masks = candidate_masks(candidates, signals)
    rng = random.Random(seed ^ 0xC0FFEE)
    total_weight = sum(signal.weight for signal in signals)
    results: dict[str, Any] = {}
    for budget in budgets:
        if budget < 1:
            raise ValueError("character budgets must be positive")
        selected: list[int] = []
        state = [0] * len(signals)
        used = 0
        remaining = set(range(len(candidates)))
        while remaining:
            current_score, _ = covered_from_state(state, signals)
            current_progress = progress_from_state(state, signals)
            feasible = [
                index for index in remaining
                if used + len(candidates[index].text) <= budget
            ]
            if not feasible:
                break
            ranked = []
            for index in feasible:
                next_state = add_mask(state, masks[index])
                score_gain = covered_from_state(next_state, signals)[0] - current_score
                progress_gain = progress_from_state(next_state, signals) - current_progress
                chars = max(1, len(candidates[index].text))
                ranked.append((score_gain / chars, progress_gain / chars, score_gain,
                               progress_gain, -chars, candidates[index].id, index, next_state))
            best = max(ranked)
            if best[1] <= 0:
                break
            index, next_state = best[-2], best[-1]
            selected.append(index)
            remaining.remove(index)
            used += len(candidates[index].text)
            state = next_state

        best_selected = tuple(selected)
        best_score, best_covered = covered_from_state(state, signals)
        best_progress = progress_from_state(state, signals)
        best_chars = used
        for _ in range(iterations):
            selected_set = set(best_selected)
            outside = [index for index in range(len(candidates)) if index not in selected_set]
            if not outside:
                break
            trial = list(best_selected)
            add_only = rng.random() < 0.3 or not trial
            if add_only:
                trial.append(rng.choice(outside))
            else:
                trial[rng.randrange(len(trial))] = rng.choice(outside)
            trial = list(dict.fromkeys(trial))
            trial_chars = sum(len(candidates[index].text) for index in trial)
            if trial_chars > budget:
                continue
            trial_state = [0] * len(signals)
            for index in trial:
                trial_state = add_mask(trial_state, masks[index])
            trial_score, trial_covered = covered_from_state(trial_state, signals)
            trial_progress = progress_from_state(trial_state, signals)
            if (trial_score, trial_progress, -trial_chars) > (
                best_score, best_progress, -best_chars
            ):
                best_selected = tuple(trial)
                best_score, best_covered = trial_score, trial_covered
                best_progress, best_chars = trial_progress, trial_chars
        selected_candidates = [candidates[index] for index in best_selected]
        results[str(budget)] = {
            "char_budget": budget,
            "selected_count": len(selected_candidates),
            "weighted_signal_recall": round(best_score / total_weight, 4) if total_weight else None,
            "covered_signal_ids": best_covered,
            "selected": [
                {"id": candidate.id, "kind": candidate.kind, "chars": len(candidate.text)}
                for candidate in selected_candidates
            ],
            "packet_chars": best_chars,
            "unused_chars": budget - best_chars,
        }
    return results


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--evidence", type=Path)
    parser.add_argument("--baseline-evidence", type=Path)
    parser.add_argument("--supplement", action="append", type=Path, default=[])
    parser.add_argument("--matrix", type=Path)
    parser.add_argument("--budgets", default="8,12,16,24")
    parser.add_argument("--char-budgets", default="4000,8000,12000,20000")
    parser.add_argument("--iterations", type=int, default=10_000)
    parser.add_argument("--seed", type=int, default=2033)
    parser.add_argument("--min-predicted-recall", type=float, default=0.9)
    parser.add_argument("--min-predicted-gain", type=float, default=0.05)
    parser.add_argument("--require-ready", action="store_true")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    started = time.perf_counter()
    fixture = read_json(args.fixture)
    if fixture.get("version") != 1:
        raise ValueError("fixture version must be 1")
    if fixture.get("reference_diff"):
        reference = Path(str(fixture["reference_diff"]))
        if not reference.is_absolute():
            fixture["reference_diff"] = str((args.fixture.parent / reference).resolve())
    signals = compile_signals(fixture.get("signals", []))
    report: dict[str, Any] = {
        "fixture": str(args.fixture),
        "fixture_sha256": hashlib.sha256(args.fixture.read_bytes()).hexdigest(),
        "artifacts": artifact_metrics(fixture, signals, args.artifacts),
    }
    if args.evidence and args.matrix:
        evidence = read_json(args.evidence)
        report["evidence_payload"] = evidence_payload_metrics(evidence)
        if args.baseline_evidence:
            report["evidence_comparison"] = compare_evidence_payloads(
                read_json(args.baseline_evidence), evidence
            )
        baseline_candidates = evidence_candidates(
            evidence, args.matrix.read_text(encoding="utf-8-sig", errors="replace")
        )
        supplements = read_supplements(args.supplement)
        supplement_bindings = supplement_source_bindings(args.supplement)
        candidates = baseline_candidates + supplements
        budgets = sorted({int(value) for value in args.budgets.split(",") if value.strip()})
        char_budgets = sorted({int(value) for value in args.char_budgets.split(",") if value.strip()})
        report["packet_optimizer"] = {
            "candidate_count": len(candidates),
            "hard_packet": packet_metrics(
                [candidate for candidate in candidates if candidate.kind.startswith("hard_")],
                signals,
            ),
            "iterations_per_budget": args.iterations,
            "budgets": optimize_packets(candidates, signals, budgets, args.iterations, args.seed),
            "character_budgets": optimize_packets_by_chars(
                candidates, signals, char_budgets, args.iterations, args.seed
            ),
            "unavailable_signal_diagnostics": signal_diagnostics(candidates, signals),
        }
        before = packet_metrics(baseline_candidates, signals)
        after = packet_metrics(candidates, signals)
        char_results = report["packet_optimizer"]["character_budgets"]
        best_bounded_recall = max(
            (result["weighted_signal_recall"] or 0.0 for result in char_results.values()),
            default=0.0,
        )
        before_recall = before["weighted_signal_recall"] or 0.0
        after_recall = after["weighted_signal_recall"] or 0.0
        impact = {
            "supplement_candidates": len(supplements),
            "before": before,
            "after": after,
            "weighted_recall_gain": round(after_recall - before_recall, 4),
            "newly_covered_signal_ids": sorted(
                set(after["covered_signal_ids"]) - set(before["covered_signal_ids"])
            ),
        }
        report["supplement_impact"] = impact
        report["supplement_source_bindings"] = supplement_bindings
        payload_ready = report.get("evidence_comparison", {}).get(
            "ready_for_single_agent_validation", not args.baseline_evidence
        )
        decision_gates = {
            "payload_integrity": bool(payload_ready),
            "bounded_packet_recall": best_bounded_recall >= args.min_predicted_recall,
            "material_predicted_gain": impact["weighted_recall_gain"] >= args.min_predicted_gain,
            "supplements_source_bound": all(
                binding["bound"] for binding in supplement_bindings
            ),
        }
        report["validation_decision"] = {
            "ready": all(decision_gates.values()),
            "gates": decision_gates,
            "best_bounded_packet_recall": best_bounded_recall,
            "minimum_predicted_recall": args.min_predicted_recall,
            "minimum_predicted_gain": args.min_predicted_gain,
            "instruction": "Run one paid agent validation only when ready is true; batch more production changes otherwise.",
        }
    report["elapsed_ms"] = round((time.perf_counter() - started) * 1000, 3)
    rendered = json.dumps(report, indent=2, ensure_ascii=False) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    if args.require_ready and not report.get("validation_decision", {}).get("ready", False):
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
