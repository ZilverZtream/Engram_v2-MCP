#!/usr/bin/env python3
"""Judge self-test (doc 11 P0-2) — the auditor's live false positive as a
fixture. Usage: python test_golden_judge.py <path-to-golden-script>. Exits 0
when the judge FAILS the blob-satisfiable report and PASSES the honest one;
exits 1 (RED) when the judge awards the false positive. Committed to eval/
with v3 as the judge's own regression guard."""
import importlib.util
import sys
from pathlib import Path


def load(path: str):
    spec = importlib.util.spec_from_file_location("golden_mod", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# The camera-icon row as the auditor re-ran it.
ROW = {
    "id": "ox_usage_5",
    "category": "usage",
    "question": "Which TypeScript files under ts/map render the camera icon on a marker?",
    "expect_status": ["answered", "partial"],
    "must_abstain": False,
    "must_cite_any": [".ts", "iomarker"],
    "required_modality": [".ts"],
    "required_all": ["iomarker"],
    # v3 upgrade fields (ignored by the v2 judge — the false positive stands
    # or falls on the judge, not the row):
    "required_items": [
        {"path_suffix": "iomarkermoment.ts", "content_all": ["camera"]}
    ],
    "answer_files": ["iomarkerinfowindow.ts", "iomarkermoment.ts"],
    "forbidden_classes": ["typings/", "memory_bank:", ".coderabbit"],
}

# What Engram actually returned live (r50 evidence, auditor's re-run): the
# modality token matched unrelated .ts files and the required token lived in
# a checklist document — nothing renders a camera icon.
FALSE_POSITIVE_REPORT = {
    "status": "answered",
    "evidence": [
        {"path": "Site/Q/typings/google.maps/google.maps.d.ts", "content": "declare namespace google.maps { }"},
        {"path": ".coderabbit.yaml", "content": "reviews: profile: chill"},
        {"path": "Site/Q/imgHandler.ts", "content": "export function resize() {}"},
        {"path": "Site/Q/typings/ie/IETypeDefinitions.ts", "content": "interface MSEventObj {}"},
        {"path": "docs/checklists/map-work.md", "content": "verify ioMarkerInfowindow after the sprint"},
        {"path": "memory_bank:engram/index_report", "content": "index report"},
    ],
}

# The honest answer: the two ts/map renderers, camera in the ITEM.
TRUE_POSITIVE_REPORT = {
    "status": "answered",
    "evidence": [
        {"path": "Site/modules/dashboard/ts/map/vsMap/iomarker/ioMarkerInfowindow.ts", "content": "private _btnCamera; m.HighlightCameraButton = (Dirs)"},
        {"path": "Site/modules/dashboard/ts/map/vsMap/iomarker/ioMarkerMoment.ts", "content": "private _btnCameraMoment: q.ctrl.baseCtrl; // camera"},
    ],
}


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: python test_golden_judge.py <path-to-golden-script>")
        return 2
    mod = load(sys.argv[1])
    fp_ok, fp_reason = mod.judge(ROW, "answered", FALSE_POSITIVE_REPORT)
    tp_ok, tp_reason = mod.judge(ROW, "answered", TRUE_POSITIVE_REPORT)
    print(f"false-positive report judged correct={fp_ok} ({fp_reason or 'no reason'})")
    print(f"true-positive  report judged correct={tp_ok} ({tp_reason or 'no reason'})")
    if fp_ok:
        print("RED: the judge awarded the auditor's false positive")
        return 1
    if not tp_ok:
        print(f"BROKEN: the judge rejected the honest answer: {tp_reason}")
        return 1
    print("GREEN: false positive rejected, honest answer accepted")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
