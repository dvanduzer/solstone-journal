#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Verify copied app data using independent SQLite; summarize recorded device work."""
import argparse
import hashlib
import json
import math
import pathlib
import sqlite3
import statistics
import tarfile
import tempfile


def metrics(values):
    ordered = sorted(values)
    return dict(population=len(values), first=values[0], last=values[-1],
                minimum=ordered[0], median=statistics.median(ordered),
                p95=ordered[math.ceil(.95 * len(ordered)) - 1], maximum=ordered[-1])


def events(file):
    rows = [json.loads(line) for line in file.read_text().splitlines()]
    assert rows[-1]['event'] == 'complete' and rows[-1]['data']['passed'] is True
    return rows


def verify_files(archive):
    results = []
    with tempfile.TemporaryDirectory(prefix='journal-runtime-check-') as temporary:
        with tarfile.open(archive) as source:
            source.extractall(temporary, filter='data')
        for name, expected in [('synthetic-journal', 512), ('synthetic-journal-mixed', 4096)]:
            root = pathlib.Path(temporary) / 'files' / name
            population = {}
            for file in sorted((root / 'facets/work/activities').glob('*.jsonl')):
                records = [json.loads(line) for line in file.read_text().splitlines()]
                for i, row in enumerate(records):
                    population[f'{file.relative_to(root).as_posix()}:{i}'] = row['story']['body']
            assert len(population) == expected, (name, len(population))
            db = sqlite3.connect(f'file:{root}/indexer/journal.sqlite?mode=ro', uri=True)
            assert db.execute('PRAGMA quick_check').fetchone()[0] == 'ok'
            query = "SELECT path,idx,content FROM chunks WHERE chunks MATCH 'androidsentinel' ORDER BY path,idx"
            chunks = db.execute(query).fetchall()
            assert len(chunks) == expected
            keys = [f'{path}:{i}' for path, i, _ in chunks]
            assert len(set(keys)) == expected and set(keys) == set(population)
            for path, i, content in chunks:
                assert population[f'{path}:{i}'] in content, (path, i)
            assert db.execute("SELECT count(*) FROM chunks WHERE content='uncommittedghost'").fetchone()[0] == 0
            assert (root / 'checkpoint.json').read_bytes() == b'{"committed":true}'
            if expected == 512:
                assert not (root / 'exclusive-before').exists()
                assert (root / 'exclusive-after').read_bytes() == b'complete exclusive crash fixture'
                assert (root / 'atomic-barrier.json').read_bytes() == b'before rename, staged file synced'
                assert json.loads((root / 'transaction-open.json').read_text())['changed'] == 1
                stages = [p.read_bytes() for p in root.glob('.tmp_*.tmp')]
                assert b'interrupted replacement' in stages
                assert b'complete exclusive crash fixture' in stages
                for file in root.glob('exclusive-*.txt'):
                    assert file.read_bytes() == b'complete synthetic publication'
                winners = list(root.glob('publication-race-*/winner'))
                assert winners
                for winner in winners:
                    b = winner.read_bytes()
                    assert len(b) == 256 * 1024 and len(set(b)) == 1
            results.append(dict(journal=name, source_rows=expected, query=query,
                                exact_matching_chunks=len(chunks), quick_check='ok'))
    return results


def summarize(full, mixed):
    assert any(e['event'] == 'publication-race' and
               e['data']['native']['value']['winners'] == 1 and
               e['data']['native']['value']['complete_reader_observations'] > 0 for e in full)
    assert any(e['event'] == 'publication-faults' and
               e['data']['native']['value']['passed'] for e in full)
    authority = next(e['data']['native']['value'] for e in full if e['event'] == 'authority')
    assert all(authority.values())
    failures = [e['data']['operation'] for e in full if e['event'] == 'worker_failure_observed']
    assert set(failures) == {'crash', 'transaction-crash', 'atomic-crash',
                             'exclusive-crash-before', 'exclusive-crash-after'}
    cancel = next(e['data'] for e in full if e['event'] == 'cancelled_batch')
    assert cancel['response']['native']['value']['cancelled']
    work = [e['data'] for e in mixed if e['event'] in ['work', 'query', 'mutate']]
    assert len(work) == 360
    for row in work:
        assert row['native']['ok'] and row['native']['value']['sqlite_check'] == 'ok'
        assert row['native']['value']['result']['total'] == 4096
    resources = [e['data']['native']['value'] for e in mixed if e['event'] == 'resources']
    for row in resources:
        row['memory_kb'] = {line.split(':')[0]: int(line.split()[1])
                            for line in row['smaps_rollup'].splitlines()[1:] if ':' in line}
    ticks = next(e['data']['gaps_ms'] for e in mixed if e['event'] == 'ui_ticks')
    return dict(construction='all 360 mixed work/query/mutate replies; resource replies every 10 units plus endpoints; p95 nearest rank',
                worker_failures=failures, cancellation_ms=cancel['cancellation_ms'],
                repeated_use=next(e['data'] for e in mixed if e['event'] == 'repeated_use'),
                metrics={key: metrics([row[key] for row in work])
                         for key in ['native_ms', 'roundtrip_ms', 'pss_kb', 'java_heap']},
                memory={key: metrics([r['memory_kb'][key] for r in resources])
                        for key in ['Rss', 'Pss', 'Private_Dirty', 'Anonymous']},
                file_descriptors=metrics([r['fd_count'] for r in resources]),
                journal_bytes=metrics([r['journal_bytes'] for r in resources]),
                ui_timer_ms=metrics(ticks), resource_samples=resources)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--files', type=pathlib.Path, required=True)
    parser.add_argument('--full', type=pathlib.Path, required=True)
    parser.add_argument('--mixed', type=pathlib.Path, required=True)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    args = parser.parse_args()
    result = dict(independent=verify_files(args.files),
                  workload=summarize(events(args.full), events(args.mixed)),
                  sqlite_version=sqlite3.sqlite_version,
                  archive_sha256=hashlib.file_digest(args.files.open('rb'), 'sha256').hexdigest())
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'workload'}))


if __name__ == '__main__':
    main()
