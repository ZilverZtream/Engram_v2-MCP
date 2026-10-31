"""Small synthetic implementation evaluation, not a retrieval-quality benchmark.

collect BINARY EVIDENCE_DIR captures actual MCP guidance and closes/cleans its
single owned fixture. judge CASE RESPONSE runs the case's behavioral contract
against coordinator-reviewed replacements in a disposable child working directory.
The child has a minimal environment and timeout; this is not an OS sandbox.
Fresh candidate agents receive the task/source with or without captured guidance.
Python blake3 is needed only to verify paged delivery in collect.
"""
import argparse
import copy
import contextlib
import hashlib
import json
import math
import os
from pathlib import Path
import queue
import re
import subprocess
import sys
import tempfile
import threading
import time


CASES = {
    "rename": {
        "task": "Trim surrounding whitespace from renamed layer names in layers.py.",
        "path": "layers.py", "concept": "rename_layer",
        "source": "def rename_layer(user, owner, name):\n    if user != owner:\n        raise PermissionError()\n    if not name:\n        raise ValueError()\n    return name\n",
        "policy": "Trim a layer name BEFORE checking whether it is empty. Reject whitespace-only names. Preserve owner authorization. Do not edit billing code.",
    },
    "zoom": {
        "task": "Implement can_show_zoom_control in zoom.py for the zoom UI.",
        "path": "zoom.py", "concept": "can_show_zoom_control",
        "source": "def save_zoom(user, owner, accessible, original_view, zoom):\n    if not accessible or user != owner:\n        raise PermissionError()\n    return zoom\n\ndef can_show_zoom_control(user, owner, accessible, original_view):\n    return False\n",
        "policy": "Show the zoom control only to the owner with access in the original view. Accepted exception: save_zoom intentionally permits an authorized owner to save from another accessible view. Do not add the UI original-view restriction to save_zoom.",
    },
    "format": {
        "task": "Change converted cached coordinates from an object to a two-element array in coordinates.py.",
        "path": "coordinates.py", "concept": "convert_coordinates",
        "source": "CONVERSION_VERSION = 1\n\ndef convert_coordinates(x, y):\n    return {'x': x, 'y': y}\n\ndef cached_coordinates(cache, x, y):\n    if cache['version'] == CONVERSION_VERSION:\n        return cache['value']\n    return convert_coordinates(x, y)\n\ndef render_coordinates(x, y):\n    return f'{x}, {y}'\n",
        "policy": "A change to stored conversion output MUST increment CONVERSION_VERSION so old cache values are recomputed. Preserve rendering behavior. Rendering-only changes do not increment the conversion version.",
    },
    "render": {
        "task": "Append the display suffix ' (map)' to layer labels in labels.py.",
        "path": "labels.py", "concept": "render_label",
        "source": "CONVERSION_VERSION = 1\n\ndef convert_coordinates(x, y):\n    return {'x': x, 'y': y}\n\ndef cached_coordinates(cache, x, y):\n    if cache['version'] == CONVERSION_VERSION:\n        return cache['value']\n    return convert_coordinates(x, y)\n\ndef render_label(name):\n    return name\n",
        "policy": "Layer labels are rendering-only. A label-format change MUST NOT increment CONVERSION_VERSION or invalidate stored coordinate caches. Stored-output changes do require a version increment.",
    },
    "csv": {
        "task": "Make include_header=True prepend a name/value header in export_rows in export.py.",
        "path": "export.py", "concept": "export_rows",
        "source": "def export_rows(rows, include_header=False):\n    return '\\n'.join(';'.join(str(cell) for cell in row) for row in rows)\n",
        "policy": "This export uses semicolon delimiters. Empty fields are accepted and must be preserved. The optional header is name;value. Generic comma-separated examples do not change this domain contract.",
    },
    "history": {
        "task": "Fix the rounding defect in a historical snapshot of round_value in rounding.py. Establish the historical rounding contract before editing.",
        "path": "rounding.py", "concept": "round_value",
        "source": "def round_value(value):\n    return int(value)\n",
        "policy": "CURRENT_LIVE_POLICY: round values to the nearest integer. This newer policy is not evidence of historical intent.",
        "historical_policy": "Historical instruction snapshot: round_value must round down using floor, including negative values. This snapshot predates the evaluated change.",
    },
    "recovery": {
        "task": "Implement bulk deletion in delete_records in bulk.py. Resolve any explicitly deferred evidence before editing.",
        "path": "bulk.py", "concept": "delete_records",
        "source": "def delete_records(records, caller):\n    return records\n",
        "policy": "Bulk deletion removes ONLY inactive records owned by the caller. Preserve active records and records owned by anyone else. Return the retained records in their original order.",
    },
}


