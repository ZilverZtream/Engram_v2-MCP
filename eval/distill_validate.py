"""End-to-end validation of the in-Engram distill_quality_gates capability.

Indexes the qg_corpus, runs distill_quality_gates on the real CodeRabbit findings
JSON (clustering + LLM-summarization via the configured LLM — deepseek/openrouter,
NOT Claude), then samples the resulting GENERIC rules via pre_push_audit. Proves
the product feature works on real data, independent of the Workflow distillation.

Uses a FRESH data_dir so it never touches the 29GB prod store.
"""
import io
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from engram_client import Engram  # noqa: E402

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")
CORPUS_DIR = os.path.join(DATA, "qg_corpus")
_PID_RE = re.compile(r"project[_ ]?id[\"']?\s*[:=]\s*[\"']?([0-9a-f-]{36})", re.I)


def main():
    eng = Engram(stderr_path=os.path.join(DATA, "distill_validate.stderr.log"))
    try:
        out = eng.tool("index_project", {
            "directory": CORPUS_DIR, "project_name": "ociusx_distill_validate",
            "project_type": "general", "wait": True, "dedupe_by_directory": False,
        })
        m = _PID_RE.search(out)
        if not m:
            print("INDEX FAILED:\n", out[:800]); return
        pid = m.group(1)
        print(f"project {pid}")

        print("running distill_quality_gates on qg_coderabbit.json (LLM=deepseek) ...", flush=True)
        r = eng.tool("distill_quality_gates", {
            "project_id": pid, "source_path": "qg_coderabbit.json",
            "source_type": "coderabbit", "batch_size": 50, "max_concurrent": 4,
        }, _cap=4000)
        print("RESULT:", r.strip()[:600])

        # sample the generic rules the tool produced
        for q in ["VB.NET method that reads a row from the database and returns a field",
                  "TypeScript function that calls an AJAX endpoint and updates the DOM"]:
            a = eng.tool("pre_push_audit", {"project_id": pid, "code": q, "top_k": 5}, _cap=3000)
            print(f"\n=== pre_push_audit: {q[:50]} ===")
            for line in a.splitlines():
                if line.strip().startswith("- ["):
                    print("  ", line.strip()[:200])
    finally:
        eng.close()


if __name__ == "__main__":
    main()
