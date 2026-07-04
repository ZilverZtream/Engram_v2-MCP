"""Fresh, leak-free Engram index of OciusX MASTER (for DMO-815 + future).
Indexes the master worktree into a fresh small data_dir (avoids the 29GB prod
store that crashes on startup). Writes the project_id to a file for get_change_set.
"""
import os
import re
import time

os.environ["ENGRAM_EVAL_DATA_DIR"] = r"C:\Users\Dennis\AppData\Local\Temp\engram_ociusx_master_idx"
# ensure the OciusX master worktree is an allowed root for the eval server
os.environ["ENGRAM_EVAL_EXTRA_ROOT"] = r"C:\playwright\OciusX_master815"

import engram_client as ec  # noqa: E402

MASTER = r"C:\playwright\OciusX_master815"
PID_FILE = os.path.join("eval", "data", "ociusx_master815_pid.txt")
_PID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")

os.makedirs(os.path.join(ec.WORKTREE_ROOT), exist_ok=True)
eng = ec.Engram(stderr_path=os.path.join(ec.WORKTREE_ROOT, "reindex_master815.stderr.log"))
try:
    t0 = time.time()
    print("indexing master worktree (this is the ~30min step)...", flush=True)
    out = eng.tool("index_project", {
        "directory": MASTER,
        "project_name": "ociusx_master_815",
        "project_type": "dotnetwebformsvb",
        "wait": True,
        "dedupe_by_directory": False,
    }, _cap=400000)
    m = _PID_RE.search(out)
    if not m:
        print("NO project_id in index_project output:\n", out[:800], flush=True)
        raise SystemExit(1)
    pid = m.group(0)
    print(f"indexed code in {time.time()-t0:.0f}s -> project_id {pid}", flush=True)
    open(PID_FILE, "w", encoding="utf-8").write(pid)

    th = time.time()
    print("indexing git history (master, max 1500 commits)...", flush=True)
    eng.tool("index_git_history", {"project_id": pid, "max_commits": 1500, "wait": True}, _cap=200000)
    print(f"git history in {time.time()-th:.0f}s", flush=True)

    health = eng.tool("project_health", {"project_id": pid})
    print("HEALTH:\n", health[:1200], flush=True)
    print(f"\nDONE total {time.time()-t0:.0f}s. project_id={pid} (saved to {PID_FILE})", flush=True)
finally:
    eng.close()