class Client:
    def __init__(self, binary, config, stderr):
        environment = dict(os.environ, ENGRAM_CONFIG_PATH=str(config), ENGRAM_MULTI_CLIENT="false", ENGRAM_LOG_FILE="")
        self.process = subprocess.Popen([str(binary), "--no-multi-client"], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=stderr, text=True, encoding="utf-8", env=environment)
        self.messages = queue.Queue()
        self.counter = 0
        def read():
            try:
                for line in self.process.stdout:
                    self.messages.put(json.loads(line))
            except Exception as error:
                self.messages.put(error)
            finally:
                self.messages.put(None)
        threading.Thread(target=read, daemon=True).start()

    def rpc(self, method, params, notification=False):
        self.counter += 1
        request = dict(jsonrpc="2.0", method=method, params=params)
        if not notification:
            request["id"] = self.counter
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        if notification:
            return None
        while True:
            response = self.messages.get(timeout=60)
            if response is None or isinstance(response, Exception):
                raise RuntimeError(f"MCP output ended: {response}")
            if response.get("id") == self.counter:
                if "error" in response:
                    raise RuntimeError(response["error"])
                return response["result"]

    def tool(self, name, **arguments):
        result = self.rpc("tools/call", dict(name=name, arguments=arguments))
        if result.get("isError"):
            raise RuntimeError(result)
        return "\n".join(item.get("text", "") for item in result.get("content", []))

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=10)
        self.process.stdout.close()


