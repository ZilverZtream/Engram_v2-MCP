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


def evidence_candidates(evidence: dict[str, Any], matrix_text: str) -> list[Candidate]:
    candidates: list[Candidate] = []
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
    guidance = {
        str(entry.get("id")): entry
        for entry in evidence.get("row_guidance", {}).get("entries", [])
    }
    for family, kind in (("files", "primary"), ("asset_dependencies", "asset"), ("caller_dependencies", "caller")):
        for row in evidence.get(family, []):
            row = dict(row)
            for field in (
                "mechanism_role", "evidence_class", "impact_question", "exclusion_evidence_required"
            ):
                reference = row.pop(f"{field}_ref", None)
                if reference in guidance:
                    row[field] = guidance[reference].get("text", "")
            row_id = str(row.get("row_id") or f"{kind}:{row.get('path', '')}")
            candidates.append(Candidate(id=row_id, kind=kind, text=value_text(row)))

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


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--evidence", type=Path)
    parser.add_argument("--matrix", type=Path)
    parser.add_argument("--budgets", default="8,12,16,24")
    parser.add_argument("--iterations", type=int, default=10_000)
    parser.add_argument("--seed", type=int, default=2033)
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
        candidates = evidence_candidates(read_json(args.evidence), args.matrix.read_text(encoding="utf-8-sig", errors="replace"))
        budgets = sorted({int(value) for value in args.budgets.split(",") if value.strip()})
        report["packet_optimizer"] = {
            "candidate_count": len(candidates),
            "hard_packet": packet_metrics(
                [candidate for candidate in candidates if candidate.kind.startswith("hard_")],
                signals,
            ),
            "iterations_per_budget": args.iterations,
            "budgets": optimize_packets(candidates, signals, budgets, args.iterations, args.seed),
        }
    report["elapsed_ms"] = round((time.perf_counter() - started) * 1000, 3)
    rendered = json.dumps(report, indent=2, ensure_ascii=False) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
