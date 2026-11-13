"""Fresh producer capture, existing model evaluation, and a failing quality gate."""
import json
import time

import frozen_task_capture as capture
import frozen_task_codex as executor
import frozen_task_eval as evaluation
import frozen_task_run as collection


def check_current(spec, destination, pins, executable, timeout_seconds=900):
    destination = capture.plain(destination)
    spec = capture.plain(spec)
    pinned = json.loads(capture.read(pins))
    source = capture.plain(pinned['source_root'])
    capture.require(not destination.is_relative_to(spec) and
                    not destination.is_relative_to(source),
                    'Current check evidence must be outside the recipe and source roots')
    destination.mkdir()  # Never reuse an earlier capture, answer, or judgment.
    started = time.monotonic()
    state = dict(status='running', stage='capture', started_at=collection.now(),
                 retention='Retain private capture, model replies and judgments for review; '
                           'failed runs are not automatically retried or deleted.')

    def save():
        (destination / 'current-check.json').write_bytes(collection.encoded(state))

    save()
    try:
        state['capture'] = capture.prepare_current(spec, destination / 'capture', pins)
        state['stage'] = 'prepare'; save()
        run = destination / 'run'
        collection.prepare(destination / 'capture', run, 'evaluation', 'manual_packet_v1')
        state['stage'] = 'execute'; save()
        state['execution'] = executor.execute(run, executable, timeout_seconds)
        capture.require(state['execution']['status'] == 'complete', 'Task execution did not complete')
        state['stage'] = 'quality'; save()
        manifest, oracle = evaluation.load(run)
        capture.require(manifest.get('producer_kind') == 'current_producer',
                        'Current verification requires freshly captured producer evidence')
        result = evaluation.quality_check(manifest, evaluation.results(run, manifest, oracle))
        # Use the sealed capture's pins, not a possibly edited caller pin file.
        observed = evaluation.read_json(run, 'provenance/pins.json')
        capture.require(observed == pinned, 'Capture pins changed during preparation')
        capture.require(capture.file_hash(observed['binary']) == observed['binary_sha256'] and
                        capture.file_hash(observed['build_manifest']) == observed['build_manifest_sha256'],
                        'Installed build changed during task evaluation')
        state.update(status='current_producer_quality_failed' if result['failed_cases'] else
                     'current_producer_quality_passed', stage='complete',
                     exit_code=int(bool(result['failed_cases'])), finished_at=collection.now(),
                     elapsed_seconds=time.monotonic() - started, quality=result,
                     producer=observed, capture_manifest_sha256=collection.digest(
                         (destination / 'capture/manifest.json').read_bytes()),
                     limits='Fresh capture and model evaluation of the supplied four frozen cases. '
                            'Producer freshness and selected source hashes were checked during capture. '
                            'The build identity was rechecked after evaluation. This is not a fresh '
                            'holdout, continuous monitoring, or demonstrated implementation/QA benefit.')
        save()
        return state
    except BaseException as error:
        state.update(status='failed', error=type(error).__name__ + ': ' + str(error),
                     exit_code=1, finished_at=collection.now(),
                     elapsed_seconds=time.monotonic() - started)
        save()
        raise