def collect(binary, evidence):
    import blake3
    evidence = evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    binary = binary.resolve()
    with binary.open("rb") as executable:
        binary_hash = hashlib.file_digest(executable, "sha256").hexdigest()
    metadata = {"binary": str(binary), "binary_sha256": binary_hash,
                "limits": "Seven synthetic tasks; guidance consumption and workflow smoke only. Not a statistical quality score or real-project retrieval benchmark.", "cases": {}}
    with tempfile.TemporaryDirectory(prefix="instruction-outcomes-", dir=evidence) as owned:
        owned = Path(owned)
        config = owned / "config.yaml"
        config.write_text("allowed_roots: [" + json.dumps(str(owned)) + "]\ndata_dir: " + json.dumps(str(owned / "data")) +
                          "\nembedding_backend: fts_only\nllm_backend: none\nmulti_client: false\n", encoding="utf-8")
        with (evidence / "mcp-stderr.log").open("w", encoding="utf-8") as stderr:
            client = Client(binary, config, stderr)
            try:
                client.rpc("initialize", dict(protocolVersion="2024-11-05", capabilities={}, clientInfo=dict(name="instruction-outcomes", version="1")))
                client.rpc("notifications/initialized", {}, True)
                for name, case in CASES.items():
                    root = owned / name
                    root.mkdir()
                    (root / case["path"]).write_text(case["source"], encoding="utf-8")
                    (root / "AGENTS.md").write_text("Make focused changes. Preserve accepted behavior and authorization. Resolve incomplete evidence before claiming compliance.\n", encoding="utf-8")
                    topics = root / ".github/instructions"
                    topics.mkdir(parents=True)
                    (topics / "domain.instructions.md").write_text("---\napplyTo: '" + case["path"] + "'\n---\n" + case["policy"] + "\n", encoding="utf-8")
                    (topics / "billing.instructions.md").write_text("---\napplyTo: 'billing/**'\n---\nUnrelated billing examples use comma delimiters and nearest rounding.\n", encoding="utf-8")
                    index_started = time.monotonic()
                    client.tool("index_project", directory=str(root), project_name="outcome-" + name, project_type="general", wait=True)
                    index_seconds = time.monotonic() - index_started
                    # Unique project names and one new project per iteration.
                    projects = client.tool("list_projects")
                    matches = re.findall(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", projects)
                    pid = next(pid for pid in dict.fromkeys(matches)
                               if pid not in {entry["project_id"] for entry in metadata["cases"].values()})
                    request = dict(project_id=pid, story=case["task"], concepts=[case["concept"]], output_json=True)
                    if name == "history":
                        request["merged_before"] = "2024-01-01"
                    if name == "recovery":
                        request["story"] += " Preserve accepted behavior." * 3000
                    started = time.monotonic()
                    initial = json.loads(client.tool("get_change_set", **request))
                    initial_seconds = time.monotonic() - started
                    response = initial
                    delivery = initial.get("response_delivery", {})
                    recovery = None
                    if delivery.get("status") == "DEFERRED_CONTEXT":
                        chunks, page_count = [], 0
                        next_call = delivery["next_call"]
                        while next_call:
                            page = json.loads(client.tool(next_call["tool"], **next_call["arguments"]))
                            assert page["start_byte"] == len("".join(chunks).encode("utf-8"))
                            assert page["response_id"] == delivery["response_id"]
                            chunks.append(page["content"])
                            page_count += 1
                            assert page_count <= 32
                            next_call = None if page["complete"] else page["next_call"]
                        recovered = "".join(chunks).encode("utf-8")
                        assert blake3.blake3(recovered).hexdigest() == delivery["response_id"]
                        response = json.loads(recovered)
                        recovery = dict(response_id=delivery["response_id"], verified_blake3=True, bytes=len(recovered), pages=page_count)
                        if name == "recovery":
                            position = recovered.find(case["policy"].encode())
                            assert position >= page["start_byte"], "decisive qualification must occur on final recovery page"
                            assert case["policy"] not in json.dumps(initial)
                            recovery["decisive_guidance_on_final_page"] = True
                    guidance = response["project_policy_sources"]
                    if name == "history":
                        assert "CURRENT_LIVE_POLICY" not in json.dumps(guidance)
                    else:
                        assert case["policy"] in json.dumps(guidance, ensure_ascii=False), guidance
                    if name == "recovery":
                        assert recovery is not None
                    record = dict(task=case["task"], allowed_path=case["path"], starting_source=case["source"],
                                  initial_context=initial if recovery else guidance,
                                  recovered_guidance=guidance if recovery else None, recovery=recovery)
                    if name == "history":
                        historical = case["historical_policy"]
                        record["historical_followup"] = dict(content=historical, blake3=blake3.blake3(historical.encode()).hexdigest(),
                                                             origin="coordinator-supplied synthetic historical snapshot; not retrieved by Engram")
                    (evidence / (name + ".json")).write_text(json.dumps(record, indent=2), encoding="utf-8")
                    metadata["cases"][name] = dict(project_id=pid, latency_seconds=time.monotonic()-started,
                                                   indexing_seconds=index_seconds,
                                                   initial_context_seconds=initial_seconds,
                                                   recovery_seconds=time.monotonic()-started-initial_seconds if recovery else 0,
                                                   initial_response_bytes=len(json.dumps(initial, ensure_ascii=False).encode()), recovery=recovery)
                    print(json.dumps(dict(collected=name, **metadata["cases"][name])), flush=True)
            finally:
                client.close()
    metadata["owned_fixture_removed"] = not Path(owned).exists()
    (evidence / "collection.json").write_text(json.dumps(metadata, indent=2), encoding="utf-8")


def judge(name, proposal, working_root=None, timeout=5):
    # Review proposals before execution. Process isolation bounds failures and
    # avoids importing candidate code into the grading coordinator itself.
    environment = {key: os.environ[key] for key in ("SystemRoot", "WINDIR") if key in os.environ}
    root = Path(working_root or Path(__file__).resolve().parents[1] / "target/diagnostics")
    root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="candidate-check-", dir=root) as directory:
        environment.update(TEMP=directory, TMP=directory)
        completed = subprocess.run([sys.executable, "-I", "-B", str(Path(__file__).resolve()), "_judge", name],
                                   input=json.dumps(proposal), text=True, capture_output=True,
                                   cwd=directory, env=environment, timeout=timeout)
        if completed.returncode:
            raise RuntimeError(f"candidate check exited {completed.returncode}: {completed.stderr[-2000:]}")
        return json.loads(completed.stdout)


