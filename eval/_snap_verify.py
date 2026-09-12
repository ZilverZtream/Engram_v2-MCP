"""Phase G scoring gate (standing mandate): agent snapshot trees are
filesystem read-only and MTIME-CHECKED before scoring. Verifies every
snapshot against its G0 manifest — any drift (touched, added, removed,
re-writable) voids the run for that PR.

Usage: python eval/_snap_verify.py [pr ...]   (default: all manifests)
Exit 0 = all verified; 2 = drift (named).
"""
import json
import os
import stat
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
MANIFEST_DIR = os.path.join(HERE, "data", "p2", "snap_manifests")
SNAP_ROOT = r"C:/ai-projects/_p2snap"


def verify(name):
    mpath = os.path.join(MANIFEST_DIR, f"pr{name}.json")
    m = json.load(open(mpath, encoding="utf-8"))
    # G1b manifests carry the tree they froze; G0-era ones fall back to _p2snap.
    dest = m.get("dest") or os.path.join(SNAP_ROOT, f"pr{name}")
    drift = []
    seen = set()
    for root, _dirs, files in os.walk(dest):
        for f in files:
            p = os.path.join(root, f)
            rel = os.path.relpath(p, dest).replace("\\", "/")
            seen.add(rel)
            want = m["files"].get(rel)
            if want is None:
                drift.append(f"ADDED {rel}")
                continue
            st = os.stat(p)
            if [st.st_mtime_ns, st.st_size] != want:
                drift.append(f"MODIFIED {rel}")
            if st.st_mode & stat.S_IWRITE:
                drift.append(f"WRITABLE {rel}")
    for rel in m["files"]:
        if rel not in seen:
            drift.append(f"REMOVED {rel}")
    return drift


def main():
    prs = sys.argv[1:]
    if not prs:
        prs = sorted(
            f[2:-5] for f in os.listdir(MANIFEST_DIR) if f.startswith("pr")
        )
    bad = 0
    for pr in prs:
        drift = verify(pr)
        if drift:
            bad += 1
            print(f"PR {pr}: DRIFT ({len(drift)}) — {drift[:5]}")
        else:
            print(f"PR {pr}: verified (read-only, mtimes intact)")
    if bad:
        print(f"REFUSE SCORING: {bad} snapshot(s) drifted")
        return 2
    print("all snapshots verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
