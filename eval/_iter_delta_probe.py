"""PROBE (not product): does PR iteration-delta mining yield clean
(review-finding -> fix-hunk) pairs?

For a merged PR, each resolved review thread was raised at publishedDate
on file+line. The iteration created just AFTER that is (usually) the
push that addressed it. Diffing that iteration's source commit against
the prior iteration's, restricted to the thread's file, and extracting
the hunk covering the thread's line, should recover the concrete fix.

This probe prints (finding, matched-fix-hunk) pairs plus a
signal-quality tally (clean single-hunk / no-hunk / multi-hunk-noisy)
so we can decide whether the product feature (fix-exemplars on the rule
corpus) is worth building. Iteration commits must exist in the local
clone (additive PRs; ADO refuses SHA fetch for force-pushed ones).

Usage: python eval/_iter_delta_probe.py <pr_id>
"""
import importlib.util
import os
import subprocess
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
HERE = os.path.dirname(os.path.abspath(__file__))
OCIUSX = r"C:\Users\Dennis\source\repos\OciusX"

spec = importlib.util.spec_from_file_location("ado_fetch", os.path.join(HERE, "ado_fetch.py"))
ado = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ado)


def git(*args):
    r = subprocess.run(["git", "-C", OCIUSX, *args], capture_output=True,
                       text=True, encoding="utf-8", errors="replace")
    return r.returncode, r.stdout


def commit_exists(sha):
    return git("cat-file", "-e", f"{sha}^{{commit}}")[0] == 0


def quoted_tokens(text):
    """Backtick-quoted code tokens the reviewer named, longest first —
    these are what the fix removes/changes, a line-number-independent
    anchor."""
    import re
    toks = re.findall(r"`([^`]{3,60})`", text)
    # Keep identifier-ish quotes (drop pure prose in backticks).
    toks = [t for t in toks if re.search(r"[A-Za-z_]\w*", t)]
    return sorted(set(toks), key=len, reverse=True)


def hunk_with_token(diff_text, tokens):
    """First @@ hunk whose REMOVED (-) side contains a named token —
    the hunk that changed the offending code, regardless of line drift."""
    cur, out = [], None
    removed = []
    for ln in diff_text.splitlines():
        if ln.startswith("@@ "):
            if out is None and any(any(t in r for t in tokens) for r in removed):
                out = "\n".join(cur)
            cur, removed = [ln], []
        else:
            cur.append(ln)
            if ln.startswith("-") and not ln.startswith("---"):
                removed.append(ln)
    if out is None and any(any(t in r for t in tokens) for r in removed):
        out = "\n".join(cur)
    return out


def hunk_covering(diff_text, line):
    """Return the @@ hunk whose NEW-file range covers `line`, else None."""
    import re
    cur, keep = [], False
    out = None
    for ln in diff_text.splitlines():
        m = re.match(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", ln)
        if m:
            if keep and out is None:
                out = "\n".join(cur)
            start = int(m.group(1))
            count = int(m.group(2) or "1")
            keep = start <= line <= start + count + 3
            cur = [ln]
        else:
            cur.append(ln)
    if keep and out is None:
        out = "\n".join(cur)
    return out


def main():
    pr = int(sys.argv[1])
    threads = ado.get(f"{ado.BASE}/git/repositories/{ado.REPO}/pullRequests/{pr}/threads").get("value", [])
    its = ado.get(f"{ado.BASE}/git/repositories/{ado.REPO}/pullRequests/{pr}/iterations").get("value", [])
    if len(its) < 2:
        sys.exit(f"PR{pr}: <2 iterations, nothing to diff")
    # (createdDate, src_commit) per iteration, in order.
    seq = [((it.get("createdDate") or ""), (it.get("sourceRefCommit") or {}).get("commitId", "")) for it in its]
    local = sum(1 for _, c in seq if c and commit_exists(c))
    if local < 2:
        sys.exit(f"PR{pr}: <2 iteration commits local (force-pushed?) — pick another PR")

    fixed = [th for th in threads
             if (th.get("threadContext") or {}).get("filePath")
             and th.get("status") in ("fixed", "closed")
             and [c for c in th.get("comments", []) if c.get("commentType") != "system"]]
    print(f"PR{pr}: {len(its)} iterations, {len(fixed)} resolved file-scoped findings\n")

    tally = {"clean": 0, "no_hunk": 0, "noisy": 0}
    for th in fixed:
        tc = th["threadContext"]
        fp = tc["filePath"].lstrip("/")
        line = (tc.get("rightFileStart") or {}).get("line") or 1
        pub = th.get("publishedDate") or ""
        full = th["comments"][0].get("content") or ""
        text = full.strip().replace("\n", " ")[:90]
        toks = quoted_tokens(full)
        # Search EVERY later local iteration's file diff (fixes batch into
        # non-adjacent pushes), preferring a token-anchored hunk; fall back
        # to the line-covering hunk in the first post-publish iteration.
        located, via, at = None, None, None
        for i in range(1, len(seq)):
            if seq[i][0] <= pub or not (seq[i][1] and commit_exists(seq[i][1])):
                continue
            if not (seq[i - 1][1] and commit_exists(seq[i - 1][1])):
                continue
            code, diff = git("diff", seq[i - 1][1], seq[i][1], "--", fp)
            if code != 0 or not diff.strip():
                continue
            if toks:
                h = hunk_with_token(diff, toks)
                if h:
                    located, via, at = h, "token", i
                    break
            if located is None:
                h = hunk_covering(diff, line)
                if h:
                    located, via, at = h, "line", i  # keep looking for a token match
        if located is None:
            tally["no_hunk"] += 1
            continue
        tally["clean" if via == "token" else "noisy"] += 1
        print(f"FINDING [{fp.split('/')[-1]}]: {text}")
        print(f"  FIX (iter{at}, via {via}):")
        for hl in located.splitlines()[:10]:
            print(f"    {hl}")
        print()
    loc = tally["clean"] + tally["noisy"]
    total = loc + tally["no_hunk"]
    print(f"SIGNAL: {tally}  (clean = token-anchored fix; noisy = line-only fallback)")
    print(f"LOCALIZED: {loc}/{total} findings paired to a fix hunk "
          f"({tally['clean']} token-anchored)")


if __name__ == "__main__":
    main()
