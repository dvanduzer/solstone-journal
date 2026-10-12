#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Capture a physical run and independently validate exported FTS and fixture bytes."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import threading

BUNDLE = 'app.solstone.journal.runtime.dev'


def value(row):
    result = row['value']['reply']['journal']['result']
    assert result['ok'], result
    return result['value']


def assert_mapping(actual, expected):
    assert actual == expected, "paths or content differ"


def verify(rows):
    events = {r['event']: r for r in rows}
    assert 'host.failed' not in events and events['host.finished']['value']['passed']
    start = events['host.start']
    count = start['value']['count']
    scenario = start['value']['scenario']
    beats = [r for r in rows if r['event'] == 'host.heartbeat']
    assert all(r['value']['state'] == 0 for r in beats), 'host left foreground'
    assert events['host.finished']['value']['state'] == 0
    assert start['value']['state'] == 0
    stamps = [start['uptime']] + [r['uptime'] for r in beats] + [events['host.finished']['uptime']]
    max_gap = max(b - a for a, b in zip(stamps, stamps[1:]))
    assert max_gap < 3, f'foreground heartbeat gap {max_gap}'
    assert events['authority']['value']['reply']['host_file_readable'] is False
    inventory = value(events['final.inventory'])
    actual = {f['name']: f['text'] for f in inventory['files']}
    assert set(actual) == {f'fixture{i:04}.md' for i in range(count)}
    # Compute expected bytes independently from the synthetic workload schedule.
    units = events.get('mixed.completed', {}).get('value', {}).get('units', 0)
    for i in range(count):
        revisions = [n for n in range(1, units + 1) if n % 32 == i]
        revision = revisions[-1] if revisions else 0
        expected = f'# Synthetic memory {i}\n\nauroraruntime document {i}: copper kite revision {revision}.\n'
        assert actual[f'fixture{i:04}.md'] == expected, i
    expected = {f'20261012/talents/{name}': text.rstrip() for name, text in actual.items()}
    results = value(events['final.query'])['results']
    assert_mapping({r['metadata']['path']: r['text'] for r in results}, expected)
    exported = events['export']['value']['reply']
    data = base64.b64decode(exported['indexer/journal.sqlite'], validate=True)
    with tempfile.TemporaryDirectory() as tmp:
        p = Path(tmp) / 'index.sqlite'
        p.write_bytes(data)
        if 'indexer/journal.sqlite-wal' in exported:
            p.with_name(p.name + '-wal').write_bytes(base64.b64decode(exported['indexer/journal.sqlite-wal'], validate=True))
        with sqlite3.connect(p.as_uri() + '?mode=ro', uri=True) as db:
            assert db.execute('pragma integrity_check').fetchall() == [('ok',)]
            matches = dict(db.execute('select path,content from chunks where chunks match ?', ('auroraruntime',)))
            assert matches == expected
            assert db.execute('select count(*) from files').fetchone()[0] == count
    worker_pids = [r['value']['reply']['journal']['pid'] for r in rows if r['event'] == 'open']
    if scenario == 'acceptance':
        assert len(worker_pids) == 5 and len(set(worker_pids[:3])) == 3
        assert start['host_pid'] not in worker_pids
        assert inventory['exclusive'] == 'create once'
        assert 'create.refused' in events
        refused = events['create.unexpected']['value']['reply']['journal']['result']
        assert refused['ok'] is False and 'destination already exists' in refused['error']
        binding = value(events['binding.control'])
        assert binding['refused'] and binding['both_absent']
        recovered = value(events['crash-write.reopened'])['before']
        assert recovered['checkpoint'] == 'prior complete checkpoint'
        assert any(name.startswith('.tmp_') for name in recovered['entries'])
        assert value(events['commit-disconnect.reopened'])['committed'] == 'committed before reply loss'
        assert base64.b64decode(exported['checkpoint.bin']) == b'recovered complete checkpoint'
        assert base64.b64decode(exported['committed.bin']) == b'committed before reply loss'
        cancellation = value(events['cancel.work'])
        assert cancellation['cancelled'] and 1 <= cancellation['seconds'] < 4
        assert events['cancel.control']['value']['roundtrip_ms'] < 1000
        assert events['idle.after']['value']['reply']['journal']['pid'] == worker_pids[-1]
    if scenario in ('mixed', 'controls'):
        for r in rows:
            if r['event'].startswith('mixed.') and r['event'] != 'mixed.completed':
                assert value(r)['failed'] == 0 and not value(r)['warnings']
    # A control for the checker: one wrong path must differ even at the same count.
    wrong = dict(expected)
    wrong['wrong.md'] = wrong.pop(next(iter(wrong)))
    try:
        assert_mapping(wrong, expected)
    except AssertionError:
        pass
    else:
        raise AssertionError('checker accepted wrong path')
    return {'passed': True, 'scenario': scenario, 'count': count, 'worker_pids': worker_pids,
            'host_pid': start['host_pid'], 'duration_seconds': events['host.finished']['time'] - start['time'],
            'foreground_heartbeats': len(beats), 'max_heartbeat_gap_seconds': max_gap, 'mixed_units': units,
            'sqlite_bytes': len(data), 'sqlite_sha256': hashlib.sha256(data).hexdigest(),
            'sizes': inventory['sizes'], 'negative_control': 'wrong path differs'}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--device', required=True)
    p.add_argument('--suite', required=True, help='Fresh synthetic namespace; reopen reuses it')
    p.add_argument('--scenario', choices=['acceptance', 'controls', 'mixed', 'reopen', 'hostloss'], default='acceptance')
    p.add_argument('--count', type=int, default=128)
    p.add_argument('--duration', type=int, default=1200)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--verify-only', action='store_true')
    p.add_argument('--metrics-udid', help='Stream DVT worker counters to a sibling JSONL file')
    args = p.parse_args()
    if not args.verify_only:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        command = ['xcrun', 'devicectl', 'device', 'process', 'launch', '--device', args.device,
                   '--console', '--terminate-existing', BUNDLE, '--qualify', '--suite', args.suite,
                   '--scenario', args.scenario, '--count', str(args.count), '--duration', str(args.duration)]
        with args.output.open('w') as log:
            if args.scenario == 'hostloss' or args.metrics_udid:
                process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                killed = False
                monitor = None
                monitor_error = None
                timeout = threading.Timer(1850, process.kill)
                timeout.start()
                try:
                    for line in process.stdout:
                        log.write(line)
                        log.flush()
                        try:
                            row = json.loads(line)
                        except ValueError:
                            continue
                        if args.metrics_udid and monitor is None and row.get('event') == 'index':
                            pid = row['value']['reply']['journal']['pid']
                            monitor_error = args.output.with_suffix('.metrics.stderr').open('w')
                            monitor = subprocess.Popen(['pymobiledevice3', 'developer', 'dvt', 'sysmon',
                                'process', 'monitor', 'process', '--udid', args.metrics_udid,
                                '--filter', f'pid={pid}', '--interval', '1000', '--duration', '1800000',
                                '--output', str(args.output.with_suffix('.metrics.jsonl'))],
                                stdout=monitor_error, stderr=subprocess.STDOUT)
                        if row.get('event') == 'host.kill-ready':
                            kill = subprocess.run(['xcrun', 'devicectl', 'device', 'process', 'terminate',
                                                   '--device', args.device, '--pid', str(row['host_pid'])],
                                                  capture_output=True, text=True, timeout=30, check=True)
                            args.output.with_suffix('.termination.json').write_text(json.dumps({
                                'host_pid': row['host_pid'], 'exit_code': kill.returncode,
                                'stdout': kill.stdout, 'stderr': kill.stderr}, indent=2))
                            killed = True
                    code = process.wait(timeout=30)
                    if args.scenario == 'hostloss':
                        assert killed and code != 0, 'host loss did not occur'
                    else:
                        assert code == 0, f'device run exited {code}'
                finally:
                    timeout.cancel()
                    if monitor is not None:
                        monitor.terminate()
                        monitor.wait(timeout=30)
                        monitor_error.close()
                    if process.poll() is None:
                        process.kill()
                        process.wait()
                if args.scenario == 'hostloss':
                    print('Host killed; run reopen with the same suite and count to verify recovery.')
                    return
                completed = subprocess.CompletedProcess(command, code)
            else:
                completed = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, timeout=1850)
        if completed.returncode:
            raise SystemExit(f'device run exited {completed.returncode}; see {args.output}')
    rows = []
    for line in args.output.read_text().splitlines():
        try:
            row = json.loads(line)
            if isinstance(row, dict) and 'event' in row:
                rows.append(row)
        except ValueError:
            pass
    receipt = verify(rows)
    args.output.with_suffix('.verification.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps(receipt, indent=2))


if __name__ == '__main__':
    main()
