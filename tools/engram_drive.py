"""Drive the deployed Engram MCP server over stdio JSON-RPC, phase by phase."""
import json
import subprocess
import sys
import os

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

EXE = r"C:\Users\Dennis\AppData\Local\engram\bin\engram_server.exe"
STDERR_LOG = r"C:\ai-projects\Engram-MCP_v2\target\driver_stderr.log"

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


def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def rpc(method, params):
    global mid
    mid += 1
    send({"jsonrpc": "2.0", "id": mid, "method": method, "params": params})
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


def tool(name, args):
    r = rpc("tools/call", {"name": name, "arguments": args})
    if "error" in r:
        return f"TOOL ERROR ({name}): {r['error']}"
    parts = r.get("result", {}).get("content", [])
    return "\n".join(p.get("text", "") for p in parts if p.get("type") == "text")


init = rpc(
    "initialize",
    {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "engram-driver", "version": "0.1"},
    },
)
send({"jsonrpc": "2.0", "method": "notifications/initialized"})

phase = sys.argv[1] if len(sys.argv) > 1 else "list"
OCIUSX_DIR = r"C:\Users\Dennis\source\repos\OciusX"

if phase == "list":
    print(tool("list_projects", {}))

elif phase == "reindex":
    old_id = sys.argv[2] if len(sys.argv) > 2 else None
    if old_id:
        print("--- delete old project ---")
        print(tool("delete_project", {"project_id": old_id}))
    print("--- index_project (this can take a while) ---")
    print(
        tool(
            "index_project",
            {
                "directory": OCIUSX_DIR,
                "project_name": "OciusX",
                "project_type": "dotnet_webforms_vb",
                "wait": True,
                "dedupe_by_directory": False,
            },
        )[:4000]
    )
    print("--- list (new id) ---")
    print(tool("list_projects", {}))

elif phase == "inspect":
    pid = sys.argv[2]
    print("=== project_health ===")
    print(tool("project_health", {"project_id": pid}))
    print("=== get_index_freshness ===")
    print(tool("get_index_freshness", {"project_id": pid, "check_disk": False}))
    print("=== overview (trimmed) ===")
    out = tool("get_codebase_overview", {"project_id": pid})
    print(out[:6500])

elif phase == "guards":
    pid = sys.argv[2]
    out = tool("map_guards_and_settings", {"project_id": pid})
    print(out[:5000])

elif phase == "story":
    pid = sys.argv[2]
    story = (
        sys.argv[3]
        if len(sys.argv) > 3
        else "As an admin I would like to set minimum number of photos required"
    )
    out = tool("plan_user_story", {"project_id": pid, "story": story})
    print(out[:7000])

elif phase == "claudemd":
    pid = sys.argv[2]
    out = tool(
        "produce_claude_md",
        {
            "project_id": pid,
            "merge_existing": True,
            "write_to_disk": True,
            "generate_agents_md": False,
        },
    )
    print(out[:3500])
    print("=== agent integration pack ===")
    print(
        tool(
            "generate_agent_integration",
            {"project_id": pid, "write_files": True, "windows": True},
        )[:2000]
    )

elif phase == "verify":
    # One-shot OciusX verification battery: health, counts, GIS, guards,
    # path probe, cycles, story. Usage: ... verify <project_id>
    pid = sys.argv[2]
    print("=== health ===")
    print(tool("project_health", {"project_id": pid}))
    print("=== gis (top) ===")
    print(tool("get_gis_inventory", {"project_id": pid})[:1200])
    print("=== guards (top) ===")
    print(tool("map_guards_and_settings", {"project_id": pid})[:1200])
    print("=== path probe ===")
    print(tool("find_connection_path", {
        "project_id": pid,
        "from": "ConfigSettings.Map",
        "to": "ss_systemsettings",
        "max_depth": 8,
    })[:1500])
    print("=== cycles (top) ===")
    print(tool("find_dependency_cycles", {"project_id": pid, "limit": 5})[:1500])
    print("=== story probe ===")
    print(tool("plan_user_story", {
        "project_id": pid,
        "story": "As an admin I would like to set minimum number of photos required",
    })[:1800])

elif phase == "eval":
    # TODO-48: golden-query retrieval scorecard against the live index.
    # Each query lists substrings; a hit = any top-5 result path contains
    # any expected substring. Usage: ... eval <project_id>
    pid = sys.argv[2]
    GOLDEN = [
        ("minimum number of photos required", ["marker", "api-images"], "photos story"),
        ("upload image for map marker", ["api-images"], "image upload api"),
        ("check if user is in role", ["checkisuserinrole", "shared", "security"], "house guard"),
        ("system settings stored in database", ["systemsettings"], "settings table access"),
        ("google maps marker clustering", ["map"], "gis surface"),
        ("session timeout configuration", ["web.config", "global", "session"], "session config"),
        ("save reporting of quantities entry", ["Roq", "roq", "qty"], "RoQ feature"),
        ("SAML single sign on", ["SAML", "saml"], "auth integration"),
        ("delete installation plan", ["instplan", "installation"], "installations"),
        ("customer specific multi tenant filter", ["tenant", "instance"], "multitenancy"),
    ]
    hits = 0
    for query, expected, note in GOLDEN:
        out = tool("search_memory", {
            "query": query, "project_id": pid, "max_results": 5,
        })
        paths = [l.split("path: ", 1)[1] for l in out.splitlines() if l.startswith("path: ")]
        ok = any(any(e.lower() in pth.lower() for e in expected) for pth in paths)
        hits += 1 if ok else 0
        mark = "HIT " if ok else "MISS"
        print(f"[{mark}] {note}: '{query}'")
        if not ok:
            for pth in paths[:3]:
                print(f"        got: {pth}")
    print(f"\nscore: {hits}/{len(GOLDEN)} hit@5")

elif phase == "tool":
    # Generic: python engram_drive.py tool <tool_name> '<json_args>' [max_chars]
    name = sys.argv[2]
    targs = json.loads(sys.argv[3]) if len(sys.argv) > 3 else {}
    cap = int(sys.argv[4]) if len(sys.argv) > 4 else 8000
    print(tool(name, targs)[:cap])

proc.stdin.close()
try:
    # Graceful: EOF on stdin lets the server drop writers/locks cleanly.
    # terminate() mid-cleanup leaves a stale tantivy writer lock that makes
    # the NEXT phase's bulk writer fail ("index consumer dropped").
    proc.wait(timeout=20)
except subprocess.TimeoutExpired:
    proc.terminate()
