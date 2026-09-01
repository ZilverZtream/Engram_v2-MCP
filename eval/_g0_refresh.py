"""Phase G0 (doc 13 item 7; owner 2026-09-01: fresh substrate): re-index the
15 canonical A/B PRs at their base commits with the CURRENT (r73) binary,
rebuild the leak-free agent snapshots, mark them read-only, and record an
mtime manifest that scoring verifies (standing mandate: snapshot trees are
filesystem read-only and mtime-checked before scoring).

Sequential by design: every eval index shares the production data_dir's
single-writer lock. Requires NO other engram_server running.

Usage: python eval/_g0_refresh.py [--only 1933,1908]
"""
import json
import os
import stat
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import engram_client as ec  # noqa: E402
import run_phase1 as rp  # noqa: E402

P2 = os.path.join(HERE, "data", "p2")
SNAP_ROOT = r"C:/ai-projects/_p2snap"
IMAP_PATH = os.path.join(P2, "index_map.json")
MANIFEST_DIR = os.path.join(P2, "snap_manifests")


def canonical_ids():
    v = json.load(open(os.path.join(P2, "_ab15_final_verdicts.json"), encoding="utf-8"))
    return sorted({str(x["pr"]) for x in v["verdicts"]})


def freeze_snapshot(dest):
    """Read-only every file + record an mtime/size manifest."""
    manifest = {}
    for root, _dirs, files in os.walk(dest):
        for f in files:
            p = os.path.join(root, f)
            os.chmod(p, stat.S_IREAD)
            st = os.stat(p)
            rel = os.path.relpath(p, dest).replace("\\", "/")
            manifest[rel] = [st.st_mtime_ns, st.st_size]
    return manifest


def main():
    only = None
    if "--only" in sys.argv:
        only = set(sys.argv[sys.argv.index("--only") + 1].split(","))
    ids = canonical_ids()
    if only:
        ids = [i for i in ids if i in only]
    corpus = {str(r["pr_id"]): r for r in json.load(open(rp.CORPUS, encoding="utf-8"))}
    os.makedirs(MANIFEST_DIR, exist_ok=True)
    imap = json.load(open(IMAP_PATH, encoding="utf-8")) if os.path.exists(IMAP_PATH) else {}

    eng = ec.Engram()
    try:
        for n, pid_key in enumerate(ids, 1):
            rec = corpus[pid_key]
            t0 = time.time()
            print(f"[{n}/{len(ids)}] PR {pid_key} @ {rec['base_commit'][:8]} …", flush=True)
            pid, wt, secs, health = rp.setup_index(eng, rec)
            hline = next(
                (ln for ln in health.splitlines() if ln.startswith("Health:")), "?"
            )
            print(f"  indexed {secs:.0f}s pid={pid} {hline[:90]}", flush=True)
            imap[pid_key] = pid
            json.dump(imap, open(IMAP_PATH, "w", encoding="utf-8"), indent=1)

            snap = os.path.join(SNAP_ROOT, f"pr{pid_key}")
            ec.add_snapshot(rec["base_commit"], snap)
            manifest = freeze_snapshot(snap)
            json.dump(
                {"pr": pid_key, "base": rec["base_commit"], "files": manifest},
                open(
                    os.path.join(MANIFEST_DIR, f"pr{pid_key}.json"),
                    "w",
                    encoding="utf-8",
                ),
            )
            print(
                f"  snapshot frozen: {len(manifest)} files read-only "
                f"({time.time() - t0:.0f}s total)",
                flush=True,
            )
    finally:
        eng.close()
    print("G0 refresh complete", flush=True)


if __name__ == "__main__":
    main()
