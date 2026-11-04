"""Bounded repeated-edit/explicit-collection extension of the frozen edit driver.

Imports the original workload unchanged. Extra work is timed separately; its
time is also included in the original driver's query windows and must not be
mistaken for an edit-only regression. All paths and children retain that
driver's ownership, stop limits and teardown. No live project is used.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re

import edit_resource_workload as driver


DRIVER_SHA256 = '4db263193a6f8b86f97736fa0aa99a484059dc3bf321f18dc3809f3fcee98fa0'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('evidence', type=Path)
    parser.add_argument('--edits', type=int, default=200,
                        help='50..500 edits in complete cycles of 50 paths (default: 200)')
    parser.add_argument('--expected-binary-sha256',
                        help='Require this frozen executable before/after every launch and at completion')
    options = parser.parse_args()
    if not 50 <= options.edits <= 500 or options.edits % 50:
        parser.error('--edits must be a multiple of 50 in 50..500')
    driver.require(hashlib.sha256(Path(driver.__file__).read_bytes()).hexdigest() == DRIVER_SHA256,
                   'Frozen edit driver changed; review protocol before running')
    expected_binary = options.expected_binary_sha256
    if expected_binary is None:
        with options.binary.open('rb') as binary:
            expected_binary = hashlib.file_digest(binary, 'sha256').hexdigest()
    details = dict(protocol=f'51 files, {options.edits} edits, first 50 paths cycled {options.edits // 50} times; explicit collection every 50 edits',
                   wrapper_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                   driver_sha256=DRIVER_SHA256, maintenance=[], verification=[], rpc_windows=[], retention=[],
                   binary_checks=[], expected_binary_sha256=expected_binary,
                   base_report_note='report.json is the unchanged base-driver report: its gc_limit text does not describe the injected collections/observations recorded here.',
                   observation_limits=dict(max_retention_response_bytes=128*1024,
                       max_retention_observations=2 + 3*(options.edits // 50),
                       watchdog_sample_seconds=0.25,
                       fixture_scan_cadence='Synchronous main-thread checks after initial index, each update, each edit query group and restart; watchdog never scans files',
                       fixture_files_and_directories_each=20000),
                   limits='Current native debug executable plus extra diagnostic RPCs. Synthetic local-backend process logical I/O only; no optimized-control comparison, causal performance comparison, physical SSD, kernel or leak attribution. Physical store counts are not selected-source counts. Original driver query windows include injected maintenance, verification and observations; rpc_windows isolate individual original RPCs. Tracked budgets/caches are bounded non-atomic observations, not a heap census.')
    original = driver.Client
    original_thread_start = driver.threading.Thread.start
    clients = []
    owned_threads = []

    def verify_binary(phase, ordinal=None):
        with options.binary.open('rb') as binary:
            actual = hashlib.file_digest(binary, 'sha256').hexdigest()
        observation = dict(phase=phase, process=ordinal, sha256=actual,
                           bytes=options.binary.stat().st_size, matches_expected=actual == expected_binary)
        details['binary_checks'].append(observation)
        driver.require(observation['matches_expected'], f'Frozen executable changed at {phase}')

    def start_owned_thread(thread):
        # The imported client does not expose its stdout-reader Thread. Record
        # only threads whose exact closure/arguments contain one of our clients;
        # this neither enumerates nor joins unrelated application threads.
        target = getattr(thread, '_target', None)
        owner = None
        purpose = None
        if getattr(target, '__name__', None) == 'read':
            for cell in getattr(target, '__closure__', ()) or ():
                if any(cell.cell_contents is client for client in clients):
                    owner, purpose = cell.cell_contents, 'mcp_stdout_reader'
                    break
        elif getattr(target, '__name__', None) == 'observe':
            arguments = getattr(thread, '_args', ())
            if len(arguments) >= 2 and any(arguments[1] is client for client in clients):
                owner, purpose = arguments[1], f'{arguments[0]}_counter_watchdog'
        if owner is not None:
            owned_threads.append((thread, owner.ordinal, purpose))
        return original_thread_start(thread)

    class ObservedClient(original):
        def __init__(self, *args, **kwargs):
            self.ordinal = len(clients)
            self.edits = 0
            self.verified_restart = False
            self.collected = set()
            clients.append(self)
            verify_binary('before_launch', self.ordinal)
            super().__init__(*args, **kwargs)
            verify_binary('after_launch', self.ordinal)

        def observe_retention(self, pid, phase):
            driver.require(len(details['retention']) < details['observation_limits']['max_retention_observations'],
                           'Retention observation count exceeded its explicit bound')
            before = driver.sample(self.process)
            response = super().tool('get_memory_budget', project_id=pid, output_json=True)
            driver.require(len(response.encode('utf-8')) <= details['observation_limits']['max_retention_response_bytes'],
                           'Retention response exceeded its bounded evidence allowance')
            payload = json.loads(response)
            driver.require(isinstance(payload, dict), 'Retention observation must be a JSON object')
            details['retention'].append(dict(phase=phase, process=self.ordinal,
                edit=options.edits if self.ordinal else self.edits,
                **driver.difference(before, driver.sample(self.process)), observation=payload))

        def verify_all(self, pid, phase):
            before = driver.sample(self.process)
            checked = []
            latest = options.edits if self.ordinal else self.edits
            for number in range(51):
                revision = (latest - 1 - number) // 50 * 50 + number + 1 if number < 50 and latest > number else 0
                query = f'resource_probe_{number:04}_revision_{revision:04}'
                path = f'Source{number:04}.rs'
                lexical = super().tool('search_memory', project_id=pid, query=query, max_results=3)
                driver.require(path in lexical and f'pub fn {query}()' in lexical,
                               f'{phase}: latest declaration missing for {path}')
                if revision:
                    previous = max(0, revision - 50)
                    old_query = f'resource_probe_{number:04}_revision_{previous:04}'
                    old = super().tool('search_memory', project_id=pid, query=old_query, max_results=3)
                    driver.require(f'pub fn {old_query}()' not in old,
                                   f'{phase}: obsolete declaration returned for {path}')
                vector = super().tool('vector_search', project_id=pid, query=query, top_k=10,
                    include_path_prefixes=[path], include_content=True, max_content_chars=4096)
                driver.require(path in vector and f'pub fn {query}()' in vector,
                               f'{phase}: current native declaration missing for {path}')
                if revision:
                    driver.require(f'pub fn {old_query}()' not in vector,
                                   f'{phase}: obsolete native declaration returned for {path}')
                checked.append(dict(path=path, revision=revision))
            details['verification'].append(dict(phase=phase, process=self.ordinal,
                **driver.difference(before, driver.sample(self.process)), checked=checked,
                native_limit='Path-filtered pure-vector search verifies current source declarations, not semantic ranking or vector-bit identity.'))

        def tool(self, name, **arguments):
            before = driver.sample(self.process)
            response = super().tool(name, **arguments)
            after = driver.sample(self.process)
            details['rpc_windows'].append(dict(name=name, process=self.ordinal, edit=self.edits,
                **driver.difference(before, after)))
            if name == 'update_project' and not self.ordinal:
                if re.findall(r'(?m)^files=(\d+)(?=\s|$)', response) == ['1']:
                    self.edits += 1
            pid = arguments.get('project_id')
            if not self.ordinal and name == 'project_info':
                self.observe_retention(pid, 'initial_index')
            sibling = arguments.get('query') == 'resource_probe_0050_revision_0000'
            # The primary driver's last query per edit is the sibling native
            # query. Its watchdog stays active throughout all injected work.
            if (not self.ordinal and name == 'vector_search' and sibling and self.edits
                    and self.edits % 50 == 0 and self.edits not in self.collected):
                self.collected.add(self.edits)
                self.observe_retention(pid, f'before_collection_{self.edits}')
                before_gc = driver.sample(self.process)
                collected = super().tool('incremental_indexing_gc', project_id=pid,
                                         target_generation=0, compact_vectors=True)
                after_gc = driver.sample(self.process)
                driver.require('target_gen=0' in collected and 'GC completed' in collected,
                               'Explicit collection did not complete at the requested cutoff')
                physical_store_counts = {}
                for label in ('graph_nodes', 'graph_edges', 'tantivy_docs', 'lancedb_vectors'):
                    match = re.search(rf'{label}: (\d+) -> (\d+)', collected)
                    driver.require(match is not None and match[1] == match[2],
                                   f'Zero-cutoff collection changed {label}')
                    physical_store_counts[label] = int(match[2])
                driver.require(physical_store_counts['tantivy_docs'] > 0 and physical_store_counts['lancedb_vectors'] > 0,
                               'Native fixture must have nonempty physical documents and vectors')
                counts = re.search(r'Vector compaction completed: (\d+) fragments replaced by (\d+)', collected)
                details['maintenance'].append(dict(edit=self.edits, response=collected,
                    physical_store_counts=physical_store_counts,
                    fragments_removed=int(counts[1]) if counts else None,
                    fragments_added=int(counts[2]) if counts else None,
                    **driver.difference(before_gc, after_gc)))
                self.observe_retention(pid, f'after_collection_{self.edits}')
                self.verify_all(pid, f'after_collection_{self.edits}')
                self.observe_retention(pid, f'after_verification_{self.edits}')
                print(json.dumps(dict(collection_edit=self.edits,
                    gc=details['maintenance'][-1], all_51_paths_verified=True)), flush=True)
            if self.ordinal and name == 'search_memory' and sibling and not self.verified_restart:
                self.verified_restart = True
                self.verify_all(pid, 'restart')
                self.observe_retention(pid, 'restart_after_verification')
            return response

    driver.Client = ObservedClient
    driver.threading.Thread.start = start_owned_thread
    args = argparse.Namespace(binary=options.binary, evidence=options.evidence,
        files=51, edits=options.edits, bytes_per_file=2048, backend='local', settle_seconds=5,
        edit_interval=0.1, query_only_count=0, verify_stat_cache=False,
        same_second_edits=False, max_seconds=900, max_store_bytes=512*1024**2,
        max_store_files=20000, max_private_bytes=512*1024**2,
        max_logical_write_bytes=1024**3)
    try:
        verify_binary('before_run')
        driver.run(args)
        driver.require(len(details['maintenance']) == options.edits // 50 and len(details['verification']) == options.edits // 50 + 1,
                       'Workload did not complete every collection and restart verification')
        details['outcome'] = 'completed'
    except BaseException as error:
        details.update(outcome='failed', error=repr(error))
        raise
    finally:
        driver.Client = original
        driver.threading.Thread.start = original_thread_start
        details['threads'] = []
        details['children'] = []
        cleanup_errors = []
        for thread, ordinal, purpose in owned_threads:
            thread.join(timeout=10)
            details['threads'].append(dict(process=ordinal, purpose=purpose, joined=not thread.is_alive()))
            if thread.is_alive():
                cleanup_errors.append(f'Owned {purpose} for process {ordinal} did not join')
        for client in clients:
            process = getattr(client, 'process', None)
            if process is None:
                continue
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
            identity = driver.child_identity(process)
            for stream in (process.stdin, process.stdout):
                if stream is not None and not stream.closed:
                    stream.close()
            identity.update(process=client.ordinal,
                stdin_closed=process.stdin is None or process.stdin.closed,
                stdout_closed=process.stdout is None or process.stdout.closed)
            process._handle.Close()
            identity['owned_process_handle_closed'] = True
            details['children'].append(identity)
        try:
            verify_binary('after_run')
            driver.require(hashlib.sha256(Path(driver.__file__).read_bytes()).hexdigest() == DRIVER_SHA256,
                           'Frozen base driver changed during experiment')
        except BaseException as error:
            cleanup_errors.append(repr(error))
        if cleanup_errors:
            details.update(outcome='failed', cleanup_errors=cleanup_errors)
        if options.evidence.exists():
            with (options.evidence / 'collection-report.json').open('x', encoding='utf-8') as output:
                json.dump(details, output, indent=2)
                output.write('\n')
        driver.require(not cleanup_errors, f'Experiment teardown/identity verification failed: {cleanup_errors}')


if __name__ == '__main__':
    main()
