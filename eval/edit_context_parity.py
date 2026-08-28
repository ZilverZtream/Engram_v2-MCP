"""Row-2 live gates on a real index (docs/audits/02 §5 G2/G3/G4/G6).

For each named method: resolve the node via query_graph_nodes, run
get_method_edit_context and check_edit_safety (output_json), and check
  G2  the two tools' edit_safety JSON is identical
  G3  the caller count is exact or labelled as a lower bound
  G4  complexity is measured (status complete) by both tools
  G6  wall time per call (includes the engram_drive client spawn)

Usage: python eval/edit_context_parity.py <project_id> [names...]
Runs against the LIVE daemon through tools/engram_drive.py.
"""
import json
import subprocess
import sys
import time

ROOT = r"C:\ai-projects\Engram-MCP_v2"
DEFAULT = [
    # hot fan-in helpers
    "Check_pr_id", "CheckRead", "CheckWrite", "SafeRedirect", "LogError",
    "GetDictionaryIntegerValue", "GetDictionaryStringValue", "CheckIfAdminOrArbetsledare",
    "GetAllByCheckingTotalProject", "GetByID",
    # ordinary endpoints / page methods
    "ioGetIdsFilteredByMarkerCheckListItemStatus", "ioGetCountByCategory", "ioUpdateBaseTypeInBulk",
    "iopDeleteInBulk", "iomsBulkUpdate", "iomsBulkPreCheck", "kmlquery_Load", "GetMimeType",
    "BuildAttachmentContentDisposition", "GetGisFile",
]


def drive(tool, args):
    t0 = time.perf_counter()
    r = subprocess.run(
        [sys.executable, "tools/engram_drive.py", "tool", tool, json.dumps(args), "200000"],
        capture_output=True, text=True, encoding="utf-8", errors="replace", cwd=ROOT,
    )
    return r.stdout, time.perf_counter() - t0


def parse_json(text):
    i = text.find("{")
    if i < 0:
        return None
    try:
        return json.loads(text[i:])
    except json.JSONDecodeError:
        return None


def resolve(pid, name):
    out, _ = drive("query_graph_nodes", {"project_id": pid, "node_type": "function", "name_pattern": name, "limit": 20})
    for line in out.splitlines():
        if not line.startswith("- sym:function:"):
            continue
        head = line[2:].split(" | ")[0]  # sym:function:<path>:<fqn>:<line>
        parts = head.split(":")
        try:
            ln = int(parts[-1])
        except ValueError:
            continue
        fqn = parts[-2]
        path = ":".join(parts[2:-2])
        if fqn.rsplit(".", 1)[-1].lower() == name.lower():
            return path, ln
    return None, None


def main():
    pid = sys.argv[1]
    names = sys.argv[2:] or DEFAULT
    rows = []
    for name in names:
        path, ln = resolve(pid, name)
        if not path:
            rows.append({"name": name, "note": "not resolved"})
            continue
        base = {"project_id": pid, "file_path": path, "method_name": name, "line": ln, "output_json": True}
        ctx_txt, t_ctx = drive("get_method_edit_context", {**base, "include_business_logic": False})
        saf_txt, t_saf = drive("check_edit_safety", base)
        ctx, saf = parse_json(ctx_txt), parse_json(saf_txt)
        if ctx is None or saf is None:
            rows.append({"name": name, "path": path, "note": "non-JSON: " + (ctx_txt if ctx is None else saf_txt)[:160].replace("\n", " ")})
            continue
        es = ctx["edit_safety"]
        callers = es["completeness"]["callers"]
        rows.append({
            "name": name, "path": path, "line": ln,
            "parity": es == saf,
            "verdict": es["verdict"],
            "callers_listed": len(ctx["method_info"]["called_by"]),
            "callers_status": callers.get("status"),
            "callers_total": callers.get("known_total"),
            "dangling": es["completeness"]["callers_dangling"],
            "complexity": ctx["method_info"]["complexity_score"],
            "complexity_status": es["completeness"]["complexity"]["status"],
            "blast": ctx["blast_radius_score"],
            "blast_status": es["completeness"]["blast"]["status"],
            "t_ctx": round(t_ctx, 2), "t_saf": round(t_saf, 2),
        })
    print("| method | verdict | parity | callers listed / total (status) | dangling | complexity (status) | blast (status) | t ctx / safety s |")
    print("|---|---|---|---|---|---|---|---|")
    for r in rows:
        if "note" in r:
            print(f"| {r['name']} | — | — | {r['note']} | | | | |")
            continue
        print(f"| {r['name']} | {r['verdict']} | {'yes' if r['parity'] else 'NO'} | {r['callers_listed']} / {r['callers_total']} ({r['callers_status']}) | {r['dangling']} | {r['complexity']} ({r['complexity_status']}) | {r['blast']} ({r['blast_status']}) | {r['t_ctx']} / {r['t_saf']} |")
    ok = [r for r in rows if "note" not in r]
    print()
    print(f"resolved {len(ok)}/{len(rows)} · parity {sum(r['parity'] for r in ok)}/{len(ok)} · complexity measured {sum(r['complexity_status']=='complete' for r in ok)}/{len(ok)} · callers exact-or-labelled {sum(r['callers_status'] in ('complete','truncated') for r in ok)}/{len(ok)} · max wall {max((max(r['t_ctx'], r['t_saf']) for r in ok), default=0)} s")
    with open(r"C:\Users\Dennis\AppData\Local\Temp\claude\C--ai-projects-Engram-MCP-v2\50d309ed-9767-44e6-b7fc-2ac51dd98fa5\scratchpad\p0\live\parity.json", "w", encoding="utf-8") as f:
        json.dump(rows, f, indent=1)


if __name__ == "__main__":
    main()
