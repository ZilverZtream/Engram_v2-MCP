"""Drive the deployed Engram server across many tools against the MiniLang
index in ONE session, and report which ones fail or return nothing useful.

Spawning engram_server per tool call (what engram_drive.py does) costs ~10s
each; this reuses a single stdio session for the whole sweep.

Usage: python tools/minilang_sweep.py <project_id> [outfile]
"""
import json
import subprocess
import sys

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

EXE = r"C:\Users\Dennis\AppData\Local\engram\bin\engram_server.exe"
STDERR_LOG = r"C:\ai-projects\Engram-MCP_v2\target\sweep_stderr.log"

PID = sys.argv[1]
OUT = sys.argv[2] if len(sys.argv) > 2 else None

proc = subprocess.Popen(
    [EXE],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=open(STDERR_LOG, "a", encoding="utf-8"),
    text=True,
    encoding="utf-8",
    bufsize=1,
)
mid = 0


def rpc(method, params):
    global mid
    mid += 1
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": mid, "method": method, "params": params}) + "\n")
    proc.stdin.flush()
    while True:
        line = proc.stdout.readline()
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


rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "sweep", "version": "0.1"}})
proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
proc.stdin.flush()


def call(name, args):
    r = rpc("tools/call", {"name": name, "arguments": args})
    if "error" in r:
        return ("ERROR", str(r["error"]))
    parts = r.get("result", {}).get("content", [])
    text = "\n".join(p.get("text", "") for p in parts if p.get("type") == "text")
    return ("OK", text)


# A MiniLang-heavy set of probes. Each is (label, tool, args).
ML_FILE = "tests/conformance/fibers/test_spawn_detached_channel_arg.ml"
PROBES = [
    ("overview", "get_codebase_overview", {"project_id": PID}),
    ("health", "project_health", {"project_id": PID}),
    ("ask", "ask_codebase", {"project_id": PID, "question": "How does a MiniLang program spawn a detached fiber and send on a channel?"}),
    ("search_ml", "search_memory", {"project_id": PID, "query": "Union variant Match Case Else", "max_results": 3, "language_filters": ["minilang"]}),
    ("grep_ml", "grep_project", {"project_id": PID, "pattern": "Spawn Detached", "max_results": 5}),
    ("refs", "find_symbol_references", {"project_id": PID, "symbol_name": "Worker", "max_incoming": 2}),
    ("blast", "compute_blast_radius", {"project_id": PID, "symbol_fqn": "Worker"}),
    ("method_info", "get_method_info", {"project_id": PID, "fqn_or_name": "Worker"}),
    ("dead", "find_dead_methods", {"project_id": PID, "limit": 5}),
    ("cycles", "find_dependency_cycles", {"project_id": PID}),
    ("patterns", "detect_design_patterns", {"project_id": PID}),
    ("impl_pattern", "find_implementation_pattern", {"project_id": PID, "pattern_query": "send a value on a typed channel then close it"}),
    ("freshness", "get_index_freshness", {"project_id": PID}),
    ("ast_graph", "ast_dependency_graph", {"project_id": PID, "entry": ML_FILE}),
    ("test_matrix", "derive_test_matrix", {"project_id": PID, "files": [ML_FILE]}),
    ("style", "analyze_file_coding_style", {"project_id": PID, "file_path": ML_FILE}),
    ("biz", "analyze_business_logic", {"project_id": PID, "file_path": ML_FILE}),
    # MiniLang-specific value: .ml <-> .expected/.error oracles are a
    # test_oracle edge kind that exists for no other language here.
    ("tests_for", "find_tests_for_method", {"project_id": PID, "method_name": "Worker"}),
    ("full_body", "get_full_method_body", {"project_id": PID, "fqn": "Worker"}),
    ("edit_safety", "check_edit_safety", {"project_id": PID, "file_path": ML_FILE, "method_name": "Worker"}),
    ("traverse", "traverse_graph", {"project_id": PID, "node_id": f"sym:function:{ML_FILE}:Worker:6", "max_hops": 2}),
    ("dataflow", "trace_data_flow", {"project_id": PID, "file_path": ML_FILE, "entry_point": "Worker"}),
    ("change_set", "get_change_set", {"project_id": PID, "story": "Add a bounded retry when a channel send fails on a closed channel"}),
]

lines = []
for label, tool, args in PROBES:
    try:
        status, text = call(tool, args)
    except Exception as e:  # noqa: BLE001
        status, text = "EXC", repr(e)
    body = (text or "").strip()
    # Heuristic: flag empties and obvious no-data answers for manual review.
    flag = ""
    if status != "OK":
        flag = "  <<< FAILED"
    elif not body:
        flag = "  <<< EMPTY"
    elif len(body) < 120:
        flag = "  <<< THIN"
    lines.append(f"=== [{label}] {tool} -> {status} ({len(body)} chars){flag}")
    lines.append(body[:1400])
    lines.append("")

report = "\n".join(lines)
if OUT:
    with open(OUT, "w", encoding="utf-8") as fh:
        fh.write(report)
    # Summary to stdout only.
    for ln in lines:
        if ln.startswith("=== ["):
            print(ln)
else:
    print(report)

try:
    proc.stdin.close()
    proc.wait(timeout=20)
except Exception:  # noqa: BLE001
    proc.kill()
