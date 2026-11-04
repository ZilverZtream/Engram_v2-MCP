"""Bounded Windows MCP edit workload; never indexes or edits a live project.

Reports process logical I/O, not physical SSD writes. A synthetic corpus gives a
repeatable baseline; it does not by itself establish large OX-project behavior.
The single fixture lives under the chosen evidence directory and owns teardown.
"""
import argparse
import contextlib
import ctypes
from ctypes import wintypes
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import threading
import time

from instruction_outcomes import Client


def require(condition, message):
    # Ownership and evidence checks must also run under python -O.
    if not condition:
        raise RuntimeError(message)


@contextlib.contextmanager
def owned_fixture(evidence, report, children):
    directory = Path(tempfile.mkdtemp(prefix="edit-workload-", dir=evidence))
    try:
        yield directory
    finally:
        # Normal and exceptional paths both remove the fixture after its exact
        # child handles have been reaped. An OS-stalled child is an interrupted
        # run: preserve and account for it instead of deleting live data.
        for child in children:
            if child.poll() is None:
                try:
                    child.terminate()
                    child.wait(timeout=10)
                except Exception as error:
                    report.setdefault("child_cleanup_errors", []).append(dict(pid=child.pid, error=repr(error)))
        alive = [child_identity(child) for child in children if child.poll() is None]
        if alive:
            report["retained_owned_fixture"] = dict(path=str(directory), children=alive,
                reason="Owned child did not exit after bounded shutdown; live data was not removed",
                cleanup_condition="Verify the recorded PID, creation time and command identity is no longer live before permitted exact-path teardown")
            raise RuntimeError("Owned child remains alive; fixture preserved and run failed")
        remove_owned_fixture(directory, evidence)


def remove_owned_fixture(directory, evidence):
    info = directory.lstat()
    require(not directory.is_symlink() and not (info.st_file_attributes & 0x400),
            "Preserve unexpected reparse point at owned fixture root")
    checked = directory.resolve(strict=True)
    require(checked.parent == evidence.resolve(strict=True) and directory.name.startswith("edit-workload-"),
            "Owned fixture cleanup escaped its recorded evidence directory")
    def remove_readonly(function, path, error):
        # Git deliberately creates read-only object files. Clear that flag
        # only on a regular file proven to belong to this exact new fixture;
        # ACL failures, reparses and other errors remain failures.
        entry = Path(path)
        details = entry.lstat()
        if (not isinstance(error, PermissionError) or not stat.S_ISREG(details.st_mode)
                or details.st_file_attributes & 0x400 or not details.st_file_attributes & 1
                or not entry.resolve(strict=True).is_relative_to(checked)):
            raise error
        entry.chmod(stat.S_IREAD | stat.S_IWRITE)
        function(path)
    shutil.rmtree(directory, onexc=remove_readonly)


def child_identity(process):
    times = [wintypes.FILETIME() for _ in range(4)]
    query = ctypes.windll.kernel32.GetProcessTimes
    query.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    ok = query(wintypes.HANDLE(int(process._handle)), *[ctypes.byref(value) for value in times])
    return dict(pid=process.pid, returncode=process.poll(), command=process.args,
                creation_filetime=(times[0].dwHighDateTime << 32) + times[0].dwLowDateTime if ok else None)


class Memory(ctypes.Structure):
    _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
        (name, ctypes.c_size_t) for name in (
            "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
            "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage", "QuotaNonPagedPoolUsage",
            "PagefileUsage", "PeakPagefileUsage", "PrivateUsage")]


