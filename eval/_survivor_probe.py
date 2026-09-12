"""Retrieval-survivor diagnosis (owner 2026-09-02): dump the FULL ask_codebase
report for each surviving golden/causal row against the production OciusX
index — one eval-server session, reports to the scratchpad for gap analysis.

Usage: python eval/_survivor_probe.py <out_dir>
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import engram_client as ec  # noqa: E402

PID = "5a35e8e0-d37a-41b3-a250-a26957e7aedb"
FAILS = [
    "ox_exact_6", "ox_usage_2", "ox_usage_3", "ox_usage_5", "ox_multi_1",
    "ox_multi_2", "ox_multi_3", "ox_multi_4", "ox_rationale_2",
    "ox_compound_1", "ox_bug_1", "ox_bug_2", "ox_causal_3", "ox_causal_12",
    "ox_causal_17", "ox_causal_19",
]


def main():
    out_dir = sys.argv[1]
    os.makedirs(out_dir, exist_ok=True)
    corpus = {}
    for c in ("ask_golden_ociusx.jsonl", "ask_causal_ociusx.jsonl"):
        for ln in open(os.path.join(HERE, "data", c), encoding="utf-8"):
            r = json.loads(ln)
            corpus[r["id"]] = r
    eng = ec.Engram()
    try:
        for rid in FAILS:
            q = corpus[rid]["question"]
            print(f"{rid}: {q[:70]}", flush=True)
            out = eng.tool("ask_codebase", {"project_id": PID, "question": q})
            open(
                os.path.join(out_dir, f"surv_{rid}.txt"), "w", encoding="utf-8"
            ).write(out)
    finally:
        eng.close()
    print("survivor probes complete", flush=True)


if __name__ == "__main__":
    main()
