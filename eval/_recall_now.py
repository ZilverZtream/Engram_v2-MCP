"""Current get_change_set page-recall on the 15 canonical PRs, reusing the
fresh kept indexes (no re-index). Honest measurement before any change.

Page-family recall over MODIFIED files (adds/renames excluded — their post-PR
paths do not exist at base). One eval server, sequential (single-writer store).

Usage: python eval/_recall_now.py
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import engram_client as ec  # noqa: E402
import phase2_prep as pp  # noqa: E402

PRS = ["1890", "1893", "1904", "1905", "1908", "1913", "1933", "1937", "1938",
       "1954", "1955", "1965", "1967", "1974", "1979"]
CORPUS = os.path.join(HERE, "data", "ociusx_prs.json")


def canon(p):
    p = p.replace("\\", "/").lower().lstrip("/")
    for pre in ("site/", "src/"):
        if p.startswith(pre):
            p = p[len(pre):]
    return p


def page_key(p):
    # foo.aspx / foo.aspx.vb / foo.aspx.designer.vb collapse to one page.
    for marker in (".aspx", ".ascx", ".master"):
        i = p.find(marker)
        if i >= 0:
            return p[: i + len(marker)]
    return p


def modified_pages(rec):
    out = set()
    for cf in rec["ground_truth"]["changed_files"]:
        ch = (cf.get("change", "") or "").lower()
        if "add" in ch or "rename" in ch:
            continue
        out.add(page_key(canon(cf["path"])))
    return out


def dossier_pages(md):
    pages = set()
    for line in md.splitlines():
        # dossier lists paths; grab anything that looks like a repo path
        for tok in line.replace("`", " ").replace("|", " ").split():
            if "/" in tok and "." in tok.rsplit("/", 1)[-1]:
                pages.add(page_key(canon(tok)))
    return pages


def main():
    recs = {str(r["pr_id"]): r for r in json.load(open(CORPUS, encoding="utf-8"))}
    imap = json.load(open(os.path.join(HERE, "data", "p2", "index_map.json"), encoding="utf-8"))
    eng = ec.Engram()
    tot_hit = tot = 0
    rows = []
    try:
        for pr in PRS:
            rec = recs[pr]
            pid = imap.get(pr)
            if not pid or not pp._project_valid(eng, pid):
                print(f"PR {pr}: NO VALID INDEX", flush=True)
                continue
            wt = os.path.join(pp.P2_WT_ROOT, f"pr{pr}_engram")
            try:
                md, _prov = pp.build_dossier(eng, pid, rec, wt)
            except Exception as e:
                print(f"PR {pr}: dossier error {e}", flush=True)
                continue
            want = modified_pages(rec)
            got = dossier_pages(md)
            hit = sum(1 for w in want if any(g.endswith(w) or w.endswith(g) for g in got))
            tot_hit += hit
            tot += len(want)
            rows.append((pr, hit, len(want)))
            print(f"PR {pr}: {hit}/{len(want)} pages", flush=True)
    finally:
        eng.close()
    print(f"\nTOTAL page-recall: {tot_hit}/{tot} = {tot_hit / tot:.2%}" if tot else "no data", flush=True)
    json.dump(rows, open(os.path.join(HERE, "data", "p2", "_recall_now.json"), "w"), indent=1)


if __name__ == "__main__":
    main()
