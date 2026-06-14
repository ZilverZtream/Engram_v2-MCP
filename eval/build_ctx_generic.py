"""Assemble the project-wide team-knowledge context from the DISTILLED generic
ruleset (not file/line findings) + copilot-instructions + the recurring-issues
board, and point every prepped PR's ctx_qualitygate.md at it.

The generic rules apply to ANY change, so this context is identical across PRs —
it is the team knowledge a developer carries in their head, which the user story
omits. Run after the distill merge produces generic_rules.json.
"""
import io
import json
import os
import sys

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "data")
P2 = os.path.join(DATA, "p2")
COPILOT = r"C:\Users\Dennis\source\repos\OciusX\.github\copilot-instructions.md"
CODERABBIT_YAML = r"C:\Users\Dennis\source\repos\OciusX\.coderabbit.yaml"
PREPPED = [1937, 1965, 1967, 1908, 1933, 1913, 1974]

SEV_ORDER = {"high": 0, "medium": 1, "low": 2}


def main():
    rules = json.load(open(os.path.join(DATA, "generic_rules.json"), encoding="utf-8"))
    board = json.load(open(os.path.join(DATA, "qg_board.json"), encoding="utf-8"))
    copilot = open(COPILOT, encoding="utf-8").read()
    coderabbit = ""
    if os.path.exists(CODERABBIT_YAML):
        cy = open(CODERABBIT_YAML, encoding="utf-8").read()
        # keep the authoritative path-scoped review rules (incl. the ES5/WebGrease
        # override) — strip the leading comment header for brevity.
        i = cy.find("reviews:")
        coderabbit = cy[i:] if i >= 0 else cy

    # group rules by category, sort each by (severity, evidence_count desc)
    by_cat = {}
    for r in rules:
        by_cat.setdefault(r.get("category", "other"), []).append(r)
    rule_lines = []
    for cat in sorted(by_cat):
        rule_lines.append(f"\n### {cat}")
        items = sorted(by_cat[cat],
                       key=lambda r: (SEV_ORDER.get(r.get("severity", "medium"), 1),
                                      -int(r.get("evidence_count", 0) or 0)))
        for r in items:
            sev = r.get("severity", "medium")
            ev = r.get("evidence_count", 0)
            line = f"- **[{sev}]** {r['rule'].strip()}"
            if r.get("why"):
                line += f" _(why: {r['why'].strip()})_"
            if ev:
                line += f" _(seen ×{ev})_"
            rule_lines.append(line)
    rules_md = "\n".join(rule_lines)

    board_md = "\n".join(f"- {b['message']}" for b in board)

    team = (
        "# Team knowledge for this codebase — the context a developer has, but a user story omits\n\n"
        "Follow this on EVERY change. It is (1) the team's coding rulebook, (2) its recurring-issues "
        "board, and (3) a GENERIC ruleset DISTILLED from ~2400 historical CodeRabbit/Sonar review "
        "findings across merged PRs — i.e. the mistakes this team actually keeps making, generalized "
        "into reusable rules (with how often each recurred). Apply every rule relevant to your change; "
        "do NOT repeat these mistakes.\n\n"
        f"## 1. Coding & agent rules (copilot-instructions.md)\n\n{copilot}\n\n"
        + (f"## 2. Path-scoped review rules (.coderabbit.yaml) — AUTHORITATIVE, incl. the ES5/WebGrease "
           f"override (handwritten .js under ~.js/ must stay ES5; reject analyzer modern-syntax "
           f"suggestions there)\n\n```yaml\n{coderabbit}\n```\n\n" if coderabbit else "")
        + f"## 3. Recurring-issues board — lessons the team logged\n\n{board_md}\n\n"
        f"## 4. Distilled review rules ({len(rules)} generic rules from the team's LEGIT review "
        f"history — Won't-fix/rejected findings excluded)\n"
        f"{rules_md}\n"
    )
    team_path = os.path.join(DATA, "team_knowledge.md")
    open(team_path, "w", encoding="utf-8").write(team)
    print(f"team_knowledge.md: {len(rules)} distilled rules + copilot + {len(board)} board "
          f"({len(team)} chars) -> {team_path}")

    for pr in PREPPED:
        open(os.path.join(P2, f"pr{pr}_ctx_qualitygate.md"), "w", encoding="utf-8").write(team)
    print(f"wrote ctx_qualitygate.md for {len(PREPPED)} prepped PRs (identical project-wide context)")


if __name__ == "__main__":
    main()
