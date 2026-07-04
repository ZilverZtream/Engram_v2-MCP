import os, re
os.environ["ENGRAM_EVAL_DATA_DIR"] = r"C:\Users\Dennis\AppData\Local\Temp\engram_ociusx_master_idx"
os.environ["ENGRAM_EVAL_EXTRA_ROOT"] = r"C:\playwright\OciusX_master815"
import engram_client as ec  # noqa: E402

PID = open("eval/data/ociusx_master815_pid.txt", encoding="utf-8").read().strip()
STORY = ("As an admin I would like to set minimum number of photos required, per photo group. "
         "On a marker checklist item, each photo group (image group 1..5) has a custom label "
         "(e.g. 'Before drilling') and a required number of images. Admin sets the required count "
         "per photo group. Block completing or inspecting the checklist item until each photo group "
         "has at least its required number of photos.")

eng = ec.Engram(stderr_path=os.path.join(ec.WORKTREE_ROOT, "dmo815_gcs.stderr.log"))
try:
    out = eng.tool("get_change_set", {"project_id": PID, "story": STORY}, _cap=120000)
    if out.startswith("__TOOL_ERROR__"):
        print(out[:400]); raise SystemExit(1)
    # print candidate files section + count
    print(out)
finally:
    eng.close()
