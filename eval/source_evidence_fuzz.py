"""Differential checks through the public MCP transport on a disposable Git project.

Example: python eval/source_evidence_fuzz.py --binary PATH --runs 8 --output results.json
The seed regenerates the checkout and queries. Expected matches come from Python
re over independently read Git blobs. A wrong answer and an unavailable tool
both fail the run. Timing is diagnostic only.
"""
import argparse
import hashlib
import json
import os
import random
import re
import subprocess
import tempfile
import time
from pathlib import Path

from instruction_outcomes import Client


def git(repo, *args):
    env = dict(os.environ, GIT_AUTHOR_DATE="2000-01-01T00:00:00Z",
               GIT_COMMITTER_DATE="2000-01-01T00:00:00Z")
    return subprocess.check_output(["git", "-C", str(repo), "-c", "core.autocrlf=false", *args],
                                   encoding="utf-8", env=env)


def checkpoint(repo, name):
    git(repo, "add", ".")
    git(repo, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
        "-c", "commit.gpgsign=false", "commit", "-qm", name)
    return git(repo, "rev-parse", "HEAD").strip()


def semantic_corpus(repo, rng):
    # Names, physical mappings, statement ordering and casing vary independently.
    tag = str(rng.randrange(10**7))
    table, entity, member = "Batch" + tag, "Record" + tag, "Entries" + tag
    owner = "Ledger" + tag
    delete, bulk, read, make, example = [prefix + tag for prefix in ("Prune", "Copy", "Fetch", "Build", "Describe")]
    db = rng.choice(["db", "context", "connection"])
    qualified = rng.choice([f"dbo.{table}", f"[dbo].[{table}]", table])
    operation = rng.choice([f"DELETE FROM {qualified} WHERE Id = {{0}}",
                            f"UPDATE {qualified} SET Id = {{0}}",
                            f"INSERT INTO {qualified} (Id) VALUES ({{0}})"])
    first, rest = operation.split(" ", 1)
    argument = rng.choice([f'"{operation}"', f'"{first} " & "{rest}"',
                           f'"{first} " & _\n            "{rest}"'])
    bodies = {
        delete: [f'{db}.ExecuteCommand({argument}, id)'],
        bulk: [f'{db}.Writer.DestinationTableName = "{qualified}"'],
        read: [f'Dim rows = {db}.{member}.ToList()'],
        make: [f'Dim row As New {entity}()'],
        example: [f'\' {db}.ExecuteCommand("DELETE FROM {qualified}")',
                  f'Dim sample = "DELETE FROM {qualified}"',
                  f'{db}.ExecuteCommand("SELECT 1 /* DELETE FROM {qualified} */")',
                  f'{db}.ExecuteCommand("SELECT \'UPDATE {qualified} SET Id=7\'")'],
    }
    keys = list(bodies)
    rng.shuffle(keys)
    source = [f"Public Class {owner}"]
    for name in keys:
        source += [f"    Public Sub {name}({db} As SampleDataContext, id As Integer)"]
        source += ["        " + line for line in bodies[name]]
        source += ["    End Sub", ""]
    source += ["End Class", f"Public Class {entity}", "End Class"]
    (repo / "store.vb").write_text("\n".join(source) + "\n")
    (repo / "model.dbml").write_text(
        '<Database Class="SampleDataContext" xmlns="http://schemas.microsoft.com/linqtosql/dbml/2007">'
        f'<Table Name="dbo.{table}" Member="{member}"><Type Name="{entity}">'
        '<Column Name="Id" Type="System.Int32" IsPrimaryKey="true"/></Type></Table></Database>')
    (repo / "schema.sql").write_text(f"CREATE TABLE dbo.{table} (Id INT PRIMARY KEY);\n")
    lock = "claim" + tag + ".sql"
    (repo / lock).write_text(f"SELECT Id FROM dbo.{table} WITH (UPDLOCK, HOLDLOCK);\n")
    # Assign names independently of semantics so directory order cannot supply
    # the expected ranking. Only the generated executable statements are witnesses.
    variant = random.Random(int(tag))
    statements = [
        ("vb", variant.choice(["Dim tx = conn.BeginTransaction(IsolationLevel.Serializable)",
                               "Dim tx = New System.Transactions.TransactionScope()"])),
        ("sql", variant.choice([f"SELECT Id FROM {table} FOR UPDATE;",
                                f"SELECT Id FROM {table} WITH (XLOCK);"])),
        ("sql", variant.choice(["SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;", "BEGIN IMMEDIATE;"])),
        ("vb", variant.choice(["<Serializable()>", "<System.Serializable>"]) + "\nPublic Class Envelope\nEnd Class"),
        ("cs", "[System.Serializable]\npublic class Packet {}"),
        ("ts", "export function isSerializable(value: unknown) { return !!value; }"),
        ("cs", "class TransactionScopeOptions { bool IsSerializable; }"),
        ("vb", "' conn.BeginTransaction(IsolationLevel.Serializable)"),
        ("sql", "-- SELECT * FROM items FOR UPDATE"),
    ]
    variant.shuffle(statements)
    lock_paths = [lock]
    for i, (extension, statement) in enumerate(statements):
        path = f"sample{tag}_{i}.{extension}"
        (repo / path).write_text(statement + "\n")
        if statement.startswith(("Dim tx", "SELECT", "SET TRANSACTION", "BEGIN IMMEDIATE")):
            lock_paths.append(path)
    readers = ["Public Class Readers"]
    for i in range(variant.randrange(80, 101)):
        readers += [f"    Public Sub Observe{i:03d}(db As SampleDataContext)",
                    f'        db.ExecuteQuery(Of Integer)("SELECT Id FROM {qualified}")',
                    f"        Dim rows = db.{member}.ToList()", "    End Sub"]
    (repo / "readers.vb").write_text("\n".join([*readers, "End Class", ""]))
    (repo / "notes.md").write_text("Consider database row locks, race conditions, updates and transactions.\n")
    (repo / "CLAUDE.md").write_text("Preserve the complete ordering invariant.\n" * 1200)
    guides = repo / ".github" / "instructions"
    guides.mkdir(parents=True)
    (guides / "storage.instructions.md").write_text(
        "---\napplyTo: '**/*.vb'\n---\nPreserve the transaction boundary when writing a row.\n")
    return dict(table=table, entity=entity, member=member, owner=owner, delete=delete,
                bulk=bulk, read=read, make=make, example=example, lock=lock, lock_paths=lock_paths)


