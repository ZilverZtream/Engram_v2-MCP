"""Build the leak-safe rules-armed dossier for an authoring experiment.

Reproduces get_change_set's '## Review rules for this change' section
against the PROD rule corpus, but time-bounded: only clusters whose PR
references ALL predate the subject PR — exactly the rule bank that
existed when the team authored it. Appends the section to the prep'd
dossier as pr<id>_dossier_armed.md; the unarmed original stays as the
control arm.

Needs the prod daemon reachable (engram_drive spawns one) — do NOT run
while phase2_prep holds the eval environment.

Usage: python eval/arm_dossier.py <pr_id>
"""
import io
import json
import re
import subprocess
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
import os

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
PID = "664003e4-2ac5-4902-a0ce-6382b6026fe5"


def drive(tool, args, cap):
    r = subprocess.run([sys.executable, os.path.join(ROOT, "target", "engram_drive.py"),
                        "tool", tool, json.dumps(args), str(cap)],
                       capture_output=True, text=True, timeout=300,
                       encoding="utf-8", errors="replace")
    return r.stdout


def main():
    pr_id = int(sys.argv[1])
    man = json.load(open(os.path.join(HERE, "data", "p2", f"pr{pr_id}.json"), encoding="utf-8"))
    st = man["story"]
    q = " ".join(filter(None, [st.get("title", ""), st.get("description", "")[:400]]))

    dossier_path = os.path.join(HERE, "data", "p2", f"pr{pr_id}_dossier.md")
    dossier = io.open(dossier_path, encoding="utf-8").read()
    cand_paths = set(p.lower() for p in re.findall(r"`([^`]+\.\w{2,7})`", dossier))

    out = drive("search_memory", {"project_id": PID, "query": q[:300],
                                  "namespace": "antipattern", "max_results": 20}, 8000)
    doc_ids = list(dict.fromkeys(re.findall(r"doc_id: (\w+)", out)))

    rows = []
    seen_instruction = set()
    for did in doc_ids:
        doc = drive("get_chunk", {"project_id": PID, "doc_id": did, "namespace": "antipattern"}, 2200)
        refs = [int(x) for x in re.findall(r"#(\d{3,5})", doc.split("PR references:")[-1][:400])]
        if not refs or max(refs) >= pr_id:
            continue
        body = doc.split("\n\n", 1)[-1] if "\n\n" in doc else doc
        instruction = next((l.strip() for l in body.splitlines()
                            if l.strip() and not l.startswith(("path:", "doc_id:", "namespace:",
                                                               "language:", "lines:", "active_"))), "")
        if len(instruction) < 12 or instruction.lower() in seen_instruction:
            continue
        seen_instruction.add(instruction.lower())
        mpath = re.search(r"path: (\S+)", doc)
        glob = mpath.group(1) if mpath else ""
        fixrate = re.search(r"Fix rate: (\d+%)", doc)
        prefix = glob.lower().split("/**")[0].lstrip("/")
        fam = bool(prefix) and any(prefix in c or c.startswith(prefix) for c in cand_paths)
        rows.append((fam, glob, instruction, fixrate.group(1) if fixrate else "?", max(refs)))

    rows.sort(key=lambda r: (not r[0],))
    rows = rows[:10]
    print(f"PR{pr_id}: {len(rows)} leak-safe rules")
    sec = ["\n## Review rules for this change (distilled from this repo's past code reviews)",
           "Reviewers flagged these issue classes repeatedly in past merged PRs. Write the "
           "code so they never fire; each one caught late costs a review round-trip:"]
    for fam, glob, ins, fr, ref in rows:
        sec.append(f"- {'▲ ' if fam else ''}`{glob}`: {ins} (fix rate {fr})")
        print(" ", "▲" if fam else " ", ins[:100])
    armed_path = os.path.join(HERE, "data", "p2", f"pr{pr_id}_dossier_armed.md")
    io.open(armed_path, "w", encoding="utf-8", newline="\n").write(dossier + "\n".join(sec) + "\n")
    print(f"wrote {armed_path}")


if __name__ == "__main__":
    main()