class IO(ctypes.Structure):
    _fields_ = [(name, ctypes.c_ulonglong) for name in (
        "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
        "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]


def logical_writes(process):
    # The owned Popen handle remains valid after wait(), so shutdown writes
    # count too. PID lookup could race reuse and is deliberately avoided.
    counters = IO()
    query = ctypes.windll.kernel32.GetProcessIoCounters
    query.argtypes = [wintypes.HANDLE, ctypes.POINTER(IO)]
    if not query(wintypes.HANDLE(int(process._handle)), ctypes.byref(counters)):
        raise ctypes.WinError()
    return counters.WriteTransferCount


def sample(process):
    handle = wintypes.HANDLE(int(process._handle))
    memory, counters = Memory(), IO()
    memory.cb = ctypes.sizeof(memory)
    memory_fn = ctypes.windll.psapi.GetProcessMemoryInfo
    memory_fn.argtypes = [wintypes.HANDLE, ctypes.POINTER(Memory), wintypes.DWORD]
    io_fn = ctypes.windll.kernel32.GetProcessIoCounters
    io_fn.argtypes = [wintypes.HANDLE, ctypes.POINTER(IO)]
    times_fn = ctypes.windll.kernel32.GetProcessTimes
    times_fn.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    times = [wintypes.FILETIME() for _ in range(4)]
    if not memory_fn(handle, ctypes.byref(memory), memory.cb):
        raise ctypes.WinError()
    if not io_fn(handle, ctypes.byref(counters)) or not times_fn(handle, *[ctypes.byref(value) for value in times]):
        raise ctypes.WinError()
    return {"at": time.monotonic(), "cpu_seconds": sum((v.dwHighDateTime << 32) + v.dwLowDateTime for v in times[2:]) / 1e7,
            "read_bytes": counters.ReadTransferCount, "write_bytes": counters.WriteTransferCount,
            "private_bytes": memory.PrivateUsage, "resident_bytes": memory.WorkingSetSize,
            "peak_resident_bytes": memory.PeakWorkingSetSize, "peak_commit_bytes": memory.PeakPagefileUsage}


def difference(before, after):
    elapsed = after["at"] - before["at"]
    cpu = after["cpu_seconds"] - before["cpu_seconds"]
    return {"seconds": elapsed, "logical_read_bytes": after["read_bytes"] - before["read_bytes"],
            "logical_write_bytes": after["write_bytes"] - before["write_bytes"],
            "cpu_seconds": cpu,
            "average_cpu_percent": cpu / elapsed / (os.cpu_count() or 1) * 100 if elapsed > 0 else None,
            "private_bytes_after": after["private_bytes"], "resident_bytes_after": after["resident_bytes"]}


class FootprintLimit(RuntimeError):
    pass


def footprint(root, file_limit=200_000):
    """Inspect only this run's owned store; never enumerate the host volume."""
    files, directories, total = 0, 0, 0
    for directory, folders, names in os.walk(root, followlinks=False):
        directories += len(folders)
        if directories > file_limit:
            raise FootprintLimit("Owned workload exceeded its directory-count inspection bound")
        for name in [*folders, *names]:
            path = Path(directory) / name
            try:
                info = path.lstat()
            except FileNotFoundError:
                continue  # A concurrent owned-store GC may retire an entry.
            if path.is_symlink() or info.st_file_attributes & 0x400:
                raise RuntimeError("Preserve unexpected reparse point in owned workload")
        for name in names:
            try:
                size = (Path(directory) / name).stat().st_size
            except FileNotFoundError:
                continue
            files += 1
            if files > file_limit:
                raise FootprintLimit("Owned workload exceeded its file-count inspection bound")
            total += size
    return {"files": files, "directories": directories, "logical_file_bytes": total}


@contextlib.contextmanager
def deny_content_reads(path):
    """Lock only the selected owned fixture file; metadata stays readable."""
    create = ctypes.windll.kernel32.CreateFileW
    create.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                       ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    create.restype = wintypes.HANDLE
    close = ctypes.windll.kernel32.CloseHandle
    close.argtypes = [wintypes.HANDLE]
    close.restype = wintypes.BOOL
    handle = create(str(path), 0x40000000, 2 | 4, None, 3, 0x80, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError()
    try:
        yield
    finally:
        if not close(handle):
            raise ctypes.WinError()


def run(args):
    if os.name != "nt":
        raise SystemExit("This workload currently samples Windows process counters only")
    if not 2 <= args.files <= 2000 or not 1 <= args.edits <= 2000 or not 256 <= args.bytes_per_file <= 16384:
        raise SystemExit("Use 2..2000 files, 1..2000 edits and 256..16384 bytes per file")
    if not 0 <= args.settle_seconds <= 30 or not 0 <= args.edit_interval <= 5:
        raise SystemExit("Use 0..30 settling seconds and 0..5 seconds between edits")
    if not 0 <= args.query_only_count <= 80:
        raise SystemExit("Use 0..80 query-only searches")
    if not 30 <= args.max_seconds <= 7200 or not 1000 <= args.max_store_files <= 50_000:
        raise SystemExit("Use 30..7200 seconds and1000..50000 store files for workload stop limits")
    if not 32*1024*1024 <= args.max_store_bytes <= 4*1024**3 or not 1 <= args.max_private_bytes <= 4*1024**3:
        raise SystemExit("Use32MiB..4GiB store and1byte..4GiB private-memory stop limits")
    if not 1 <= args.max_logical_write_bytes <= 8*1024**3:
        raise SystemExit("Use 1 byte..8 GiB for the cumulative process logical-write limit")
    if args.same_second_edits and (args.files != 2 or args.edits != 1):
        raise SystemExit("The focused same-size/same-second probe requires --files 2 --edits 1")
    started = time.monotonic()
    deadline = started + args.max_seconds
    binary, evidence = args.binary.resolve(), args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=False)
    with binary.open("rb") as source:
        binary_hash = hashlib.file_digest(source, "sha256").hexdigest()
    report = {"binary": str(binary), "binary_sha256": binary_hash, "files": args.files, "bytes_per_file_target": args.bytes_per_file,
              "driver_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "backend": args.backend, "limitations": "Synthetic repeatable workload. Counters are process logical I/O, not physical disk wear. No live project or production reindex.",
              "same_second_edits": args.same_second_edits,
              "operations": [], "samples": [], "restart_samples": [], "footprint_samples": [], "distinct_edited_paths": [],
              "stop_limits": dict(seconds=args.max_seconds, owned_fixture_bytes=args.max_store_bytes,
                  owned_fixture_files_and_directories_each=args.max_store_files,
                  private_bytes=args.max_private_bytes, log_bytes=32*1024*1024,
                  cumulative_process_logical_write_bytes=args.max_logical_write_bytes,
                  limits="Private memory, cumulative process writes and one absolute deadline are sampled during RPCs. Whole-fixture size/entry and log limits are checked synchronously between operations and may overshoot until the next checkpoint. These are protective stops, not hard OS quotas or formal Engram memory goals. Application counters exclude owned-directory inspection overhead."),
              "verify_stat_cache": args.verify_stat_cache,
              "gc_limit": "No forced GC or held-reader operation in this subprocess workload; those require their own gates."}
    fixture = None
    owned_children = []
    completed_phase_writes = {}
    report["phase_logical_write_bytes"] = completed_phase_writes
    try:
        with owned_fixture(evidence, report, owned_children) as directory:
            fixture = Path(directory)
            temporary_root = fixture / "tmp"
            temporary_root.mkdir()
            with (evidence / "owner.json").open("x", encoding="utf-8") as owner:
                json.dump({"script": str(Path(__file__).resolve()), "fixture": str(fixture),
                           "binary_sha256": binary_hash, "generated_source_only": True,
                           "driver_sha256": report["driver_sha256"],
                           "files": args.files, "edits": args.edits,
                           "temporary_root": str(temporary_root)}, owner, indent=2)
                owner.write("\n")
            root = fixture / "source"
            root.mkdir()
            def body(number, revision):
                function = f"pub fn resource_probe_{number:04}_revision_{revision:04}() -> usize {{ {number + revision} }}\n"
                # Exercise file-metadata scans against a real symbol/call graph,
                # rather than a corpus dominated by padding comments.
                ordinal = 0
                while True:
                    helper = f"pub fn context_{number:04}_{ordinal:03}() -> usize {{ resource_probe_{number:04}_revision_{revision:04}() + {ordinal} }}\n"
                    if len(function) + len(helper) > args.bytes_per_file:
                        break
                    function += helper
                    ordinal += 1
                padding = "// Representative unchanged source context.\n"
                return function + padding * max(0, (args.bytes_per_file - len(function)) // len(padding))
            report["corpus_kind"] = "methods_and_call_edges"
            report["methods_per_file_approx"] = body(0, 0).count("pub fn ")
            for number in range(args.files):
                (root / f"Source{number:04}.rs").write_text(body(number, 0), encoding="utf-8")
            for git_args in (["init", "-q"], ["add", "."], ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]):
                subprocess.run(["git", "-C", str(root), *git_args], check=True, capture_output=True)
            configuration = fixture / "configuration.yaml"
            configuration.write_text("allowed_roots: [" + json.dumps(str(root)) + "]\ndata_dir: " + json.dumps(str(fixture / "data")) +
                                     f"\nembedding_backend: {args.backend}\nllm_backend: none\nmulti_client: false\nmax_project_files: 2100\nmax_project_bytes: 40000000\n", encoding="utf-8")
            report["configuration"] = configuration.read_text(encoding="utf-8")
            report["fixture"] = str(fixture)
            def check_fixture(phase):
                # Filesystem work stays on the main thread. The watchdog must
                # never retain a fixture path or race its teardown if an OS
                # process-counter query unexpectedly fails to return.
                reason = None
                try:
                    size = footprint(fixture, args.max_store_files)
                    report["footprint_samples"].append(dict(at=time.monotonic(), phase=phase, scope="whole_owned_fixture", **size))
                    if size["logical_file_bytes"] > args.max_store_bytes:
                        reason = dict(kind="owned_fixture_byte_limit", observed=size["logical_file_bytes"])
                except FootprintLimit as error:
                    reason = dict(kind="owned_fixture_entry_limit", error=str(error))
                log_bytes = sum(p.stat().st_size for p in [evidence / "stderr.log", evidence / "restart-stderr.log"] if p.exists())
                if log_bytes > 32*1024*1024:
                    reason = reason or dict(kind="log_byte_limit", observed=log_bytes)
                if time.monotonic() >= deadline:
                    reason = reason or dict(kind="wall_time_limit", observed=time.monotonic()-started)
                if reason is not None:
                    report.setdefault("budget_stop", dict(phase=phase, **reason))
                if "budget_stop" in report:
                    raise RuntimeError(f"Workload stopped at a protective limit: {report['budget_stop']}")
            check_fixture("before_startup")
            with (evidence / "stderr.log").open("w", encoding="utf-8") as stderr:
                client = Client(binary, configuration, stderr, creationflags=0x4000 | 0x08000000, temporary_root=temporary_root, owned_processes=owned_children)
                report["process_id"] = client.process.pid
                stop = threading.Event()
                monitor = None
                try:
                    def observe(phase, watched, signal):
                        samples = report["samples" if phase == "primary" else "restart_samples"]
                        while not signal.wait(0.25):
                            try:
                                observation = sample(watched.process)
                                samples.append(observation)
                                reason = None
                                total_writes = sum(completed_phase_writes.values()) + observation["write_bytes"]
                                observation["cumulative_workload_logical_write_bytes"] = total_writes
                                if observation["private_bytes"] > args.max_private_bytes:
                                    reason = dict(kind="private_memory_limit", observed=observation["private_bytes"])
                                elif total_writes > args.max_logical_write_bytes:
                                    reason = dict(kind="cumulative_logical_write_limit", observed=total_writes)
                                elif time.monotonic() >= deadline:
                                    reason = dict(kind="wall_time_limit", observed=time.monotonic()-started)
                                if reason is not None:
                                    report.setdefault("budget_stop", dict(phase=phase, **reason))
                                    watched.process.terminate()  # This exact owned Popen handle only.
                                    break
                            except Exception as error:
                                if watched.process.poll() is None:
                                    report.setdefault("budget_stop", dict(kind="monitor_failure", phase=phase, error=repr(error)))
                                    watched.process.terminate()
                                break
                    monitor = threading.Thread(target=observe, args=("primary", client, stop), daemon=True)
                    report["samples"].append(sample(client.process))
                    monitor.start()
                    client.rpc("initialize", dict(protocolVersion="2024-11-05", capabilities={}, clientInfo=dict(name="bounded-edit-workload", version="1")))
                    client.rpc("notifications/initialized", {}, True)
                    before = sample(client.process)
                    indexed = client.tool("index_project", directory=str(root), project_name="bounded-edit-workload", project_type="rust", wait=True)
                    report["initial_index"] = difference(before, sample(client.process))
                    projects = client.tool("list_projects")
                    identifier = re.search(r"project_id:\s*([0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})", indexed)
                    require(identifier, "Index response must identify this fixture's project explicitly")
                    pid = identifier.group(1)
                    require(pid in projects, "Owned project is missing from list_projects")
                    require((fixture / "data" / "projects" / pid).is_dir(), "Refuse a project outside this owned data directory")
                    info = client.tool("project_info", project_id=pid)
                    normalized = info.replace("\\\\", "\\").replace("\\", "/").lower()
                    require(str(root).replace("\\", "/").lower() in normalized, "Refuse a project with another source root")
                    report["owned_project_id"] = pid
                    report["owned_source_verified"] = True
                    check_fixture("after_initial_index")
                    if args.same_second_edits:
                        # Only this newly registered owned project is touched.
                        # No watcher may consume the change ahead of the RPC.
                        report["probe_unwatch_response"] = client.tool("unwatch_project", project_id=pid)
                    def update(kind, revision=None):
                        before = sample(client.process)
                        response = client.tool("update_project", project_id=pid, wait=True, max_commits=1, index_antipatterns=False)
                        result = dict(difference(before, sample(client.process)), kind=kind, revision=revision, response=response)
                        report["operations"].append(result)
                        print(json.dumps({key:value for key,value in result.items() if key != "response"}), flush=True)
                        check_fixture(kind)
                        return response
                    # Warm history's cursor before measuring genuine no-op refreshes.
                    update("warm_history")
                    idle = sample(client.process)
                    time.sleep(args.settle_seconds)
                    report["idle_before"] = difference(idle, sample(client.process))
                    for _ in range(3):
                        response = update("unchanged")
                        require("no_changes_detected" in response, response)
                    if args.query_only_count:
                        # A separate unchanged-source phase includes background
                        # learning after the search responses, rather than
                        # attributing overlapping edit work to search writes.
                        time.sleep(args.settle_seconds)
                        query_start = sample(client.process)
                        for ordinal in range(args.query_only_count):
                            result = client.tool("search_memory", project_id=pid,
                                query="resource_probe", max_results=10)
                            require("resource_probe" in result and "Source" in result, result)
                        responses_done = sample(client.process)
                        # A bounded quiet tail is evidence of settling, not an
                        # acknowledgement of event delivery or physical IO.
                        last_write = responses_done["write_bytes"]
                        quiet_since = time.monotonic()
                        quiet_deadline = quiet_since + 20
                        while True:
                            time.sleep(0.5)
                            current = sample(client.process)
                            if current["write_bytes"] != last_write:
                                last_write = current["write_bytes"]
                                quiet_since = time.monotonic()
                            if time.monotonic() - quiet_since >= 3:
                                quiet = True
                                break
                            if time.monotonic() >= quiet_deadline:
                                quiet = False
                                break
                        report["query_only"] = dict(difference(query_start, current),
                            searches=args.query_only_count, max_results=10,
                            response_window=difference(query_start, responses_done),
                            quiet_tail_reached=quiet, source_unchanged=True,
                            tail_limit_seconds=20,
                            limits="Process logical IO including IPC; quiet counters do not acknowledge all queued events or physical SSD writes")
                        print(json.dumps({"query_only": report["query_only"]}), flush=True)
                        check_fixture("after_query_only")
                    for revision in range(1, args.edits + 1):
                        # Distinct paths exercise snapshot ancestry/checkpoints.
                        # Keep the last file untouched throughout the whole run.
                        number = (revision - 1) % (args.files - 1)
                        path = f"Source{number:04}.rs"
                        edited_file = root / path
                        before_stat = edited_file.stat() if args.same_second_edits else None
                        edited_file.write_text(body(number, revision), encoding="utf-8")
                        if before_stat is not None:
                            # Distinct, Windows-representable subsecond time,
                            # deliberately retaining the previous whole second.
                            stamp = before_stat.st_mtime_ns
                            changed_stamp = stamp // 1_000_000_000 * 1_000_000_000 + (stamp % 1_000_000_000 + 1_000_000) % 1_000_000_000
                            os.utime(edited_file, ns=(before_stat.st_atime_ns, changed_stamp))
                            after_stat = edited_file.stat()
                            require(after_stat.st_mtime_ns != stamp and after_stat.st_mtime_ns // 1_000_000_000 == stamp // 1_000_000_000,
                                    "Filesystem did not retain the controlled distinct subsecond timestamp")
                            require(after_stat.st_size == before_stat.st_size, "Focused probe must change content without changing size")
                            report["same_second_edit_observation"] = dict(path=path,
                                before_mtime_ns=stamp, after_mtime_ns=after_stat.st_mtime_ns,
                                before_bytes=before_stat.st_size, after_bytes=after_stat.st_size)
                        if path not in report["distinct_edited_paths"]:
                            report["distinct_edited_paths"].append(path)
                        response = update("real_edit", revision)
                        require(re.findall(r"(?m)^files=(\d+)(?=\s|$)", response) == ["1"], response)
                        query_before = sample(client.process)
                        changed = client.tool("search_memory", project_id=pid, query=f"resource_probe_{number:04}_revision_{revision:04}", max_results=3)
                        sibling_name = f"Source{args.files - 1:04}.rs"
                        sibling_query = f"resource_probe_{args.files - 1:04}_revision_0000"
                        sibling = client.tool("search_memory", project_id=pid, query=sibling_query, max_results=3)
                        require(path in changed and f"pub fn resource_probe_{number:04}_revision_{revision:04}()" in changed, "Edited declaration missing from search results")
                        require(sibling_name in sibling and f"pub fn resource_probe_{args.files - 1:04}_revision_0000()" in sibling, "Unchanged sibling declaration missing from search results")
                        if args.backend != "fts_only":
                            require(sibling_name in client.tool("vector_search", project_id=pid, query=sibling_query, top_k=10), "Unchanged sibling missing from vector search")
                        report["operations"].append(dict(difference(query_before, sample(client.process)),
                            kind="queries_after_edit", revision=revision, edited_path=path))
                        check_fixture("after_edit_queries")
                        time.sleep(args.edit_interval)
                    settle = sample(client.process)
                    time.sleep(args.settle_seconds)
                    report["idle_after"] = difference(settle, sample(client.process))
                    response = update("final_unchanged")
                    require("no_changes_detected" in response, response)
                    report["peak_private_bytes"] = max(s["private_bytes"] for s in report["samples"])
                    report["peak_resident_bytes"] = max(s["resident_bytes"] for s in report["samples"])
                    report["process_peak_working_set_bytes"] = max(s["peak_resident_bytes"] for s in report["samples"])
                    report["process_peak_commit_bytes"] = max(s["peak_commit_bytes"] for s in report["samples"])
                    report["source_queries_preserved"] = True
                    report["exact_new_source_declarations_verified"] = True
                    report["store_before_restart"] = footprint(fixture / "data")
                    if args.verify_stat_cache:
                        # Separate feature probe after the common resource
                        # workload: warm a timestamp-only observation, then
                        # deny content reads in the next actual MCP process.
                        probe = root / sibling_name
                        stat = probe.stat()
                        os.utime(probe, ns=(stat.st_atime_ns, stat.st_mtime_ns + 2_000_000_000))
                        response = update("timestamp_only_warm")
                        require("no_changes_detected" in response, response)
                finally:
                    try:
                        # Keep the counter-only watchdog active while shutdown
                        # flushes run; account for the final counters afterwards.
                        client.close()
                    finally:
                        if client.process.poll() is not None:
                            stop.set()
                        if monitor is not None: monitor.join(timeout=10)
                    report["primary_returncode"] = client.process.returncode
                    completed_phase_writes["primary"] = logical_writes(client.process)
                    report["cumulative_process_logical_write_bytes"] = sum(completed_phase_writes.values())
                    if monitor is not None and monitor.is_alive():
                        report["cleanup_issue"] = "Owned process-counter watchdog did not join; it holds no fixture paths. Child reaped; no restart permitted."
                        raise RuntimeError("Owned workload monitor did not finish; no restart permitted")
                    if report["cumulative_process_logical_write_bytes"] > args.max_logical_write_bytes:
                        report.setdefault("budget_stop", dict(phase="primary_shutdown", kind="cumulative_logical_write_limit", observed=report["cumulative_process_logical_write_bytes"]))
                    if "budget_stop" in report:
                        raise RuntimeError(f"Workload stopped at a protective limit: {report['budget_stop']}")
                    if client.process.returncode != 0:
                        raise RuntimeError(f"Owned workload process exited with {client.process.returncode}")
            # Restart sequentially against this exact owned store and configuration.
            # No second live process may compete with the original workload.
            check_fixture("before_restart")
            with (evidence / "restart-stderr.log").open("x", encoding="utf-8") as stderr:
                client = Client(binary, configuration, stderr, creationflags=0x4000 | 0x08000000, temporary_root=temporary_root, owned_processes=owned_children)
                report["restart_process_id"] = client.process.pid
                stop = threading.Event()
                monitor = threading.Thread(target=observe, args=("restart", client, stop), daemon=True)
                report["restart_samples"].append(sample(client.process))
                monitor.start()
                try:
                    client.rpc("initialize", dict(protocolVersion="2024-11-05", capabilities={}, clientInfo=dict(name="bounded-edit-workload-restart", version="1")))
                    client.rpc("notifications/initialized", {}, True)
                    before = sample(client.process)
                    guard = deny_content_reads(root / sibling_name) if args.verify_stat_cache else contextlib.nullcontext()
                    with guard:
                        response = client.tool("update_project", project_id=pid, wait=True, max_commits=1, index_antipatterns=False)
                    require("no_changes_detected" in response, response)
                    report["restart_update_without_content_read"] = args.verify_stat_cache
                    changed = client.tool("search_memory", project_id=pid, query=f"resource_probe_{number:04}_revision_{revision:04}", max_results=3)
                    sibling = client.tool("search_memory", project_id=pid, query=sibling_query, max_results=3)
                    require(path in changed and f"pub fn resource_probe_{number:04}_revision_{revision:04}()" in changed, "Edited declaration missing after restart")
                    require(sibling_name in sibling and f"pub fn resource_probe_{args.files - 1:04}_revision_0000()" in sibling, "Unchanged sibling declaration missing after restart")
                    report["restart"] = difference(before, sample(client.process))
                    report["restart_queries_preserved"] = True
                    report["store_after_restart"] = footprint(fixture / "data")
                    check_fixture("after_restart")
                finally:
                    try:
                        client.close()
                    finally:
                        if client.process.poll() is not None:
                            stop.set()
                        monitor.join(timeout=10)
                    report["restart_returncode"] = client.process.returncode
                    completed_phase_writes["restart"] = logical_writes(client.process)
                    report["cumulative_process_logical_write_bytes"] = sum(completed_phase_writes.values())
                    if monitor.is_alive():
                        report["cleanup_issue"] = "Owned process-counter restart watchdog did not join; it holds no fixture paths. Child reaped."
                        raise RuntimeError("Owned restart monitor did not finish")
                    if report["cumulative_process_logical_write_bytes"] > args.max_logical_write_bytes:
                        report.setdefault("budget_stop", dict(phase="restart_shutdown", kind="cumulative_logical_write_limit", observed=report["cumulative_process_logical_write_bytes"]))
                    if "budget_stop" in report:
                        raise RuntimeError(f"Workload stopped at a protective limit: {report['budget_stop']}")
                    if client.process.returncode != 0:
                        raise RuntimeError(f"Owned workload process exited with {client.process.returncode}")
        report["outcome"] = "completed"
    except BaseException as error:
        report["outcome"] = "failed"
        report["error"] = repr(error)
        raise
    finally:
        report["owned_children"] = [child_identity(child) for child in owned_children]
        observations = report["samples"] + report["restart_samples"]
        if observations:
            report["peak_private_bytes"] = max(s["private_bytes"] for s in observations)
            report["peak_resident_bytes"] = max(s["resident_bytes"] for s in observations)
            report["process_peak_working_set_bytes"] = max(s["peak_resident_bytes"] for s in observations)
            report["process_peak_commit_bytes"] = max(s["peak_commit_bytes"] for s in observations)
        report["owned_fixture_removed"] = fixture is None or not fixture.exists()
        with (evidence / "report.json").open("x", encoding="utf-8") as output:
            json.dump(report, output, indent=2)
            output.write("\n")
    print(json.dumps({key:value for key,value in report.items() if key not in {"samples", "restart_samples", "operations", "footprint_samples"}}), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("evidence", type=Path)
    parser.add_argument("--files", type=int, default=500)
    parser.add_argument("--bytes-per-file", type=int, default=2048)
    parser.add_argument("--edits", type=int, default=6)
    parser.add_argument("--backend", choices=("local", "fts_only"), default="local")
    parser.add_argument("--settle-seconds", type=float, default=5)
    parser.add_argument("--edit-interval", type=float, default=0.5)
    parser.add_argument("--query-only-count", type=int, default=0,
                        help="Measure unchanged-source searches plus a bounded quiet tail before edits")
    parser.add_argument("--verify-stat-cache", action="store_true",
                        help="After resource phases, deny one source read across a real process restart")
    parser.add_argument("--same-second-edits", action="store_true",
                        help="Focused 2-file/1-edit behavior probe: same size and whole-second mtime, distinct actual subsecond time; disabled for ordinary performance measurements")
    parser.add_argument("--max-seconds", type=float, default=180,
                        help="Sampled wall-time stop condition; only this run's owned child is terminated")
    parser.add_argument("--max-fixture-bytes", "--max-store-bytes", dest="max_store_bytes", type=int, default=512*1024*1024,
                        help="Sampled size stop condition covers source, Git, data and temp under this one owned fixture")
    parser.add_argument("--max-fixture-entries", "--max-store-files", dest="max_store_files", type=int, default=20_000,
                        help="Maximum observed file count or directory count in the entire owned fixture")
    parser.add_argument("--max-private-bytes", type=int, default=2*1024**3,
                        help="Protective sampled stop condition, not an Engram RSS requirement")
    parser.add_argument("--max-logical-write-bytes", type=int, default=1024**3,
                        help="Cumulative owned-child process writes across startup, edits, shutdown and restart; includes IPC, not physical SSD writes")
    run(parser.parse_args())