def populate(repo, rng):
    words = ["Rune" + str(rng.randrange(10**7)) for _ in range(12)]
    paths = ["src/alpha.py", "src/nested/beta.py", "extra/gamma.py"]
    for path in paths:
        file = repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        lines = [f"# {rng.choice(words)}{rng.choice([' ', '|', '.', '-'])}{rng.choice(words)}"
                 for _ in range(18)]
        file.write_text("\n".join(lines) + "\n", encoding="utf-8")
    (repo / "src" / "shadow.ts").write_text("// " + " ".join(words) + "\n")
    facts = semantic_corpus(repo, rng)
    base = checkpoint(repo, "Initial corpus")
    git(repo, "checkout", "-qb", "work")
    file = repo / paths[0]
    file.write_text("# new prefix\n" + file.read_text() + "# moved revision\n")
    (repo / paths[-1]).rename(repo / "extra/renamed.py")
    head = checkpoint(repo, "Shift source coordinates and rename a file")
    dirty = f"Dirty{rng.randrange(10**7)}"
    file.write_text("# " + dirty + "\n" + file.read_text())
    (repo / "extra/dirty.py").write_text("# " + dirty + "\n")
    return words, base, head, dirty, facts


def corpus(repo, revision):
    if revision is None:
        return {p.relative_to(repo).as_posix(): p.read_text() for p in repo.rglob("*.py")}
    paths = git(repo, "ls-tree", "-r", "--name-only", revision).splitlines()
    return {p: git(repo, "show", revision + ":" + p) for p in paths if p.endswith(".py")}


def regex_cases(rng, words, n):
    def atom():
        word = rng.choice(words)
        return rng.choice([word, word.lower(), word[:4] + r"\d+", re.escape(word) + "?",
                           r"\b" + word, r"[Rr]" + word[1:], r"\|", word + "$"])

    def expression(depth):
        if not depth or rng.random() < .45:
            return atom()
        left, right = expression(depth - 1), expression(depth - 1)
        return rng.choice([f"(?:{left}|{right})", left + "|" + right,
                           left + r"[ .|\-]" + right, left + ".*" + right])

    for i in range(n):
        a, b = rng.sample(words, 2)
        forms = [a + "|" + b, a.lower() + "|" + b.lower(), a + r"\|" + b,
                 a + ".*" + b, "(" + a + "|" + b + ")", a + "|z", a + r"\." + b,
                 a + "?|" + b, a + "|" + a, a + r"\s+" + b]
        pattern = forms[i % len(forms)] if i < len(forms) or i % 3 == 0 else expression(rng.randint(1, 3))
        yield pattern, rng.choice(["", "src/", "extra/"]), i % 3 == 0