def judge_behavior(name, proposal):
    case = CASES[name]
    edits = proposal.get("edits", [])
    assert proposal.get("decision") == "edit", "candidate did not propose an implementation"
    assert edits and all(edit["path"] == case["path"] for edit in edits), "unrelated or missing edits"
    assert len(edits) == 1, "one complete replacement is required for this one-file task"
    namespace = {}
    exec(compile(edits[0]["replacement"], case["path"], "exec"), namespace)
    if name == "rename":
        function = namespace["rename_layer"]
        assert function("owner", "owner", "  North  ") == "North"
        for args, error in [(("owner", "owner", "  "), ValueError), (("other", "owner", "North"), PermissionError)]:
            try: function(*args)
            except error: pass
            else: raise AssertionError(f"required rejection absent: {args}")
    elif name == "zoom":
        show, save = namespace["can_show_zoom_control"], namespace["save_zoom"]
        assert show("o", "o", True, True)
        assert not show("o", "o", True, False), "zoom UI must require the original view"
        assert not show("x", "o", True, True)
        assert not show("o", "o", False, True)
        assert save("o", "o", True, False, 17) == 17
        for user, accessible in [("x", True), ("o", False)]:
            try: save(user, "o", accessible, False, 17)
            except PermissionError: pass
            else: raise AssertionError("unauthorized save accepted")
    elif name == "format":
        assert namespace["CONVERSION_VERSION"] > 1
        assert namespace["convert_coordinates"](2, 3) == [2, 3]
        assert namespace["cached_coordinates"]({"version": 1, "value": {"x": 9, "y": 9}}, 2, 3) == [2, 3]
        assert namespace["render_coordinates"](2, 3) == "2, 3"
    elif name == "render":
        assert namespace["render_label"]("North") == "North (map)"
        assert namespace["CONVERSION_VERSION"] == 1
        cached = {"x": 9, "y": 9}
        assert namespace["cached_coordinates"]({"version": 1, "value": cached}, 2, 3) is cached
        assert namespace["convert_coordinates"](2, 3) == {"x": 2, "y": 3}
    elif name == "csv":
        assert namespace["export_rows"]([["", 2]]) == ";2"
        assert namespace["export_rows"]([["", 2]], True) == "name;value\n;2"
    elif name == "history":
        assert all(namespace["round_value"](value) == math.floor(value) for value in (-1.8, -0.2, 0.2, 1.8))
    elif name == "recovery":
        records = [dict(id=1, owner="o", active=False), dict(id=2, owner="o", active=True),
                   dict(id=3, owner="x", active=False), dict(id=4, owner="x", active=True)]
        expected = copy.deepcopy(records[1:])
        assert namespace["delete_records"](records, "o") == expected
    return dict(case=name, behavior_passed=True, unrelated_edits=0)


def assess(candidates, output):
    proposals = json.loads(candidates.read_text(encoding="utf-8"))
    result = {"limits": "One fresh candidate per arm/task; seven synthetic contracts. No statistical coding-quality claim. Raw proposals are retained unchanged.", "arms": {}}
    for arm, entries in proposals.items():
        outcomes = {}
        for name in CASES:
            proposal = entries[name]
            outcome = dict(initial_decision=proposal["decision"], initial_edits=len(proposal.get("edits", [])))
            if name == "history":
                outcome["historical_evidence_withheld_correctly"] = proposal["decision"] == "needs_evidence" and not proposal.get("edits")
                proposal = entries.get("history_followup", proposal)
            if name == "recovery" and arm == "guided":
                outcome["requested_recovery_before_editing"] = proposal["decision"] == "recover" and not proposal.get("edits")
                proposal = entries.get("recovery_followup", proposal)
            if proposal["decision"] != "edit":
                outcome.update(behavior_passed=None, implementation="withheld pending evidence")
            else:
                try:
                    outcome.update(judge(name, proposal, working_root=output.parent))
                except Exception as error:
                    outcome.update(behavior_passed=False, failure=f"{type(error).__name__}: {error}")
            outcomes[name] = outcome
        result["arms"][arm] = outcomes
    output.write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    capture = commands.add_parser("collect")
    capture.add_argument("binary", type=Path)
    capture.add_argument("evidence", type=Path)
    assessment = commands.add_parser("judge")
    assessment.add_argument("case", choices=CASES)
    assessment.add_argument("response", type=Path)
    worker = commands.add_parser("_judge", help=argparse.SUPPRESS)
    worker.add_argument("case", choices=CASES)
    batch = commands.add_parser("assess")
    batch.add_argument("candidates", type=Path)
    batch.add_argument("output", type=Path)
    args = parser.parse_args()
    if args.command == "collect":
        collect(args.binary, args.evidence)
    elif args.command == "judge":
        print(json.dumps(judge(args.case, json.loads(args.response.read_text(encoding="utf-8")), working_root=args.response.parent)))
    elif args.command == "_judge":
        with contextlib.redirect_stdout(sys.stderr):
            outcome = judge_behavior(args.case, json.load(sys.stdin))
        print(json.dumps(outcome))
    else:
        assess(args.candidates, args.output)
