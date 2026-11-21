"""Replay a plain issue on the fix's first parent and score against the withheld patch.

Run with --repo LOCAL_REPOSITORY --fix SHA --story-file ISSUE.txt --binary SERVER
--output RECEIPT.json [--scope SOURCE_DIRECTORY]. Supply the original issue, without
PR discussion or solution hints. Compare different binaries using the same inputs.
A historical patch is one implementation. Overlap measures localization only.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import time

from instruction_outcomes import Client


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args])


def snapshot(repo, parent, root, with_history):
    archive = root.parent / "snapshot.tar"
    with archive.open("wb") as stream:
        subprocess.run(["git", "-C", str(repo), "archive", parent], stdout=stream, check=True)
    with tarfile.open(archive) as tar:
        tar.extractall(root, members=(m for m in tar if m.isfile() or m.isdir()), filter="data")
    archive.unlink()
    if not with_history:
        return
    # Fetch only ancestors of the pre-fix commit into an independent object
    # database. No alternates, remote-tracking refs, tags or oracle remote.
    git(root, "init", "--quiet", "--initial-branch=replay")
    git(root, "fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--depth=512",
        "--update-shallow", repo.as_uri(), parent + ":refs/heads/history")
    git(root, "update-ref", "HEAD", parent)
    git(root, "read-tree", parent)



def first_page(reply):
    if reply.lstrip().startswith("{"):
        try:
            value = json.loads(reply)
            if isinstance(value.get("first_page"), str):
                return value["first_page"]
        except ValueError:
            pass
    return reply


def ranked_sites(plan, root):
    section = first_page(plan).partition("## Ranked implementation sites")[2].split("\n## ")[0]
    rows = []
    for block in re.split(r"(?m)^(?=\d+\. )", section):
        title = re.match(r"\d+\. .*? — (.+?)(?: \[indexed working checkout\])?\s*$", block.split("\n")[0])
        if not title:
            continue
        citations, callee_paths = [], []
        for path, number, quoted in re.findall(r"(?m)^   - (.+?):(\d+)  (.+)$", block):
            file = (root / path).resolve()
            if not file.is_relative_to(root) or not file.is_file():
                continue
            if "(indexed callee " in quoted:
                if path not in callee_paths:
                    callee_paths.append(path)
                continue
            if "(same-file candidate " in quoted:
                continue
            lines = file.read_text(encoding="utf-8-sig", errors="replace").splitlines()
            n = int(number)
            snippet = quoted.removesuffix("…").strip()
            citations.append(dict(path=path, line=n, verified=bool(snippet and 0 < n <= len(lines)
                                                                  and snippet in lines[n-1])))
        declaration = re.search(r"(?m)^   - Declaration: (.+?):(\d+) \(use this start line", block)
        start = int(declaration[2]) if declaration and citations and declaration[1] == citations[0]["path"] else None
        rows.append(dict(name=title[1], citations=citations, callee_paths=callee_paths, declaration_line=start))
    return rows[:3]


def search_paths(reply):
    try:
        data = json.loads(reply)
    except ValueError:
        return []
    return [row["path"] for row in data.get("results", [])[:5] if isinstance(row.get("path"), str)]


def assess_handoff(site, refs, context):
    path = site["citations"][0]["path"]
    symbols = re.findall(r"(?m)^  node_id: sym:[^:\n]+:(.+?):([^:\n]+):(\d+)\s*$", refs)
    site["reference_resolved"] = any(p == path and (n == site["name"] or site["name"].endswith("." + n))
                                     for p, n, _ in symbols)
    related = []
    in_calls = False
    for line in refs.splitlines():
        if line.startswith("    ["):
            in_calls = line.startswith("    [calls]")
        elif line.startswith("  ") and not line.startswith("    "):
            in_calls = False
        if in_calls:
            target = re.search(r"^\s+[<>-]{2} .+? \((.+?):\d+\) ", line)
            if target and target[1] not in related:
                related.append(target[1])
    site["call_edge_files"] = related
    context = first_page(context)
    heading = re.search(r"^(?:# Edit Context|## Method): \x60([^\x60]+)\x60", context, re.M)
    file = re.search(r"\*\*File\*\*: \x60([^\x60]+)\x60", context)
    bounds = re.search(r"\*\*Lines\*\*: (\d+)[–-](\d+)", context)
    if not (heading and file and bounds and file[1] == path):
        return
    name = site["name"]
    if heading[1] != name and not heading[1].endswith("." + name):
        return
    start, end = map(int, bounds.groups())
    if any(c["verified"] and c["path"] == path and start <= c["line"] <= end for c in site["citations"]):
        site["indexed_span"] = [start, end]


def named_call_routes(story, root, call, pid):
    """Probe up to two story-named APIs and two of their indexed callees."""
    names = []
    for match in re.finditer(r"\b(?:[A-Za-z_]\w*\.)*([A-Za-z_]\w*)\b", story[:4000]):
        name = match[1]
        if (re.search(r"[a-z][A-Z]", name) or
                (len(name) >= 4 and story[match.end():].lstrip().startswith("("))):
            if name not in names:
                names.append(name)
        if len(names) == 2:
            break
    paths, children = [], []
    def inspect(name, file=None):
        args = dict(project_id=pid, symbol_name=name, max_incoming=2, max_outgoing_per_kind=8)
        if file:
            args["file_scope"] = file
        reply = call("find_symbol_references", **args)
        for match in re.finditer(r"(?m)^  node_id: sym:[^:\n]+:(.+?):([^:\n]+):\d+\s*$", reply):
            path = match[1]
            if (root / path).is_file():
                paths.append(path)
        for line in reply.splitlines():
            edge = re.match(r"^\s+-> (.+?) \((.+?):\d+\) .*?\[sym:(?:function|method):", line)
            if edge and (root / edge[2]).is_file():
                paths.append(edge[2])
                children.append((edge[1], edge[2]))
    for name in names:
        inspect(name)
    for name, file in list(dict.fromkeys(children))[:2]:
        inspect(name, file)
    return dict(seeds=names, paths=list(dict.fromkeys(paths)))


def collect(client, root, story):
    calls = []
    def call(tool, **args):
        start = time.monotonic()
        entry = dict(tool=tool)
        try:
            reply = client.tool(tool, **args)
        except RuntimeError as exc:
            reply = ""
            entry["error"] = str(exc)
        entry.update(seconds=round(time.monotonic()-start, 3), reply=reply)
        calls.append(entry)
        return reply

    client.rpc("initialize", dict(protocolVersion="2024-11-05", capabilities={},
                                  clientInfo=dict(name="story-workflow-replay", version="1")))
    client.rpc("notifications/initialized", {}, True)
    client.tool("index_project", directory=str(root), project_type="general", project_name="replay", wait=True)
    projects = client.tool("list_projects")
    pid, = set(re.findall(r"[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}", projects))
    plan = call("plan_user_story", project_id=pid, story=story, max_tokens=10000)
    sites = ranked_sites(plan, root)
    search = call("search_memory", project_id=pid, query=story.strip().splitlines()[0],
                  max_results=5, fts_mode="loose", include_content=False, output_json=True, max_tokens=10000)
    for site in sites:
        if not site["citations"]:
            continue
        path = site["citations"][0]["path"]
        name = site["name"]
        refs = call("find_symbol_references", project_id=pid, symbol_name=name, file_scope=path,
                    max_incoming=5, max_outgoing_per_kind=3)
        site["reference_reply_chars"] = len(refs)
        context = call("get_method_edit_context", project_id=pid, file_path=path,
                       method_name=re.split(r"\.|::", name)[-1], include_full_body=False,
                       max_callers=3, max_tokens=10000,
                       **({"line": site["declaration_line"]} if site.get("declaration_line") else {}))
        assess_handoff(site, refs, context)
    named_routes = named_call_routes(story, root, call, pid)
    return dict(sites=sites, plan_section_delivered="## Ranked implementation sites" in first_page(plan),
                search_paths=search_paths(search), named_routes=named_routes, calls=calls)


def score(repo, parent, fix, scope, observations):
    prefix = scope.strip("/")
    files = git(repo, "diff", "--no-renames", "--name-only", "-z", parent, fix, "--").decode().split("\0")
    all_paths = [p for p in files if p]
    files = [p for p in all_paths if (not prefix or p == prefix or p.startswith(prefix + "/"))]
    ranges = {}
    for path in files:
        patch = git(repo, "diff", "--no-ext-diff", "--no-textconv", "--no-renames", "-U0",
                    parent, fix, "--", path).decode(errors="replace")
        ranges[path] = [(int(n), int(count or 1)) for n, count in
                        re.findall(r"(?m)^@@ -(\d+)(?:,(\d+))? \+", patch)]
    file_hits, method_hits = [], []
    for site in observations["sites"]:
        if not site["citations"]:
            continue
        path = site["citations"][0]["path"]
        if path not in ranges:
            continue
        file_hits.append(path)
        if "indexed_span" in site:
            lo, hi = site["indexed_span"]
            # Pure insertions are between parent lines. At either boundary
            # the new text may belong to an adjacent method.
            if any((n <= hi and n+k-1 >= lo) if k else lo <= n < hi for n, k in ranges[path]):
                method_hits.append(site["name"])
    return dict(changed_paths=files, other_changed_paths=[p for p in all_paths if p not in ranges],
                ranked_file_hits=file_hits, ranked_method_hits=method_hits,
                search_file_hits=list(dict.fromkeys(p for p in observations["search_paths"] if p in ranges)),
                named_route_file_hits=[p for p in observations["named_routes"]["paths"] if p in ranges],
                ranked_callee_file_hits=sorted({p for site in observations["sites"] for p in site["callee_paths"] if p in ranges}),
                call_edge_file_hits=sorted({p for site in observations["sites"]
                                            for p in site.get("call_edge_files", []) if p in ranges}))


def run(args):
    repo, binary = args.repo.resolve(), args.binary.resolve()
    if not re.fullmatch(r"[a-fA-F0-9]{7,64}", args.fix):
        raise ValueError("Supply a hexadecimal fix commit ID")
    fix = git(repo, "rev-parse", "--verify", args.fix + "^{commit}").decode().strip()
    parent = git(repo, "rev-parse", fix + "^1").decode().strip()
    story = args.story_file.read_text(encoding="utf-8-sig")
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists():
        raise FileExistsError(output)
    with tempfile.TemporaryDirectory(dir=output.parent, prefix="story-replay-") as temp:
        owned = Path(temp)
        root = owned / "source"
        root.mkdir()
        with_history = not getattr(args, "source_only", False)
        snapshot(repo, parent, root, with_history)
        if with_history and subprocess.run(["git", "-C", str(root), "cat-file", "-e", fix + "^{commit}"],
                          stderr=subprocess.DEVNULL).returncode == 0:
            raise RuntimeError("Withheld fix present in replay Git database")
        history_count = int(git(root, "rev-list", "--count", "HEAD")) if with_history else 0
        config = owned / "config.yaml"
        config.write_text("allowed_roots: [" + json.dumps(str(root)) + "]\ndata_dir: " +
                          json.dumps(str(owned / "data")) +
                          "\nembedding_backend: fts_only\nllm_backend: none\nmulti_client: false\n", encoding="utf8")
        with (owned / "stderr.log").open("w", encoding="utf8") as log:
            client = Client(binary, config, log, temporary_root=owned)
            try:
                evidence = collect(client, root, story)
            finally:
                client.close()
    with binary.open("rb") as stream:
        binary_hash = hashlib.file_digest(stream, "sha256").hexdigest()
    receipt = dict(fix=fix, parent=parent, reachable_history_commits=history_count, scope=args.scope, binary_sha256=binary_hash,
                   story_sha256=hashlib.sha256(story.encode()).hexdigest(), observations=evidence,
                   score=score(repo, parent, fix, args.scope, evidence))
    with output.open("x", encoding="utf8") as stream:
        json.dump(receipt, stream, ensure_ascii=False, indent=2)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("repo", "story-file", "binary", "output"):
        parser.add_argument("--" + arg, required=True, type=Path)
    parser.add_argument("--fix", required=True)
    parser.add_argument("--scope", default="")
    parser.add_argument("--source-only", action="store_true", help="Withhold all Git history for an ablation")
    run(parser.parse_args())
