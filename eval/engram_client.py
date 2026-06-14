"""Minimal stdio JSON-RPC client for a spawned engram_server, plus helpers
to (a) index an OciusX worktree at a historical commit and (b) extract the
set of source-file paths a tool's text output references.

Used by the Phase-1 strategy tournament. Read-only w.r.t. OciusX: the
historical checkout happens in a git worktree under a temp dir; the live
working tree is never touched.
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

EXE = r"C:\Users\Dennis\AppData\Local\engram\bin\engram_server.exe"
OCIUSX_REPO = r"C:\Users\Dennis\source\repos\OciusX"
PROD_CONFIG = r"C:\Users\Dennis\AppData\Roaming\engram\engram\config\engram_mcp.yaml"

# Throwaway git worktrees + the eval server config live in the system temp dir,
# never in the repo (the config carries an API key; worktrees are scratch).
WORKTREE_ROOT = os.path.join(tempfile.gettempdir(), "engram_eval_wt")
EVAL_CONFIG = os.path.join(tempfile.gettempdir(), "engram_eval_config.yaml")


def build_eval_config():
    """Derive an eval server config from the production one: same data_dir (so
    the 515 MB embed cache is reused — historical OciusX content is mostly cache
    hits), same Ollama embedding settings, but (a) allowed_roots extended with
    the temp worktree root and (b) multi_client forced off so this dedicated
    eval server is the sole holder of the data_dir lock. Written to %TEMP% so
    the embedded API key never lands in the repo.

    Requires no production engram_server to be running (single-writer redb lock).
    """
    os.makedirs(WORKTREE_ROOT, exist_ok=True)  # must exist for root canonicalization
    # Optional override: point at a FRESH, small data_dir (env ENGRAM_EVAL_DATA_DIR)
    # to avoid the 29 GB production store whose 18 GB graph redb makes startup slow
    # (and crash-recovery after a forced kill slower still). Used by the Stage-3 QG
    # work, which is self-contained and needs none of the production indexes.
    data_override = os.environ.get("ENGRAM_EVAL_DATA_DIR")
    extra_root = os.environ.get("ENGRAM_EVAL_EXTRA_ROOT")
    with open(PROD_CONFIG, encoding="utf-8") as fh:
        lines = fh.readlines()
    out, injected, saw_mc = [], False, False
    wt_yaml = WORKTREE_ROOT.replace("'", "''")
    if data_override:
        os.makedirs(data_override, exist_ok=True)
    for ln in lines:
        stripped = ln.strip()
        if stripped.startswith("multi_client:"):
            out.append("multi_client: false\n")
            saw_mc = True
            continue
        if data_override and stripped.startswith("data_dir:"):
            out.append(f"data_dir: '{data_override.replace(chr(39), chr(39) * 2)}'\n")
            continue
        out.append(ln)
        if not injected and stripped == "allowed_roots:":
            out.append(f"  - '{wt_yaml}'\n")
            if extra_root:
                out.append(f"  - '{extra_root.replace(chr(39), chr(39) * 2)}'\n")
            injected = True
    if not injected:
        raise RuntimeError("could not find 'allowed_roots:' in production config")
    if not saw_mc:
        out.append("multi_client: false\n")
    with open(EVAL_CONFIG, "w", encoding="utf-8") as fh:
        fh.writelines(out)
    return EVAL_CONFIG

# Source paths the eval cares about (matches both prose and `kind:PATH:name:line`
# node-id forms). Compound ASP.NET extensions are listed FIRST so a code-behind
# like `logs.aspx.vb` matches as `.aspx.vb`, not truncated to `.aspx`.
_SRC_RE = re.compile(
    r"[\w./\\-]*?\.(?:"
    r"aspx\.vb|ascx\.vb|asax\.vb|asmx\.vb|ashx\.vb|svc\.vb|master\.vb|"
    r"aspx\.cs|ascx\.cs|asax\.cs|asmx\.cs|ashx\.cs|svc\.cs|master\.cs|"
    r"aspx|ascx|asax|ashx|asmx|svc|master|vb|cs|ts|tsx|js|jsx|sql|config|"
    r"vbhtml|cshtml|resx|css|html|yaml|yml)\b",
    re.I,
)


class Engram:
    def __init__(self, stderr_path=None):
        env = dict(os.environ)
        env["ENGRAM_CONFIG_PATH"] = build_eval_config()
        # Dedicated eval server: own config (multi_client off), production data_dir.
        err = open(stderr_path, "w", encoding="utf-8") if stderr_path else subprocess.DEVNULL
        self._err = err if stderr_path else None
        self.p = subprocess.Popen(
            [EXE], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=err, text=True, encoding="utf-8", bufsize=1, env=env,
        )
        self.mid = 0
        self._rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                 "clientInfo": {"name": "eval", "version": "1"}})
        self._notify("notifications/initialized")

    def _send(self, obj):
        self.p.stdin.write(json.dumps(obj) + "\n")
        self.p.stdin.flush()

    def _notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def _rpc(self, method, params):
        self.mid += 1
        mid = self.mid
        self._send({"jsonrpc": "2.0", "id": mid, "method": method, "params": params})
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"server exited during {method}")
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == mid:
                return msg

    def tool(self, name, args, _cap=200000):
        """Call a tool, return its text output (or '' on error)."""
        r = self._rpc("tools/call", {"name": name, "arguments": args})
        if "error" in r:
            return f"__TOOL_ERROR__ {name}: {r['error']}"
        parts = r.get("result", {}).get("content", [])
        return "\n".join(p.get("text", "") for p in parts if p.get("type") == "text")[:_cap]

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=10)
        except Exception:
            self.p.terminate()
        finally:
            if getattr(self, "_err", None):
                try:
                    self._err.close()
                except Exception:
                    pass


def add_worktree(base_commit, dest):
    """Create a detached worktree of OciusX at `base_commit` (read-only to the
    live tree). Returns dest on success."""
    if os.path.exists(dest):
        subprocess.run(["git", "-C", OCIUSX_REPO, "worktree", "remove", "--force", dest],
                       capture_output=True)
    r = subprocess.run(
        ["git", "-C", OCIUSX_REPO, "worktree", "add", "--detach", dest, base_commit],
        capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"worktree add failed: {r.stderr[:300]}")
    return dest


def remove_worktree(dest):
    subprocess.run(["git", "-C", OCIUSX_REPO, "worktree", "remove", "--force", dest],
                   capture_output=True)


def add_snapshot(base_commit, dest):
    """Materialize OciusX at `base_commit` as a PLAIN file tree (no `.git`) via
    `git archive`. Use this for any tree an IMPLEMENTING AGENT reads.

    CRITICAL leakage fix: a linked `git worktree` shares OciusX's object store, so
    an agent can `git -C <dest> log --all` / `git show <future-sha>` and read the
    merged TARGET PR (the answer) — observed: agents quoting future commit hashes
    and reproducing the exact PR. A `git archive` snapshot has no history, so the
    agent sees only the base-state files. (The Engram INDEX worktree still uses
    add_worktree — it legitimately needs git for co-change/history at base.)
    """
    if os.path.exists(dest):
        shutil.rmtree(dest, ignore_errors=True)
    os.makedirs(dest, exist_ok=True)
    archive = subprocess.run(["git", "-C", OCIUSX_REPO, "archive", base_commit],
                             capture_output=True)
    if archive.returncode != 0:
        raise RuntimeError(f"git archive failed: {archive.stderr[:300]!r}")
    tar = subprocess.run(["tar", "-x", "-C", dest], input=archive.stdout,
                         capture_output=True)
    if tar.returncode != 0:
        raise RuntimeError(f"tar extract failed: {tar.stderr[:300]!r}")
    return dest


def norm_path(p):
    """Normalize a path for set comparison: forward slashes (collapsed), no
    leading slash, lowercased."""
    p = p.replace("\\", "/").lower()
    while "//" in p:
        p = p.replace("//", "/")
    return p.lstrip("/")


# Bare filenames (no directory) that are real OciusX root artifacts worth keeping
# even though they have no slash.
_ROOT_FILES = {"web.config", "global.asax", "global.asax.vb", "packages.config",
               "global.asax.cs", "site.master", "site.master.vb"}


def _keep(np):
    if not np or np.startswith(("http", "c:", "f:", "d:")):
        return False
    return "/" in np or np in _ROOT_FILES


def extract_paths(text):
    """Pull the set of normalized source-file paths a tool output references —
    works for prose paths and for `sym:kind:PATH:name:line` node ids."""
    return {np for np in (norm_path(m) for m in _SRC_RE.findall(text or "")) if _keep(np)}


def extract_paths_ordered(text):
    """Like extract_paths but a list in first-appearance order, deduped — tool
    outputs are ranked best-first, so this approximates 'top results by score'."""
    seen, out = set(), []
    for m in _SRC_RE.findall(text or ""):
        np = norm_path(m)
        if _keep(np) and np not in seen:
            seen.add(np)
            out.append(np)
    return out


# Engram emits graph node ids as `node_id=<id>` where <id> is e.g.
# sym:function:Site/foo/bar.aspx.vb:Name:12  or  table:io_marker  etc.
_NODE_ID_RE = re.compile(r"node_id=([^\s)|,;]+)")


def extract_node_ids(text):
    """First-appearance-ordered, deduped list of graph node ids from a tool output."""
    seen, out = set(), []
    for nid in _NODE_ID_RE.findall(text or ""):
        nid = nid.rstrip(".,")
        if nid and nid not in seen:
            seen.add(nid)
            out.append(nid)
    return out


def canon(p):
    """Canonical key for set comparison, robust to the prefix conventions
    different tools (and DevOps) use for the SAME file:
      - git-diff prefixes:  a/foo.vb, b/foo.vb
      - web root present/absent:  Site/App_Code/x.vb  vs  App_Code/x.vb
      - DB root variants:  db-ociusx.sql/dbo/Tables/x.sql  vs  dbo/Tables/x.sql
    Applied symmetrically to predicted AND ground-truth paths."""
    p = norm_path(p)
    for pre in ("a/", "b/"):
        if p.startswith(pre):
            p = p[len(pre):]
    for pre in ("site/", "db-ociusx.sql/", "db-ociusx/"):
        if p.startswith(pre):
            p = p[len(pre):]
            break
    return p


def canon_set(paths):
    return {canon(p) for p in paths}


def basename_set(paths):
    return {p.rsplit("/", 1)[-1] for p in paths}


# WebForms/ASP.NET page families: a "page" spans markup + code-behind + designer
# (foo.aspx, foo.aspx.vb, foo.aspx.designer.vb). Surfacing any member means the
# agent found the right place, so page-level recall collapses them to one key.
_PAGE_EXT = ("aspx", "ascx", "asax", "master", "asmx", "ashx", "svc")


def page_stem(p):
    """Collapse a code-behind/designer/markup path to its page-family key.
    Non-page files map to themselves (canonical path)."""
    p = canon(p)
    for suf in (".designer.vb", ".designer.cs"):
        if p.endswith(suf):
            return p[: -len(suf)]  # foo.aspx.designer.vb -> foo.aspx
    for ext in _PAGE_EXT:
        for code in (".vb", ".cs"):
            if p.endswith("." + ext + code):
                return p[: -len(code)]  # foo.aspx.vb -> foo.aspx
    return p


def page_stem_set(paths):
    return {page_stem(p) for p in paths}
