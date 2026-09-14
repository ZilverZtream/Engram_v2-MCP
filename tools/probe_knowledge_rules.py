"""Probe selected Engram knowledge namespaces without dumping full results.

This is a small diagnostic utility. It deliberately prints only source/path
metadata and bounded snippets so large review corpora cannot flood the caller.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
from typing import Any


def rpc(proc: subprocess.Popen[str], sequence: int, method: str, params: dict[str, Any]) -> dict[str, Any]:
    assert proc.stdin is not None and proc.stdout is not None
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": sequence, "method": method, "params": params}) + "\n")
    proc.stdin.flush()
    while True:
        line = proc.stdout.readline()
        if not line:
            error = proc.stderr.read()[-2000:] if proc.stderr else ""
            raise RuntimeError(error or "Engram server closed stdout")
        response = json.loads(line)
        if response.get("id") == sequence:
            return response


def result_text(response: dict[str, Any]) -> str:
    result = response.get("result") or {}
    return "\n".join(
        item.get("text", "")
        for item in result.get("content", [])
        if item.get("type") == "text"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--project-id", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("queries", nargs="+")
    args = parser.parse_args()

    env = os.environ.copy()
    env["ENGRAM_CONFIG_PATH"] = str(args.config)
    env["RUST_LOG"] = "warn"
    proc = subprocess.Popen(
        [str(args.exe), "--no-multi-client"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        env=env,
        bufsize=1,
    )

    captured: list[dict[str, str]] = []
    sequence = 1
    try:
        rpc(proc, sequence, "initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "knowledge-rule-probe", "version": "1"},
        })
        assert proc.stdin is not None
        proc.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        proc.stdin.flush()

        for namespace in ("antipattern", "quality_gate", "memory_bank"):
            for query in args.queries:
                sequence += 1
                response = rpc(proc, sequence, "tools/call", {
                    "name": "search_memory",
                    "arguments": {
                        "project_id": args.project_id,
                        "query": query,
                        "namespace": namespace,
                        "search_scope": "code",
                        "max_results": 5,
                        "fts_mode": "loose",
                        "semantic": False,
                        "use_mmr": False,
                        "include_content": False,
                        "include_user_memory": False,
                    },
                })
                text = result_text(response)
                captured.append({"namespace": namespace, "query": query, "result": text})
                status = "no_hits" if "result: no_hits" in text else "hits"
                print(f"{namespace:13} {status:7} {query}")

        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(captured, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    finally:
        if proc.stdin:
            proc.stdin.close()
        proc.wait(timeout=30)


if __name__ == "__main__":
    main()
