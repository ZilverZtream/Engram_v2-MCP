"""Fast precision gate for full-index planning boundary candidates.

This mirrors the cheap path-only filter in get_change_set. It lets candidate
precision be tuned against a checkout and saved evidence without compiling the
server, querying an LLM, or modifying the reference repository.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
from typing import Any, Iterable


ALLOWED_EXTENSIONS = (
    ".vb", ".cs", ".fs", ".js", ".ts", ".aspx", ".ascx", ".master", ".vbhtml",
    ".cshtml", ".config", ".json", ".yaml", ".yml", ".sql", ".sqlproj", ".dbml",
    ".edmx", ".asax", ".ashx", ".asmx", ".svc",
)
EXCLUDED_SEGMENTS = {
    "bin", "obj", "node_modules", "bower_components", "packages", "vendor", "dist",
    "coverage", ".git", ".vs",
}
STRONG_AUTH_NAMES = (
    "auth", "session", "login", "logout", "signin", "signout", "permission", "security",
    "credential", "token", "oauth", "saml", "mfa", "membership", "principal", "identity",
    "lockout", "useraccess", "tenantaccess",
)
WEAK_AUTH_NAMES = ("user", "role", "tenant", "access", "account")
BOUNDARY_ROLE_NAMES = (
    "controller", "service", "provider", "middleware", "filter", "manager", "repository",
    "store", "command", "handler", "config", "startup", "client", "policy", "gate",
)
SCOPE_NAMES = (
    "owner", "scope", "tenant", "customer", "account", "project", "organization", "parent",
    "child", "inherit", "override", "fallback", "copy", "clone", "move", "reassign",
    "transfer", "import",
)


def normalized(path: str) -> str:
    return path.replace("\\", "/").casefold()


def structurally_eligible(path: str) -> bool:
    value = normalized(path)
    if value.startswith(("diff:", "history:", "pr:")):
        return False
    if any(segment in EXCLUDED_SEGMENTS for segment in value.split("/")):
        return False
    if value.endswith((".min.js", ".min.css", ".map", ".refresh")):
        return False
    return value.endswith(ALLOWED_EXTENSIONS)


def relevant(path: str, auth_applicable: bool, scope_applicable: bool) -> bool:
    value = normalized(path)
    name = value.rsplit("/", 1)[-1]
    depth = value.count("/")
    infrastructure = (
        name in {"global.asax", "global.asax.vb", "global.asax.cs", "web.config"}
        and depth <= 2
    ) or any(term in name for term in ("startup", "middleware", "routeconfig", "bundleconfig", "appsettings"))
    strong_auth = any(term in name for term in STRONG_AUTH_NAMES)
    weak_auth = any(term in name for term in WEAK_AUTH_NAMES)
    role = any(term in name for term in BOUNDARY_ROLE_NAMES)
    auth = auth_applicable and (strong_auth or (weak_auth and role))
    scope = scope_applicable and any(term in value for term in SCOPE_NAMES)
    return infrastructure or auth or scope


def repository_paths(root: Path) -> Iterable[str]:
    for current, directories, files in os.walk(root):
        directories[:] = [
            directory for directory in directories if directory.casefold() not in EXCLUDED_SEGMENTS
        ]
        current_path = Path(current)
        for name in files:
            yield (current_path / name).relative_to(root).as_posix()


def retrieved_paths(evidence: dict[str, Any]) -> set[str]:
    return {
        normalized(str(row.get("path")))
        for family in ("files", "asset_dependencies", "caller_dependencies")
        for row in evidence.get(family, [])
        if row.get("path")
    }


def suffix_present(paths: Iterable[str], expected: str) -> bool:
    expected = normalized(expected)
    return any(normalized(path).endswith(expected) for path in paths)


def evaluate(
    root: Path,
    evidence: dict[str, Any],
    auth_applicable: bool,
    scope_applicable: bool,
    expectations: dict[str, Any] | None = None,
) -> dict[str, Any]:
    retrieved = retrieved_paths(evidence)
    candidates = sorted({
        path for path in repository_paths(root)
        if structurally_eligible(path) and relevant(path, auth_applicable, scope_applicable)
    }, key=normalized)
    unretrieved = [path for path in candidates if normalized(path) not in retrieved]
    expectations = expectations or {}
    required = [str(path) for path in expectations.get("required_leads", [])]
    forbidden = [str(path) for path in expectations.get("forbidden_leads", [])]
    maximum = int(expectations.get("max_unretrieved_candidates", 0)) or None
    checks = {
        "required_leads_present": all(suffix_present(unretrieved, path) for path in required),
        "forbidden_leads_absent": all(not suffix_present(candidates, path) for path in forbidden),
        "candidate_budget": maximum is None or len(unretrieved) <= maximum,
    }
    return {
        "repository_files": sum(1 for _ in repository_paths(root)),
        "retrieved_paths": len(retrieved),
        "eligible_relevant_paths": len(candidates),
        "unretrieved_candidates": len(unretrieved),
        "candidates": unretrieved,
        "expectations": {
            "required_leads": required,
            "forbidden_leads": forbidden,
            "max_unretrieved_candidates": maximum,
        },
        "checks": checks,
        "ready": all(checks.values()),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--fixture", type=Path)
    parser.add_argument("--auth", action="store_true")
    parser.add_argument("--scope", action="store_true")
    parser.add_argument("--require-ready", action="store_true")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    if not args.repo.is_dir():
        raise ValueError(f"repository does not exist: {args.repo}")
    evidence = json.loads(args.evidence.read_text(encoding="utf-8-sig"))
    fixture = json.loads(args.fixture.read_text(encoding="utf-8-sig")) if args.fixture else {}
    report = evaluate(
        args.repo, evidence, args.auth, args.scope,
        fixture.get("boundary_candidate_expectations"),
    )
    rendered = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0 if not args.require_ready or report["ready"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
