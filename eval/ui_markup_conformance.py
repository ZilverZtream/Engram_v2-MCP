"""Row 5 v3 slice 1 — a DETERMINISTIC markup-conformance metric for implementation
output (external audit 2026-08-29, owner decision 22:50: "metric -> exemplar
-> enforce"). Two A/Bs of a UI contract were measured on file-F1, which cannot
see HOW markup is written; this metric can.

For every markup file (.aspx/.ascx/.master) the merged PR MODIFIED, compare the
lines the implementer ADDED against the lines the developer ADDED (both diffed
against the same base-commit content, read from the OciusX repo):

  tag F1    — set of element tags in the added lines (asp:Panel, div, uc:files …)
  class F1  — set of CSS classes in the added lines (class= / CssClass=)
  idiom F1  — house idioms in the added lines: Resources:<family>.<key>
              references, runat, server-control attribute names (OnClick,
              CssClass, Visible, DataField …)

file score = mean of the three F1s (a side with nothing added on an axis scores
1.0 when the other side also added nothing, else 0.0); a truth file the
implementer did not touch scores 0.0 and is reported as "untouched".
story score = mean over the truth markup files; the run's score = mean over
stories. No LLM, no ground-truth leakage into the agents (they never see the
merged PR; only this scorer does).

Usage:
  python eval/ui_markup_conformance.py <runs.json> [--repo PATH] [--out report.json]
    runs.json = {"runs": [{"pr_id","arm","rep","files":[{"path","new_content"}]}]}
              (a Workflow return value, or {"result": {...}} as the tool saves it)
  python eval/ui_markup_conformance.py --truth 1899 [1941 ...]
    prints the developer's added-line features per truth file (sanity view).
"""
import argparse
import difflib
import io
import json
import os
import re
import statistics
import subprocess
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
HERE = os.path.dirname(os.path.abspath(__file__))
P2 = os.path.join(HERE, "data", "p2")
DEFAULT_REPO = r"C:\Users\Dennis\source\repos\OciusX"
MARKUP_EXT = ("aspx", "ascx", "master")

TAG_RE = re.compile(r"<([A-Za-z][A-Za-z0-9]*(?::[A-Za-z_][A-Za-z0-9_]*)?)\b")
CLASS_RE = re.compile(r"(?:class|CssClass)\s*=\s*[\"']([^\"']+)[\"']", re.I)
RES_RE = re.compile(r"Resources:\s*([A-Za-z_]+)\s*,\s*([A-Za-z0-9_]+)")
# Every attribute NAME on the added lines (server-control conventions such as
# runat / CssClass / OnClick / DataField / SortExpression …) — names, never
# values, so story-specific IDs and texts do not leak into the axis.
ATTR_RE = re.compile(r"\b([A-Za-z][A-Za-z0-9_:-]*)\s*=\s*[\"'<]")


def is_markup(path):
    return path.lower().rsplit(".", 1)[-1] in MARKUP_EXT


def norm(path):
    return path.replace("\\", "/").lstrip("/")


def git_show(repo, commit, path):
    r = subprocess.run(["git", "-C", repo, "show", f"{commit}:{norm(path)}"],
                       capture_output=True, text=True, encoding="utf-8", errors="replace")
    return r.stdout if r.returncode == 0 else None


def added_lines(base, new):
    """Lines present in `new` but not in `base` (difflib, whitespace-stripped)."""
    # BOM / EOL / indentation rewrites are not markup changes.
    a = [l.strip().lstrip("﻿") for l in (base or "").splitlines()]
    b = [l.strip().lstrip("﻿") for l in (new or "").splitlines()]
    out = []
    for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes():
        if tag in ("insert", "replace"):
            out.extend(l for l in b[j1:j2] if l)
    return out


NUGGET_RE = re.compile(r"<%\s*(#|=|@|:|--)?\s*([A-Za-z]+)?")
JS_LINE_RE = re.compile(r"(^\s*(let|var|const|function|if\s*\(|for\s*\(|return)\b)|;\s*$|^\s*[{}]\s*$")


def features(lines):
    text = "\n".join(lines)
    tags = {t.lower() for t in TAG_RE.findall(text) if not t.startswith(("%", "!", "/"))}
    classes = set()
    for grp in CLASS_RE.findall(text):
        classes.update(c.lower() for c in grp.split()
                       if c and not any(ch in c for ch in "<%()'\""))
    idioms = {f"res:{fam.lower()}.{key.lower()}" for fam, key in RES_RE.findall(text)}
    for line in lines:
        # Server code nuggets are a house idiom of their own (how the team gates
        # or binds markup: `<% If … Then %>` vs `Visible='<%# … %>'`), but a
        # nugget inside a JavaScript line is not markup.
        if JS_LINE_RE.search(line) and not TAG_RE.search(line):
            continue
        for kind, word in NUGGET_RE.findall(line):
            if kind in ("@", "--"):
                continue  # page directives and server comments are not idioms
            idioms.add(f"nugget:{kind or ''}{(word or '').lower()}")
        # Attribute NAMES (never values) on every markup line, continuation
        # lines of a multi-line tag included.
        idioms.update(f"attr:{a.lower()}" for a in ATTR_RE.findall(line))
    return {"tags": tags, "classes": classes, "idioms": idioms}


