"""Doc-13 Phase F staging: eval/_seal_suite.py — the sealed-blind-suite
protocol (owner 2026-09-01: ONE new sealed OciusX set). Mechanics only;
question authoring is the Phase-F work item itself.

  seal <suite.jsonl>    sha256 the corpus, write eval/data/SEALS/<name>.seal
                        (hash, sealed-at, row count — never the content),
                        and mark the corpus path in the seal as UNINSPECTED.
  verify <suite.jsonl>  re-hash; any drift = the seal is void (exit 2).
  retire <suite.jsonl>  first inspection: stamp inspected-at; the suite
                        drops into the dev/validation pool; a fresh set
                        must be authored and sealed to keep a blind gate.

Run scoring ONLY through verify-then-score wrappers; a suite whose seal
file says inspected is never again cited as blind."""
import hashlib
import io
import json
import os
import sys
import time

ROOT = r"C:/ai-projects/Engram-MCP_v2"
SEALS = os.path.join(ROOT, "eval", "data", "SEALS")


def seal_path(corpus: str) -> str:
    return os.path.join(SEALS, os.path.basename(corpus) + ".seal")


def sha(corpus: str) -> str:
    return hashlib.sha256(io.open(corpus, "rb").read()).hexdigest()


def main() -> int:
    if len(sys.argv) != 3 or sys.argv[1] not in ("seal", "verify", "retire"):
        print(__doc__)
        return 1
    verb, corpus = sys.argv[1], sys.argv[2]
    sp = seal_path(corpus)
    if verb == "seal":
        if os.path.exists(sp):
            print(f"refused: {sp} exists — a suite is sealed once")
            return 2
        os.makedirs(SEALS, exist_ok=True)
        rows = sum(1 for _ in io.open(corpus, encoding="utf-8"))
        rec = {
            "corpus": os.path.basename(corpus),
            "sha256": sha(corpus),
            "rows": rows,
            "sealed_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
            "inspected_at": None,
        }
        io.open(sp, "w", encoding="utf-8", newline="\n").write(
            json.dumps(rec, indent=2) + "\n"
        )
        print(f"sealed: {rec['corpus']} rows={rows} sha={rec['sha256'][:16]}…")
        return 0
    rec = json.loads(io.open(sp, encoding="utf-8").read())
    if rec["sha256"] != sha(corpus):
        print("SEAL VOID: corpus hash drift")
        return 2
    if verb == "verify":
        state = "BLIND" if rec["inspected_at"] is None else f"retired {rec['inspected_at']}"
        print(f"seal ok: {rec['corpus']} rows={rec['rows']} state={state}")
        return 0 if rec["inspected_at"] is None else 3
    if rec["inspected_at"] is not None:
        print(f"already retired {rec['inspected_at']}")
        return 0
    rec["inspected_at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
    io.open(sp, "w", encoding="utf-8", newline="\n").write(
        json.dumps(rec, indent=2) + "\n"
    )
    print(f"retired into the dev/validation pool: {rec['corpus']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