def run(args, seed):
    rng = random.Random(seed)
    binary = args.binary.resolve(strict=True)
    with binary.open("rb") as f:
        digest = hashlib.file_digest(f, "sha256").hexdigest()
    report = {"seed": seed, "binary_sha256": digest, "checks": [], "failures": []}
    args.output.parent.mkdir(exist_ok=True, parents=True)
    with tempfile.TemporaryDirectory(prefix="source-evidence-", dir=args.output.parent) as directory:
        owned = Path(directory).resolve()
        repo = owned / "repo"
        repo.mkdir()
        git(repo, "init", "-q", "-b", "base")
        words, base, head, dirty, facts = populate(repo, rng)
        snapshots = {rev: corpus(repo, rev) for rev in [None, head, base]}
        config = owned / "config.yml"
        config.write_text("allowed_roots: [" + json.dumps(str(repo)) + "]\ndata_dir: " +
                          json.dumps(str(owned / "data")) +
                          "\nembedding_backend: fts_only\nllm_backend: none\nmulti_client: false\n")
        with (owned / "stderr.log").open("w", encoding="utf-8") as log:
            client = Client(binary, config, log, temporary_root=owned)
            try:
                client.rpc("initialize", dict(protocolVersion="2024-11-05", capabilities={},
                           clientInfo=dict(name="source-evidence-check", version="1")))
                client.rpc("notifications/initialized", {}, True)
                client.tool("index_project", directory=str(repo), project_name="source-evidence",
                            project_type="general", wait=True)
                project = re.search(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}",
                                    client.tool("list_projects")).group()
                # The indexed bytes and the current disk bytes now also disagree.
                stale = repo / "src" / "nested" / "beta.py"
                stale.write_text("# " + dirty + "\n" + stale.read_text())
                snapshots[None] = corpus(repo, None)
                def check(family, request, expected, extract):
                    started = time.monotonic()
                    try:
                        arguments = dict(project_id=project, output_json=True, max_tokens=6000,
                                         max_results=180, **request)
                        body = json.loads(client.tool("grep_project", **arguments))
                        page = body
                        offset = 0
                        while (following := page.get("response_page", {}).get("next_offset")) is not None:
                            if not offset < following <= arguments["max_results"]:
                                raise ValueError(f"Invalid grep continuation {following} after {offset}")
                            offset = following
                            page = json.loads(client.tool("grep_project", **arguments, offset=offset))
                            body["matches"].extend(page["matches"])
                        actual = extract(body)
                        passed = expected == actual
                    except Exception as exc:
                        actual = str(exc)[:1400]
                        body = {}
                        passed = False
                    report["checks"].append(dict(family=family, passed=passed, tier=body.get("tier_used"),
                                                 ms=round(1000 * (time.monotonic()-started))))
                    if not passed:
                        report["failures"].append(dict(family=family, request=request, expected=expected, actual=actual))
                queries = list(regex_cases(rng, words, args.cases))
                for rev in snapshots:
                    for pattern, prefix, sensitive in queries + [(dirty, "", True)]:
                        flags = 0 if sensitive else re.IGNORECASE
                        expected = sorted((p, i, line) for p, source in snapshots[rev].items()
                                          if p.startswith(prefix) for i, line in enumerate(source.splitlines(), 1)
                                          if re.search(pattern, line, flags))
                        req = dict(pattern=pattern, regex=True, language="python", case_sensitive=sensitive, path_prefix=prefix)
                        if rev:
                            req["as_of_rev"] = rev
                        def lines(body):
                            return sorted({(m["file_path"], m["line"], m["line_text"])
                                           for m in body["matches"]})
                        check("pinned_matches" if rev else "indexed_matches", req, expected, lines)
                        if not rev and re.fullmatch(r"\w+\|\w{3,}", pattern):
                            check("alternation_prefilter", req, "term_narrowed", lambda b: b["tier_used"])
                        if rev:
                            check("revision_identity", req, rev, lambda b: b["source_basis"]["commit"])
                def verify(name, tool, arguments, judge):
                    started = time.monotonic()
                    try:
                        reply = client.tool(tool, project_id=project, **arguments)
                        passed = bool(judge(reply))
                    except Exception as exc:
                        passed, reply = False, str(exc)
                    report["checks"].append(dict(family=name, passed=passed,
                                                 ms=round(1000 * (time.monotonic() - started))))
                    if not passed:
                        report["failures"].append(dict(family=name, tool=tool, request=arguments,
                                                       observed=reply[:6000], corpus=facts))
                for rev in (head, base):
                    path = "src/alpha.py"
                    line = rng.randint(2, 10)
                    expected_line = snapshots[rev][path].splitlines()[line - 1]
                    verify("body_coordinate", "get_full_method_body",
                           dict(as_of_rev=rev, file_path=path, line_start=line, line_end=line,
                                context_lines=0, output_json=True),
                           lambda r: expected_line in r and json.loads(r)["source_basis"]["commit"] == rev)
                for name, tool, args_for_tool in [
                    ("pinned_reference", "find_symbol_references", dict(symbol_name=facts["delete"])),
                    ("pinned_footprint", "get_concept_footprint", dict(concept=facts["delete"])),
                    ("pinned_pattern", "find_implementation_pattern", dict(pattern_query="database row locking")),
                ]:
                    def pinned_sites(reply):
                        body = json.loads(reply)
                        sites = body["source_sites"]
                        if body["source_basis"]["commit"] != base or not sites:
                            return False
                        expected = "UPDLOCK" if name == "pinned_pattern" else facts["delete"]
                        return any(expected in row["line_text"] for row in sites) and all(
                            git(repo, "show", base + ":" + row["file_path"]).splitlines()[row["line"]-1]
                            == row["line_text"] for row in sites)
                    verify(name, tool, dict(as_of_rev=base, **args_for_tool), pinned_sites)
                for name in ("bulk", "delete"):
                    verify("writer_" + name, "get_concept_footprint",
                           dict(concept=facts["table"], max_tokens=4000),
                           lambda r, key=name: any(facts[key].lower() in line.lower() and
                                                   "write:queries_table" in line for line in r.splitlines()))
                raw_vb = git(repo, "show", base + ":store.vb").splitlines()
                # The handwritten generator emits two writer statements. Code
                # comments and example strings contain identical SQL words.
                expected_writes = {i for i, text in enumerate(raw_vb, 1)
                                   if ".DestinationTableName =" in text or
                                   re.match(r'^\s*\w+\.ExecuteCommand\("(DELETE|UPDATE|INSERT)\b', text)}
                def pinned_writes(reply):
                    rows = json.loads(reply)["source_sites"]
                    observed = {r["line"] for r in rows if r["file_path"] == "store.vb" and
                                r["evidence"].startswith("SQL write")}
                    return len(expected_writes) == 2 and expected_writes == observed
                verify("pinned_write_coordinates", "find_symbol_references",
                       dict(as_of_rev=base, symbol_name=facts["table"]), pinned_writes)
                verify("indexed_writer_references", "find_symbol_references",
                       dict(symbol_name=facts["table"], edge_kind_filter=["queries_table"]),
                       lambda r: all(any(facts[key].lower() in line.lower() and "[write]" in line
                                         for line in r.splitlines()) for key in ("delete", "bulk")))
                verify("comment_is_not_writer", "get_concept_footprint", dict(concept=facts["table"]),
                       lambda r: not any(facts["example"].lower() in line.lower() and
                                         "write:queries_table" in line for line in r.splitlines()))
                verify("mapped_entity", "find_symbol_references", dict(symbol_name=facts["entity"]),
                       lambda r: facts["member"].lower() in r.lower() and any(
                           facts["read"].lower() in line.lower() and
                           any(mark in line.lower() for mark in ("[read]", "[mapped property access]"))
                           for line in r.splitlines()))
                verify("entity_construction", "find_symbol_references", dict(symbol_name=facts["entity"]),
                       lambda r: any(facts["make"].lower() in line.lower() and
                                     "type construction" in line for line in r.splitlines()))
                verify("pattern_witness", "find_implementation_pattern",
                       dict(pattern_query="race-safe database row update", output_json=True, max_examples=1,
                            max_tokens=4000), lambda r: facts["lock"] in r and "UPDLOCK" in r)
                for rev in (None, base):
                    def ranked_locks(raw):
                        result = json.loads(raw)
                        if rev:
                            paths = [row["file_path"] for row in result["source_sites"]]
                        elif "exemplars" in result:
                            paths = [row["path"] for row in result["exemplars"]]
                        else:
                            paths = re.findall(r"^#\d+ (.+?):\d+$", result["first_page"], re.M)
                        return sorted(paths) == sorted(facts["lock_paths"])
                    params = dict(pattern_query="database row locking for conditional writes",
                                  output_json=True, max_examples=len(facts["lock_paths"]) + 2, max_tokens=4000)
                    if rev:
                        params["as_of_rev"] = rev
                    verify("lock_ranking_pinned" if rev else "lock_ranking_indexed",
                           "find_implementation_pattern", params, ranked_locks)
                def first_page_writers(raw):
                    page = json.loads(raw)["first_page"] if raw.lstrip().startswith("{") else raw
                    return all(any(facts[key] in row and "[write]" in row for row in page.splitlines())
                               for key in ("delete", "bulk"))
                verify("writers_under_reader_pressure", "find_symbol_references",
                       dict(symbol_name=facts["table"], edge_kind_filter=["queries_table"],
                            max_incoming=150, max_tokens=2000), first_page_writers)
                verify("small_source_and_guidance", "get_full_method_body",
                       dict(file_path="store.vb", line_start=1, line_end=2, context_lines=0,
                            max_tokens=4000, output_json=True),
                       lambda r: facts["owner"] in r and "CLAUDE.md" in r and
                       "storage.instructions.md" in r and len(r.encode()) <= 16000)
                def policy_read(raw):
                    policy = json.loads(raw)["project_policy_sources"]
                    omitted = next(o for o in policy["omissions"]
                                   if o.get("path", o.get("file_path")) == "CLAUDE.md")
                    action = omitted["read_action"]
                    args = dict(action["arguments"], max_tokens=4000)
                    result = client.tool(action["tool"], **args)
                    following = json.loads(result)["file_excerpt"]["next_page"]["arguments"]
                    return ("Preserve the complete ordering invariant." in result and bool(args["expected_file_blake3"])
                            and following["max_tokens"] == args["max_tokens"]
                            and following["expected_file_blake3"] == args["expected_file_blake3"])
                verify("oversized_policy_recovery", "get_full_method_body",
                       dict(file_path="store.vb", line_start=1, line_end=2, context_lines=0,
                            max_tokens=4000, output_json=True), policy_read)
                # Removing only the executable witnesses must remove the historical
                # results even though a newer working index still contains them.
                for path in facts["lock_paths"]:
                    (repo / path).unlink()
                decoy_revision = checkpoint(repo, "Keep only lexical lookalikes")
                verify("lock_absence_at_revision", "find_implementation_pattern",
                       dict(as_of_rev=decoy_revision, pattern_query="database row locking",
                            output_json=True, max_examples=8),
                       lambda raw: json.loads(raw)["source_sites"] == [] and
                       json.loads(raw)["source_basis"]["commit"] == decoy_revision)
                client.tool("update_project", project_id=project, max_commits=0, wait=True,
                            index_antipatterns=False)
                verify("lock_absence_after_update", "find_implementation_pattern",
                       dict(pattern_query="database row locking", output_json=True, max_examples=8),
                       lambda raw: json.loads(raw)["exemplars"] == [])
            finally:
                client.close()
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--seed", type=int, help="Replay a seed or use it to generate a repeatable campaign")
    parser.add_argument("--runs", type=int, default=1, help="Number of independent generated repositories")
    parser.add_argument("--cases", type=int, default=30)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.cases < 1 or args.runs < 1:
        parser.error("--cases and --runs must be positive")
    seed = args.seed if args.seed is not None else random.SystemRandom().getrandbits(64)
    generator = random.Random(seed)
    runs = []
    for index in range(args.runs):
        current_seed = seed if index == 0 else generator.getrandbits(64)
        print(f"Run {index + 1}/{args.runs}, seed {current_seed}", flush=True)
        result = run(args, current_seed)
        runs.append(result)
        payload = result if args.runs == 1 else {"campaign_seed": seed, "runs": runs}
        args.output.write_text(json.dumps(payload, indent=2), encoding="utf-8")
        print(f"{len(result['checks'])} checks, {len(result['failures'])} failures; {args.output}", flush=True)
    raise SystemExit(any(r["failures"] for r in runs))
