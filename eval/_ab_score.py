"""Score the n=6 control vs engram A/B by file-set recall/precision vs the real
merged PR. Proposals captured from the Sonnet impl agents (this session)."""
import ast
import json
import os


def base(p):
    return p.replace("\\", "/").lower().rstrip("/").split("/")[-1]


recs = {str(r["pr_id"]): r for r in json.load(open(os.path.join("eval", "data", "ociusx_prs.json"), encoding="utf-8"))}


def real_bases(pr):
    g = recs[pr]["ground_truth"]
    g = ast.literal_eval(g) if isinstance(g, str) else g
    return {base(f["path"] if isinstance(f, dict) else f) for f in g["changed_files"]}


# Proposed file basenames per arm (from the impl agents)
P = {
    "1908": {
        "control": ["web.config", "configsettings.vb", "ctrl_files.ascx.vb", "markericonhandler.ashx",
                    "markericonhandler.ashx.vb", "control.resx", "control.en.resx", "control.de.resx",
                    "control.no.resx", "control.es.resx", "control.pt.resx", "control.sl.resx"],
        "engram": ["web.config", "configsettings.vb", "ctrl_files.ascx", "ctrl_files.ascx.vb",
                   "text.resx", "text.en.resx", "text.no.resx", "text.de.resx", "text.pt.resx",
                   "text.es.resx", "text.sl.resx"],
    },
    "1933": {
        "control": ["producedq.aspx.vb"],
        "engram": ["producedq.aspx"],
    },
    "1893": {
        "control": ["io-installationsobjekt.vb", "marker.aspx", "marker.aspx.vb"],
        "engram": ["marker_edit.aspx", "marker_edit.aspx.vb", "marker.edit.js", "label.resx",
                   "label.en.resx", "label.de.resx", "label.no.resx", "label.es.resx",
                   "label.pt.resx", "label.sl.resx"],
    },
    "1904": {
        "control": ["map.aspx", "tasksearchfilter.vb", "api-taskmanagement.vb", "aktivitet.vb",
                    "maptaskmanager.ts", "map.js"],
        "engram": ["tasksearchfilter.vb", "api-taskmanagement.vb", "aktivitet.vb", "index.vbhtml",
                   "maptaskmanager.ts", "map.js", "taskmanagement.aspx", "taskmanagement.aspx.vb",
                   "label.resx", "label.en.resx", "label.de.resx", "label.es.resx", "label.no.resx",
                   "label.pt.resx", "label.sl.resx"],
    },
    "1938": {
        "control": ["estimatedvsreportedquantities.aspx", "estimatedvsreportedquantities.aspx.vb",
                    "label.resx", "label.en.resx"],
        "engram": ["qtymanager.ts", "qtymodaldialog.ts", "roqqtymanager.js", "fbinstplan.js", "map.js"],
    },
    "1905": {
        "control": ["roqpricelistservice.vb", "api-redovisningslisttyp.vb", "iqtymanager.ts", "qtymanager.ts",
                    "roqqtymanager.js", "map.js", "fbinstplan.js", "text.resx", "text.en.resx",
                    "text.de.resx", "text.no.resx", "text.es.resx", "text.pt.resx", "text.sl.resx"],
        "engram": ["producedq.aspx.vb", "roqreport.vb", "api-redovisning.vb"],
    },
}

tot = {"control": [0, 0], "engram": [0, 0]}  # [hits, real_total]
prec = {"control": [0, 0], "engram": [0, 0]}  # [hits, proposed_total]
print(f"{'PR':6} {'real':4}  control(rec/prec)   engram(rec/prec)")
for pr in P:
    real = real_bases(pr)
    line = f"{pr:6} {len(real):4}  "
    for arm in ("control", "engram"):
        prop = {base(x) for x in P[pr][arm]}
        hits = len(prop & real)
        tot[arm][0] += hits; tot[arm][1] += len(real)
        prec[arm][0] += hits; prec[arm][1] += len(prop)
        line += f"{hits}/{len(real)}={hits/len(real):.0%},{hits}/{len(prop)}={hits/len(prop):.0%}".ljust(20)
    print(line)

print("\n=== TOTALS (micro-avg over 6 PRs) ===")
for arm in ("control", "engram"):
    r = tot[arm][0] / tot[arm][1]
    p = prec[arm][0] / prec[arm][1]
    f1 = 2 * r * p / (r + p) if (r + p) else 0
    print(f"  {arm:8}: recall {tot[arm][0]}/{tot[arm][1]}={r:.1%}  precision {prec[arm][0]}/{prec[arm][1]}={p:.1%}  F1={f1:.1%}")
