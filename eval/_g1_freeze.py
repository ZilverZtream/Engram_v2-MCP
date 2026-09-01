"""Phase G1b: freeze the trees the code-gen agents actually read — the
pr{N}_alone / pr{N}_engram snapshot pairs from the phase2 manifests — as
read-only, with mtime/size manifests that _snap_verify checks before
scoring (standing mandate).

Usage: python eval/_g1_freeze.py [pr ...]   (default: the canonical 15)
"""
import json
import os
import stat
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
P2 = os.path.join(HERE, "data", "p2")
MANIFEST_DIR = os.path.join(P2, "snap_manifests")


def canonical_ids():
    v = json.load(open(os.path.join(P2, "_ab15_final_verdicts.json"), encoding="utf-8"))
    return sorted({str(x["pr"]) for x in v["verdicts"]})


def freeze(dest):
    files = {}
    for root, _dirs, names in os.walk(dest):
        for f in names:
            p = os.path.join(root, f)
            os.chmod(p, stat.S_IREAD)
            st = os.stat(p)
            files[os.path.relpath(p, dest).replace("\\", "/")] = [
                st.st_mtime_ns,
                st.st_size,
            ]
    return files


def main():
    prs = sys.argv[1:] or canonical_ids()
    os.makedirs(MANIFEST_DIR, exist_ok=True)
    for pr in prs:
        man = json.load(open(os.path.join(P2, f"pr{pr}.json"), encoding="utf-8"))
        for arm in ("alone", "engram"):
            dest = man[f"worktree_{arm}"]
            files = freeze(dest)
            out = os.path.join(MANIFEST_DIR, f"pr{pr}_{arm}.json")
            json.dump(
                {"pr": pr, "arm": arm, "dest": dest, "files": files},
                open(out, "w", encoding="utf-8"),
            )
            print(f"PR {pr} {arm}: froze {len(files)} files at {dest}", flush=True)
    print("G1b freeze complete", flush=True)


if __name__ == "__main__":
    main()