def f1(a, b):
    if not a and not b:
        return 1.0
    if not a or not b:
        return 0.0
    tp = len(a & b)
    if tp == 0:
        return 0.0
    p, r = tp / len(b), tp / len(a)
    return 2 * p * r / (p + r)


def truth_files(pr):
    m = json.load(io.open(os.path.join(P2, f"pr{pr}.json"), encoding="utf-8"))
    cf = m["ground_truth"]["changed_files"]
    files = [norm(c["path"]) for c in cf
             if is_markup(c["path"]) and "add" not in (c.get("change", "") or "").lower()]
    return m["base_commit"], m["ground_truth"]["merge_commit"], files


def score_file(repo, base, merge, path, produced):
    base_txt = git_show(repo, base, path)
    merged_txt = git_show(repo, merge, path)
    if merged_txt is None:
        return None
    dev = features(added_lines(base_txt, merged_txt))
    if not (dev["tags"] or dev["classes"] or dev["idioms"]):
        # The developer's change has no markup feature to conform to (a pure
        # deletion or a value-only edit): not measurable on this metric.
        return {"path": path, "na": True, "untouched": produced is None, "score": None,
                "dev": {k: sorted(v) for k, v in dev.items()}}
    if produced is None:
        return {"path": path, "untouched": True, "score": 0.0, "dev": {k: sorted(v) for k, v in dev.items()}}
    imp = features(added_lines(base_txt, produced))
    parts = {k: f1(dev[k], imp[k]) for k in ("tags", "classes", "idioms")}
    return {"path": path, "untouched": False, "score": statistics.mean(parts.values()), "axes": parts,
            "dev": {k: sorted(v) for k, v in dev.items()}, "impl": {k: sorted(v) for k, v in imp.items()}}


def match_produced(files, path):
    """The implementer's content for `path` (suffix-tolerant path spelling)."""
    want = norm(path).lower()
    for f in files or []:
        p = norm(f.get("path", "")).lower()
        if p == want or want.endswith("/" + p) or p.endswith("/" + want):
            return f.get("new_content")
    return None


def score_runs(runs, repo):
    rows = []
    for r in runs:
        pr = str(r["pr_id"])
        base, merge, files = truth_files(pr)
        if not files:
            continue
        per = [score_file(repo, base, merge, p, match_produced(r.get("files"), p)) for p in files]
        per = [x for x in per if x]
        scored = [x for x in per if x.get("score") is not None]
        rows.append({"pr_id": pr, "arm": r.get("arm"), "rep": r.get("rep"),
                     "score": statistics.mean(x["score"] for x in scored) if scored else None,
                     "files": per})
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("runs", nargs="?")
    ap.add_argument("--repo", default=DEFAULT_REPO)
    ap.add_argument("--out")
    ap.add_argument("--truth", nargs="*", type=int)
    a = ap.parse_args()
    if a.truth:
        for pr in a.truth:
            base, merge, files = truth_files(pr)
            print(f"== pr{pr}: {len(files)} truth markup file(s)")
            for p in files:
                dev = features(added_lines(git_show(a.repo, base, p), git_show(a.repo, merge, p)))
                print(f"  {p}\n    tags={sorted(dev['tags'])}\n    classes={sorted(dev['classes'])}\n    idioms={sorted(dev['idioms'])}")
        return
    d = json.load(io.open(a.runs, encoding="utf-8", errors="replace"))
    runs = (d.get("result") or d)["runs"]
    rows = score_runs(runs, a.repo)
    by = {}
    print(f"{'pr':>5} {'arm':>9} rep {'score':>6}  files")
    for x in sorted(rows, key=lambda x: (x["pr_id"], x["arm"] or "", x["rep"] or 0)):
        fl = ", ".join(f"{f['path'].split('/')[-1]}={'n/a' if f.get('na') else ('untouched' if f['untouched'] else f'{f['score']:.2f}')}" for f in x["files"])
        print(f"{x['pr_id']:>5} {str(x['arm']):>9} {str(x['rep']):>3} {'-' if x['score'] is None else f'{x['score']:6.2f}'}  {fl}")
        if x["score"] is not None:
            by.setdefault((x["pr_id"], x["arm"]), []).append(x["score"])
    print("\n== per story x arm (mean over reps)")
    arms = sorted({a for _, a in by}, key=str)
    for pr in sorted({p for p, _ in by}):
        print(f"{pr:>5} " + " | ".join(f"{arm}: {statistics.mean(by[(pr, arm)]):.2f} (n={len(by[(pr, arm)])})" for arm in arms if (pr, arm) in by))
    print("\n== overall mean per arm")
    for arm in arms:
        v = [s for (p, ar), ss in by.items() if ar == arm for s in ss]
        if v:
            print(f"  {str(arm):>9}: {statistics.mean(v):.3f} (n={len(v)} runs, {len({p for (p, ar) in by if ar == arm})} stories)")
    if a.out:
        io.open(a.out, "w", encoding="utf-8").write(json.dumps(rows, indent=1, ensure_ascii=False))
        print("wrote", a.out)


if __name__ == "__main__":
    main()
