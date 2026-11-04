"""One bounded, owned MCP new/old/new source-materialization experiment.

This tests a candidate downgrade procedure, not a promised compatibility feature.
Only generated fixture data is mutable; retained evidence lives in --evidence.
"""
import argparse
import contextlib
import hashlib
import json
import mmap
import os
import queue
from pathlib import Path
import re
import subprocess
import tempfile
import threading
import time

from instruction_outcomes import Client
from downgrade_policy_preflight import (
    OLD_POLICY_PROFILE, PreflightRefusal, inspect_inventory, strict_json,
)
import blake3
from edit_resource_workload import (
    child_identity, footprint, logical_writes, remove_owned_fixture, require, sample,
)


NEW_HASH = "530b90ab94d65def3917eb78961d8ef5cf917fa2f601e480ac510999bbf96e28"
HELPER_HASH = "4db263193a6f8b86f97736fa0aa99a484059dc3bf321f18dc3809f3fcee98fa0"
FLAGS = 0x4000 | 0x08000000


class CoverageFailure(RuntimeError):
    pass


class RpcFailure(RuntimeError):
    pass



class ProcedureClient(Client):
    """Own the actual stdout reader while leaving the frozen base driver alone."""
    def __init__(self, binary, config, stderr, *, creationflags=0, temporary_root=None, owned_processes=None):
        environment = dict(os.environ, ENGRAM_CONFIG_PATH=str(config), ENGRAM_MULTI_CLIENT="false", ENGRAM_LOG_FILE="")
        root = Path(temporary_root).resolve(strict=True)
        require(root.is_dir(), "Temporary root must be an owned directory")
        environment.update(TEMP=str(root), TMP=str(root))
        self.process = subprocess.Popen([str(binary), "--no-multi-client"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=stderr, text=True, encoding="utf-8", env=environment,
            creationflags=creationflags)
        if owned_processes is not None:
            owned_processes.append(self.process)
        self.messages = queue.Queue(maxsize=16)
        self.counter = 0
        def read():
            try:
                while True:
                    line = self.process.stdout.readline(16 * 1024**2 + 1)
                    if not line:
                        break
                    require(len(line) <= 16 * 1024**2, "Owned MCP response exceeded16MiB")
                    self.messages.put(strict_json(line), timeout=5)
            except Exception as error:
                try:
                    self.messages.put(error, timeout=5)
                except queue.Full:
                    pass
            finally:
                try:
                    self.messages.put(None, timeout=5)
                except queue.Full:
                    pass
        self.reader = threading.Thread(target=read, name="s18-owned-mcp-reader", daemon=True)
        try:
            self.reader.start()
        except BaseException:
            self.process.terminate()
            self.process.wait(timeout=10)
            self.process.stdin.close()
            self.process.stdout.close()
            raise

    def close(self):
        try:
            super().close()
        finally:
            self.reader.join(timeout=10)
            require(not self.reader.is_alive(), "Owned MCP stdout reader did not finish; teardown unsafe")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def source(number, revision):
    declaration = f"pub fn downgrade_marker_{number:02}_revision_{revision:02}() -> usize {{ {17000 + number * 101 + revision} }}"
    lines = [declaration + "\n"]
    ordinal = 0
    while len("".join(lines)) < 1950:
        lines.append(f"pub fn sibling_context_{number:02}_{ordinal:02}() -> usize {{ {number * 100 + ordinal} }}\n")
        ordinal += 1
    return "".join(lines), declaration


def run(args):
    require(os.name == "nt", "Windows process counters are required")
    started = time.monotonic()
    deadline = started + 120
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=False)
    report = dict(
        started_unix=time.time(), candidate_procedure=(
            "new public adoption; authority preflight refuses before full materialization/old launch; new public reimport and real edit"
            if args.mode == "adopted-authority-refusal" else
            "new index; two updates; authority preflight; full materialization; repeated preflight; old read/edit; new read or exact explicit recovery; next edit"),
        old_launch_authorized=False,
        binary_scope="Exact caller-selected executable hashes; no build/install is performed. Historical default is530b90ab; source-only, representable-policy and unsupported-adoption refusal modes are reported separately.",
        mode=args.mode, expected_new_sha256=args.expected_new_sha256.lower(),
        safety_scope="Operator/harness procedure on one exclusively owned fixture, not an application minimum-version fence. Direct old executable launches can bypass it.",
        limits=dict(seconds=120, private_bytes=512 * 1024**2, fixture_bytes=512 * 1024**2,
                    cumulative_process_logical_write_bytes=1024**3, log_bytes=32 * 1024**2,
                    fixture_entries=20000, sample_seconds=0.25,
                    interpretation="Sampled protective stops, not OS quotas. Process logical writes include shutdown, not physical SSD writes."),
        calls=[], phases=[], samples=[], footprint_samples=[], owned_processes=[],
        cleanup_condition="Retain this evidence directory for audit review; generated owned fixture is removed only after every exact child is reaped and its monitor has stopped.",
        driver_sha256=digest(Path(__file__)),
    )
    children = []
    readers = []
    stop = threading.Event()
    monitor = None
    phase = "preflight"
    fixture = None
    current = dict(client=None, phase=phase)

    def check():
        if "budget_stop" in report:
            raise RuntimeError(f"Protective stop: {report['budget_stop']}")
        require(time.monotonic() < deadline, "Absolute experiment deadline reached")

    @contextlib.contextmanager
    def guarded_fixture():
        directory = Path(tempfile.mkdtemp(prefix="edit-workload-", dir=evidence))
        try:
            yield directory
        finally:
            stop.set()
            for child in children:
                if child.poll() is None:
                    try:
                        child.terminate()
                        child.wait(timeout=10)
                    except Exception as error:
                        report.setdefault("child_cleanup_errors", []).append(dict(pid=child.pid, error=repr(error)))
            alive = [child_identity(child) for child in children if child.poll() is None]
            monitor_alive = monitor is not None and monitor.is_alive()
            reader_alive = [reader.name for reader in readers if reader.is_alive()]
            if alive or monitor_alive or reader_alive:
                # A monitor may still hold this fixture path even after all
                # children exit. Never let child-only teardown remove it.
                report["retained_owned_fixture"] = dict(
                    path=str(directory), children=alive, monitor_alive=monitor_alive, reader_alive=reader_alive,
                    monitor_name=monitor.name if monitor is not None else None,
                    driver_pid=os.getpid(),
                    reason="Owned child, stdout reader or fixture monitor remains live; fixture was not removed",
                    cleanup_condition="Wait for this recorded driver process to exit so its owned monitor/readers cannot access the fixture; verify recorded child PID/creation-time/command identities are no longer live before permitted exact-path teardown.")
                raise RuntimeError("Owned child or fixture monitor remains live; fixture preserved")
            remove_owned_fixture(directory, evidence)

    def call(client, name, **arguments):
        check()
        entry = dict(phase=current["phase"], tool=name, arguments=arguments)
        report["calls"].append(entry)
        try:
            result = client.rpc("tools/call", dict(name=name, arguments=arguments))
            entry["raw_response"] = result
        except Exception as error:
            entry["transport_error"] = repr(error)
            raise
        check()
        text = "\n".join(item.get("text", "") for item in result.get("content", []))
        if result.get("isError"):
            raise RpcFailure(text)
        return text

    def observe():
        while not stop.wait(0.25):
            try:
                total_writes = sum(logical_writes(child) for child in tuple(children))
                client = current["client"]
                observed = dict(at=time.monotonic(), phase=current["phase"],
                                cumulative_logical_write_bytes=total_writes)
                if client is not None and client.process.poll() is None:
                    try:
                        observed.update(sample(client.process))
                    except OSError:
                        if client.process.poll() is None:
                            raise
                report["samples"].append(observed)
                size = footprint(fixture, 20000)
                report["footprint_samples"].append(dict(at=time.monotonic(), phase=current["phase"], **size))
                logs = sum(path.stat().st_size for path in evidence.glob("*-stderr.log"))
                reason = None
                if time.monotonic() >= deadline:
                    reason = "absolute_120_second_deadline"
                elif observed.get("private_bytes", 0) > 512 * 1024**2:
                    reason = "512_MiB_process_private"
                elif total_writes > 1024**3:
                    reason = "1_GiB_cumulative_process_logical_writes"
                elif size["logical_file_bytes"] > 512 * 1024**2:
                    reason = "512_MiB_whole_fixture"
                elif size["files"] + size["directories"] > 20000:
                    reason = "20000_fixture_entries"
                elif logs > 32 * 1024**2:
                    reason = "32_MiB_logs"
                if reason:
                    report.setdefault("budget_stop", dict(phase=current["phase"], reason=reason, sample=observed, footprint=size, log_bytes=logs))
                    if client is not None and client.process.poll() is None:
                        client.process.terminate()
                    return
            except Exception as error:
                report.setdefault("budget_stop", dict(phase=current["phase"], reason="monitor_failure", error=repr(error)))
                client = current["client"]
                if client is not None and client.process.poll() is None:
                    client.process.terminate()
                return

    @contextlib.contextmanager
    def process(name, binary, configuration, temporary_root):
        check()
        require(all(child.poll() is not None for child in children), "Processes must be sequential")
        expected_role = "old" if Path(binary) == binaries["old"] else "new"
        require(digest(Path(binary)) == report["binaries"][expected_role]["sha256"], "Executable changed before launch")
        current["phase"] = name
        with (evidence / f"{name}-stderr.log").open("x", encoding="utf-8") as stderr:
            if Path(binary) == binaries["old"]:
                require(report.get("old_launch_authorized") is True, "Old launch blocked: no successful post-materialization authority preflight")
                require(args.mode != "adopted-authority-refusal", "Refusal acceptance mode never launches the old writer")
            client = ProcedureClient(binary, configuration, stderr, creationflags=FLAGS,
                            temporary_root=temporary_root, owned_processes=children)
            readers.append(client.reader)
            current["client"] = client
            identity = child_identity(client.process)
            identity["phase"] = name
            report["owned_processes"].append(identity)
            try:
                client.rpc("initialize", dict(protocolVersion="2024-11-05", capabilities={}, clientInfo=dict(name="bounded-downgrade-compatibility", version="1")))
                client.rpc("notifications/initialized", {}, True)
                yield client
            finally:
                try:
                    client.close()
                finally:
                    identity.update(child_identity(client.process))
                    identity["logical_write_bytes_including_shutdown"] = logical_writes(client.process)
                    current["client"] = None
                    require(digest(Path(binary)) == report["binaries"][expected_role]["sha256"], "Executable changed during owned phase")

    try:
        require(digest(Path(__file__).with_name("edit_resource_workload.py")) == HELPER_HASH, "Frozen helper changed")
        binaries = dict(new=args.new_binary.resolve(strict=True), old=args.old_binary.resolve(strict=True))
        report["binaries"] = {name: dict(path=str(path), sha256=digest(path)) for name, path in binaries.items()}
        require(re.fullmatch(r"[0-9a-fA-F]{64}", args.expected_new_sha256) is not None,
                "Expected new SHA256 must be64 hexadecimal digits")
        require(report["binaries"]["new"]["sha256"] == args.expected_new_sha256.lower(), "New candidate binary hash differs")
        require(report["binaries"]["old"]["sha256"] == OLD_POLICY_PROFILE, "Old executable differs from the inspected b762 policy profile")
        with binaries["old"].open("rb") as stream, mmap.mmap(stream.fileno(), 0, access=mmap.ACCESS_READ) as image:
            report["old_config_support_preflight"] = {key: image.find(key.encode()) >= 0 for key in ("ENGRAM_CONFIG_PATH", "ENGRAM_MULTI_CLIENT", "no-multi-client")}
        require(all(report["old_config_support_preflight"].values()), "Old executable does not contain required isolated-config/transport option strings")
        with guarded_fixture() as fixture:
            report["fixture"] = str(fixture)
            root, data, temporary_root = (fixture / name for name in ("source", "data", "tmp"))
            root.mkdir()
            temporary_root.mkdir()
            revisions = [0] * 8
            paths = [root / f"Source{number:02}.rs" for number in range(8)]
            for number, path in enumerate(paths):
                path.write_text(source(number, 0)[0], encoding="utf-8")
            for git_args in (["init", "-q"], ["add", "."], ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]):
                subprocess.run(["git", "-C", str(root), *git_args], check=True, capture_output=True,
                               creationflags=FLAGS, timeout=max(0.1, deadline-time.monotonic()))
            configuration = fixture / "configuration.yaml"
            configuration.write_text("allowed_roots: [" + json.dumps(str(root)) + "]\ndata_dir: " + json.dumps(str(data)) +
                                     "\nembedding_backend: local\nllm_backend: none\nmulti_client: false\n", encoding="utf-8")
            report["configuration"] = configuration.read_text(encoding="utf-8")
            report["corpus"] = [dict(path=path.name, bytes=path.stat().st_size, sha256=digest(path)) for path in paths]
            (evidence / "owner.json").write_text(json.dumps(dict(script=str(Path(__file__).resolve()), fixture=str(fixture), source=str(root), data=str(data), temporary_root=str(temporary_root), generated_source_only=True), indent=2) + "\n", encoding="utf-8")
            monitor = threading.Thread(target=observe, name="s18-resource-monitor", daemon=True)
            monitor.start()
            try:
                def verify_ownership(client, pid):
                    require((data / "projects" / pid).is_dir(), "Project does not live under owned data directory")
                    info = call(client, "project_info", project_id=pid)
                    normalized = info.replace("\\\\", "\\").replace("\\", "/").lower()
                    require(str(root).replace("\\", "/").lower() in normalized, "Project references another source root")
                    report.setdefault("ownership_checks", []).append(dict(phase=current["phase"], project_id=pid, source_verified=True, data_verified=True))

                def verify(client, pid, boundary):
                    record = dict(boundary=boundary, phase=current["phase"], files=[])
                    report.setdefault("coverage", []).append(record)
                    for number, path in enumerate(paths):
                        expected_source, declaration = source(number, revisions[number])
                        require(path.read_text(encoding="utf-8") == expected_source, "Owned source unexpectedly changed")
                        query = declaration.split("fn ")[1].split("(")[0]
                        lexical = call(client, "search_memory", project_id=pid, query=query, semantic=False,
                                       include_content=True, max_results=10, include_path_prefixes=[path.name])
                        native = call(client, "vector_search", project_id=pid, query=query, include_content=True,
                                      max_content_chars=4096, include_path_prefixes=[path.name], top_k=10)
                        row = dict(path=path.name, revision=revisions[number], expected_declaration=declaration,
                                   declaration_sha256=hashlib.sha256(declaration.encode()).hexdigest(),
                                   lexical_exact=declaration in lexical, native_exact=declaration in native,
                                   source_sha256=digest(path))
                        record["files"].append(row)
                        if not row["lexical_exact"] or not row["native_exact"]:
                            raise CoverageFailure(f"{boundary}: {path.name} revision {revisions[number]} exact declaration missing/stale; lexical={row['lexical_exact']} native={row['native_exact']}")
                    record["passed"] = True

                def edit_update(client, pid, number):
                    revisions[number] += 1
                    text, declaration = source(number, revisions[number])
                    old_hash = digest(paths[number])
                    paths[number].write_text(text, encoding="utf-8")
                    require(digest(paths[number]) != old_hash, "Edit must change actual source bytes")
                    report.setdefault("edits", []).append(dict(phase=current["phase"], path=paths[number].name,
                        revision=revisions[number], before_sha256=old_hash, after_sha256=digest(paths[number]), declaration=declaration))
                    response = call(client, "update_project", project_id=pid, wait=True, max_commits=1, index_antipatterns=False)
                    if "no_changes_detected" in response:
                        raise CoverageFailure("Actual source edit was silently accepted as no_changes_detected")


                def inventory(client):
                    raw = call(client, "list_repo_rules", project_id=pid, ownership_inventory=True)
                    require(len(raw.encode("utf-8")) <= 4 * 1024**2, "Exact ownership inventory exceeds4MiB")
                    return raw, strict_json(raw)

                def authority_preflight(client, boundary):
                    raw, captured = inventory(client)
                    info = call(client, "project_info", project_id=pid)
                    generation = re.findall(r"(?m)^active_generation: ([0-9]+)$", info)
                    require(len(generation) == 1 and int(generation[0]) > 0, "Exact public active generation unavailable")
                    entry = dict(boundary=boundary, inventory_raw=raw, project_info=info,
                        observed_active_generation=int(generation[0]), old_binary_sha256=report["binaries"]["old"]["sha256"],
                        observation_limit="Public generation plus raw ownership/identity evidence; opaque Registry HEAD revision token is not exposed.")
                    report.setdefault("authority_preflights", []).append(entry)
                    try:
                        inspect_inventory(raw, pid, report["binaries"]["old"]["sha256"])
                        require(Path(captured["project"]["directory"]).resolve(strict=True) == root.resolve(strict=True),
                            "Ownership inventory points outside the exact owned source root")
                    except PreflightRefusal as error:
                        entry.update(disposition="refused", reason=str(error))
                        raise
                    entry["disposition"] = "supported_by_explicit_procedure"
                    return captured

                def policy_payload(captured):
                    # Preserve exact bytes of every rule and receipt, not just decoded policy text.
                    return dict(rows={r["rule_id"]: r["raw"] for r in captured["rows"]},
                        receipts={r["metadata_key"]: r["raw"] for r in captured["receipts"]})

                def seed_policy(client, adopted):
                    policy = '[{"rule":"Never add S18Forbidden to production source. Exception: read-only probes remain allowed.","path_scope":"**/*.rs","severity":"high"}]'
                    (root / "policy.json").write_text(policy, encoding="utf-8")
                    call(client, "add_repo_rule", project_id=pid, rule_id="s18-stable-manual",
                        file_pattern="**/*.rs", rule_text="Always retain S18 manual evidence. Exception: read-only probes remain allowed.",
                        priority=19, introduced_at="2025-09-12", provenance="manual repository rule")
                    if not adopted:
                        call(client, "ingest_quality_gates", project_id=pid, source_path="policy.json", source_type="rules")
                        _, captured = inventory(client)
                        require(len(captured["rows"]) == 2 and len(captured["receipts"]) == 1,
                            "Representable control must contain a real manual row plus an ordinary source-owned rule/receipt")
                        return policy_payload(captured)
                    _, captured = inventory(client)
                    manual = next(r for r in captured["rows"] if r["rule_id"] == "s18-stable-manual")
                    adoption = dict(mode="preview", expected_source_blake3=blake3.blake3(policy.encode()).hexdigest(),
                        bindings=[], decision_ref="owned-S18-fixture-explicit-transfer; not a live human policy decision")
                    base = dict(project_id=pid, source_path="policy.json", source_type="rules")
                    discovered = strict_json(call(client, "ingest_quality_gates", **base, adoption=adoption))
                    require(discovered["status"] == "selection_required"
                        and len(discovered["source_entries"]) == 1
                        and discovered["next_source_entry_offset"] is None, "Complete adoption discovery missing")
                    adoption["bindings"] = [dict(rule_id="s18-stable-manual", expected_raw_blake3=manual["raw"]["blake3"],
                        source_rule_id=discovered["source_entries"][0]["id"], decision="transfer_manual")]
                    preview = strict_json(call(client, "ingest_quality_gates", **base, adoption=adoption))
                    require(preview["status"] == "ready_for_explicit_apply", "Explicit fixture adoption preview unavailable")
                    adoption.update(mode="apply", expected_plan_blake3=preview["plan_blake3"])
                    applied = strict_json(call(client, "ingest_quality_gates", **base, adoption=adoption))
                    require(applied["status"] == "adopted" and applied["adoption_performed"] is True,
                        "Real public adoption did not occur")

                def adoption_refusal(client):
                    before_raw, before = inventory(client)
                    before_info = call(client, "project_info", project_id=pid)
                    try:
                        authority_preflight(client, "before-any-compatible-materialization")
                    except PreflightRefusal as error:
                        require("legacy alias authority" in str(error), "Expected specific unsupported-alias refusal")
                        report["adopted_authority_refused"] = str(error)
                    else:
                        raise CoverageFailure("Adopted aliases were wrongly accepted for an incompatible old writer")
                    after_raw, after = inventory(client)
                    after_info = call(client, "project_info", project_id=pid)
                    require(before_raw == after_raw and before == after and before_info == after_info,
                        "Preflight refusal changed exact rule/receipt/identity evidence or public source generation")
                    require(not any(p["phase"].startswith("old-") for p in report["owned_processes"]),
                        "An old writer was started in refusal mode")
                    require(not report.get("old_launch_authorized", False), "Refusal granted launch authority")
                    report["refusal_preserved_complete_raw_inventory_and_public_generation"] = True
                    held_rows = policy_payload(before)["rows"]
                    call(client, "ingest_quality_gates", project_id=pid, source_path="policy.json", source_type="rules")
                    edit_update(client, pid, 3)
                    verify(client, pid, "new_after_refused_downgrade_and_real_reimport_edit")
                    _, retried = inventory(client)
                    require(policy_payload(retried)["rows"] == held_rows,
                        "Supported new-runtime retry changed the accepted stable row")
                    require(len(retried["receipts"]) == 1, "Reimport lost adoption ownership")
                    receipt = strict_json(retried["receipts"][0]["raw"]["utf8"])
                    require(receipt["pending"] is False and len(receipt["legacy_aliases"]) == 1,
                        "New-runtime reimport did not publish complete alias-bound evidence")
                    report["new_public_reimport_and_update_after_refusal"] = True

                repair = dict(scope="full", wipe_and_reindex=False, max_commits=1, index_antipatterns=False)
                phase = "new-materialize"
                with process(phase, binaries["new"], configuration, temporary_root) as client:
                    empty = call(client, "list_projects")
                    require(empty.strip() == "No projects indexed.", "Primary preflight registry must be empty")
                    report["primary_empty_registry_verified"] = True
                    indexed = call(client, "index_project", directory=str(root), project_name="owned-downgrade-roundtrip", project_type="rust", wait=True)
                    identifier = re.search(r"project_id:\s*([0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})", indexed)
                    require(identifier, "Index response must identify owned project explicitly")
                    pid = identifier.group(1)
                    report["project_id"] = pid
                    verify_ownership(client, pid)
                    call(client, "unwatch_project", project_id=pid)
                    edit_update(client, pid, 0)
                    edit_update(client, pid, 1)
                    policy_control = None
                    if args.mode != "source-roundtrip":
                        policy_control = seed_policy(client, args.mode == "adopted-authority-refusal")
                    if args.mode == "adopted-authority-refusal":
                        adoption_refusal(client)
                    else:
                        authority_preflight(client, "before-compatible-full-materialization")
                        call(client, "repair_project", project_id=pid, **repair)
                        verify(client, pid, "new_after_full_materialization")
                        authority_preflight(client, "after-materialization-before-old-launch")
                        report["old_launch_authorized"] = True
                report["phases"].append(dict(name=phase, passed=True))
                if args.mode != "adopted-authority-refusal":
                    phase = "old-read-edit"
                    with process(phase, binaries["old"], configuration, temporary_root) as client:
                        verify_ownership(client, pid)
                        verify(client, pid, "old_initial_read")
                        if policy_control is not None:
                            visible = call(client, "list_repo_rules", project_id=pid)
                            require("s18-stable-manual" in visible and "Always retain S18 manual evidence" in visible
                                and "Never add S18Forbidden" in visible, "Old writer lost representable policy control")
                        call(client, "unwatch_project", project_id=pid)
                        edit_update(client, pid, 2)
                        verify(client, pid, "old_after_real_edit")
                    report["phases"].append(dict(name=phase, passed=True))
                    phase = "new-return"
                    with process(phase, binaries["new"], configuration, temporary_root) as client:
                        if policy_control is not None:
                            _, returned = inventory(client)
                            require(policy_payload(returned) == policy_control,
                                "Old source read/edit changed exact manual/source rule or receipt bytes")
                            report["representable_policy_raw_bytes_preserved_across_old_writer"] = True
                        try:
                            verify_ownership(client, pid)
                            verify(client, pid, "new_reads_old_written_edit")
                        except RuntimeError as error:
                            # Client.rpc preserves JSON-RPC's structured error as
                            # RuntimeError.args[0]. Recover only the exact explicit
                            # source-layout refusal contracts. The older message
                            # was observed in the original owned roundtrip; the
                            # second is the independently reviewed managed-layout
                            # admission path. Unrelated errors must propagate.
                            payload = error.args[0] if error.args else None
                            expected_refusals = {
                                "source snapshot unavailable: managed source snapshot metadata is missing; full repair required",
                                "source snapshot unavailable: Source snapshot layout is managed or unproven, but required metadata is missing; full repair required",
                            }
                            if (not isinstance(payload, dict) or payload.get("code") != -32603
                                    or payload.get("message") not in expected_refusals):
                                raise
                            report["new_return_explicit_refusal"] = str(error)
                            call(client, "repair_project", project_id=pid, **repair)
                            verify(client, pid, "new_after_supported_full_recovery")
                        call(client, "unwatch_project", project_id=pid)
                        edit_update(client, pid, 3)
                        verify(client, pid, "new_after_next_real_edit")
                        if policy_control is not None:
                            _, final_policy = inventory(client)
                            require(policy_payload(final_policy) == policy_control,
                                "New recovery/next edit changed representable policy raw bytes")
                    report["phases"].append(dict(name=phase, passed=True))
                check()
            finally:
                stop.set()
                if monitor is not None:
                    monitor.join(timeout=5)
                    require(not monitor.is_alive(), "Monitor did not finish; fixture teardown unsafe")
                report["final_fixture_footprint"] = footprint(fixture, 20000)
                report["cumulative_logical_write_bytes_including_shutdown"] = sum(logical_writes(child) for child in children)
                report["all_children_reaped"] = all(child.poll() is not None for child in children)
            # The watchdog can miss a last shutdown burst between samples.
            # Check closed-process counters and the entire final fixture before
            # accepting a run, while the owned directory still exists.
            check()
            require(report["all_children_reaped"], "Final process handles have not all been reaped")
            require(report["cumulative_logical_write_bytes_including_shutdown"] <= 1024**3,
                    "Final cumulative process logical writes exceed 1 GiB")
            size = report["final_fixture_footprint"]
            require(size["logical_file_bytes"] <= 512 * 1024**2,
                    "Final whole-fixture size exceeds 512 MiB")
            require(size["files"] + size["directories"] <= 20000,
                    "Final whole-fixture entry count exceeds 20000")
            require(sum(path.stat().st_size for path in evidence.glob("*-stderr.log")) <= 32 * 1024**2,
                    "Final log size exceeds 32 MiB")
            report["outcome"] = ("unsupported_adopted_authority_refusal_passed"
                if args.mode == "adopted-authority-refusal" else "candidate_procedure_passed")
        report["fixture_removed"] = not fixture.exists()
    except Exception as error:
        report["first_failure"] = dict(phase=phase, type=type(error).__name__, error=str(error))
        report["outcome"] = "failed_candidate_procedure" if isinstance(error, CoverageFailure) else "unresolved"
        report["fixture_removed"] = fixture is not None and not fixture.exists()
        report["all_children_reaped"] = all(child.poll() is not None for child in children)
    finally:
        stop.set()
        report["elapsed_seconds"] = time.monotonic() - started
        report["owned_readers"] = [dict(name=reader.name, joined=not reader.is_alive()) for reader in readers]
        for child in children:
            if child.poll() is not None:
                for stream in (child.stdin, child.stdout):
                    if stream is not None and not stream.closed:
                        stream.close()
                if hasattr(child, "_handle"):
                    child._handle.Close()
        report["old_child_started"] = any(entry["phase"].startswith("old-") for entry in report["owned_processes"])
        report["limitations"] = "Eight synthetic Rust files; explicit operator procedure only, not app-enforced downgrade prevention. Modes distinguish source-only roundtrip, unchanged recognized manual/ordinary-source policy control, and negative adopted-alias refusal with no old launch. Public active_generation plus exact ownership inventory is observed, not an opaque HEAD revision token. No general knowledge/history/alias compatibility, crash guarantee or live deployment claim. Evidence retained until audit review."
        (evidence / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(dict(outcome=report["outcome"], first_failure=report.get("first_failure"),
                          elapsed_seconds=report["elapsed_seconds"], fixture_removed=report.get("fixture_removed"),
                          all_children_reaped=report.get("all_children_reaped"), report=str(evidence / "report.json"))), flush=True)
    return 0 if report["outcome"] in ("candidate_procedure_passed", "unsupported_adopted_authority_refusal_passed") else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("source-roundtrip", "representable-policy-roundtrip", "adopted-authority-refusal"), default="source-roundtrip")
    parser.add_argument("--expected-new-sha256", default=NEW_HASH,
                        help="Exact candidate SHA256; historical530b default retained, never inferred from the executable")
    parser.add_argument("--new-binary", type=Path, required=True)
    parser.add_argument("--old-binary", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    raise SystemExit(run(parser.parse_args()))
