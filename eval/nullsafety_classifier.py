#!/usr/bin/env python3
"""Round-7 P1-6: a COMMITTED, reproducible null-safety classifier over the
CodeRabbit finding corpus, so the ChangeVerifier ground-truth count is
independently reproducible (not a bare claim in a doc).

It applies the TIGHT null-safety rule to eval/data/qg_coderabbit.json, writes the
immutable labelled subset to eval/data/nullsafety_subset.json, and prints the
count plus a sha256 of the sealed subset. Re-running must reproduce the same
subset and hash for an unchanged corpus.

Usage: python eval/nullsafety_classifier.py [--write]
"""
import hashlib
import io
import json
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
CORPUS = REPO / "eval" / "data" / "qg_coderabbit.json"
OUT = REPO / "eval" / "data" / "nullsafety_subset.json"

# TIGHT null-safety rule (documented, exclusionary). A finding qualifies only
# when its message describes a null/Nothing dereference or a missing null guard
# — NOT the looser "nothing/null appears anywhere" families (false-success
# conditions, reload logic, serialization) that over-count.
TIGHT = re.compile(
    r"(null[- ]?reference|could be (null|nothing)|possible null|nullreferenceexception"
    r"|dereferenc|is nothing|==\s*null|!=\s*null|\bnull check\b|guard.*(null|nothing)"
    r"|may be null|potential null|check for (null|nothing))",
    re.I,
)


def classify():
    recs = json.load(io.open(CORPUS, encoding="utf-8"))
    if isinstance(recs, dict):
        recs = recs.get("findings") or recs.get("records") or []
    hits = []
    for r in recs:
        blob = " ".join(str(r.get(k, "")) for k in ("message", "title", "body"))
        if TIGHT.search(blob):
            hits.append(
                {
                    "file": r.get("file"),
                    "line": r.get("line"),
                    "source_pr": r.get("source_pr"),
                    "message": (r.get("message") or "")[:200],
                }
            )
    hits.sort(key=lambda h: (str(h["source_pr"]), str(h["file"]), h["line"] or 0))
    return recs, hits


def main():
    recs, hits = classify()
    prs = sorted({str(h["source_pr"]) for h in hits if h["source_pr"] is not None})
    payload = {
        "classifier": "tight-null-safety-v1",
        "corpus": "eval/data/qg_coderabbit.json",
        "corpus_findings": len(recs),
        "null_safety_count": len(hits),
        "prs": prs,
        "findings": hits,
    }
    blob = json.dumps(payload, ensure_ascii=False, sort_keys=True, indent=2)
    digest = hashlib.sha256(blob.encode("utf-8")).hexdigest()
    print(f"corpus findings : {len(recs)}")
    print(f"null-safety      : {len(hits)} across {len(prs)} PRs {prs}")
    print(f"subset sha256    : {digest}")
    if "--write" in sys.argv:
        io.open(OUT, "w", encoding="utf-8", newline="\n").write(blob + "\n")
        print(f"sealed subset -> {OUT.relative_to(REPO)}")


if __name__ == "__main__":
    main()
