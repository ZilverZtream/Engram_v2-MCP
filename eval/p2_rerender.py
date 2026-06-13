"""Instantly re-render Phase-2 dossiers from the saved prov dumps — NO Engram
calls, NO index, NO server. Use this to iterate render_dossier ranking/prose:
edit phase2_prep.render_dossier, run this, dossiers update in <1s.

(prov.json already contains the co-change-confirmed + family-expanded file set,
so rendering is pure formatting.)

Usage: python eval/p2_rerender.py 1933 1908 1967 1937 1974
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from phase2_prep import render_dossier, P2_DATA  # noqa: E402


def main():
    prs = [int(x) for x in sys.argv[1:]] or [1933, 1908, 1967, 1937, 1974]
    for pr in prs:
        prov_path = os.path.join(P2_DATA, f"pr{pr}_prov.json")
        man_path = os.path.join(P2_DATA, f"pr{pr}.json")
        if not os.path.exists(prov_path):
            print(f"PR {pr}: no prov dump, skip")
            continue
        prov = json.load(open(prov_path, encoding="utf-8"))   # path -> [signals]
        rows = [(p, set(sigs)) for p, sigs in prov.items()]
        title = json.load(open(man_path, encoding="utf-8"))["story"]["title"]
        md = render_dossier(title, rows)
        with open(os.path.join(P2_DATA, f"pr{pr}_dossier.md"), "w", encoding="utf-8") as fh:
            fh.write(md)
        print(f"PR {pr}: re-rendered ({len(rows)} files, {md.count(chr(10))} lines)")


if __name__ == "__main__":
    main()
