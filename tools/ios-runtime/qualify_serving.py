#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Qualify direct authenticated phone serving with synthetic activity records."""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import queue
import sqlite3
import subprocess
import threading
import time
import uuid

BUNDLE = 'app.solstone.journal.runtime.dev'
DAY = '20261012'
RECORDS = f'/app/activities/api/day/{DAY}/records?facet=work'
SOURCE = f'facets/work/activities/{DAY}.jsonl'


class Phone:
    def __init__(self, args):
        self.args = args
        self.rows = []
        self.processes = []
        self.log = (args.output / 'phone.jsonl').open('w')

    def launch(self, extra_args=()):
        self.events = queue.Queue()
        process = subprocess.Popen([
            'xcrun', 'devicectl', 'device', 'process', 'launch', '--device', self.args.device,
            '--console', '--terminate-existing', BUNDLE, '--serve', '--suite', self.args.suite,
            '--port', str(self.args.port), '--duration', '1700', *extra_args],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.processes.append(process)
        events = self.events

        def capture():
            for line in process.stdout:
                self.log.write(line)
                self.log.flush()
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                self.rows.append(row)
                events.put(row)
            events.put(None)
        threading.Thread(target=capture, daemon=True).start()
        return self.wait(lambda r: r['event'] == 'serve.start')

    def wait(self, predicate, seconds=160):
        deadline = time.monotonic() + seconds
        while True:
            row = self.events.get(timeout=max(0.01, deadline - time.monotonic()))
            assert row is not None, 'phone console ended before expected event'
            assert row['event'] != 'host.failed', row
            if predicate(row):
                return row

    def control(self, op, payload='', count=128, mode='reopen'):
        identifier = uuid.uuid4().hex
        command = self.args.output / 'command.json'
        command.write_text(json.dumps({'id': identifier, 'request': {
            'op': op, 'journal': self.args.suite, 'root': '', 'count': count,
            'seconds': 0, 'revision': 0, 'mode': mode, 'storage': 'serving', 'payload': payload}}))
        subprocess.run(['xcrun', 'devicectl', 'device', 'copy', 'to', '--device', self.args.device,
                        '--domain-type', 'appDataContainer', '--domain-identifier', BUNDLE,
                        '--source', str(command), '--destination', 'Documents/owner-command.json'],
                       check=True, stdout=subprocess.DEVNULL)
        reply = self.wait(lambda r: r['event'] == 'owner.reply' and r['value'].get('id') == identifier)['value']
        assert reply['ok'], reply
        return reply['reply']

    def owner(self, method, uri, body=None):
        reply = self.control('serve-owner', json.dumps({
            'method': method, 'uri': uri, 'body': '' if body is None else json.dumps(body)}))
        value = native(reply)
        assert 200 <= value['status'] < 300, value
        return json.loads(value['body'])


def native(reply):
    result = reply['journal']['result']
    assert result['ok'], result
    return result['value']


class Client:
    def __init__(self, args):
        self.args = args
        self.process = subprocess.Popen([str(args.client)], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, text=True)
        self.log = (args.output / 'network.jsonl').open('w')

    def call(self, op, **fields):
        command = {'op': op, 'host': self.args.host, 'port': self.args.port,
                   'credential': str(self.args.output / 'credential.json'), **fields}
        self.process.stdin.write(json.dumps(command) + '\n')
        self.process.stdin.flush()
        reply = json.loads(self.process.stdout.readline())
        # Pair links and credential material stay out of durable receipts.
        receipt = {'time': time.time(), 'command': {k: v for k, v in command.items() if k != 'link'}, 'reply': reply}
        self.log.write(json.dumps(receipt) + '\n')
        self.log.flush()
        return reply

    def http(self, method, uri, body=None, status=200, **fields):
        reply = self.call('request', method=method, uri=uri,
                          body='' if body is None else json.dumps(body), **fields)
        assert reply['ok'] and reply['value']['status'] == status, reply
        return json.loads(reply['value']['body'])


def body_for(anchor):
    return {'activity': 'meeting', 'title': f'lanservingqual {anchor}',
            'description': f'Synthetic copper kite {anchor}', 'details': f'Exact payload {anchor}.',
            'since_segment': anchor, 'source': 'user'}


def record_uri(anchor):
    return f'/app/activities/api/day/{DAY}/record/meeting_{anchor}?facet=work'


def reconcile(actual, expected):
    actual = actual.get('record', actual)
    for key in ('activity', 'title', 'description', 'details', 'source'):
        assert actual[key] == expected[key], (key, actual, expected)
    assert actual['id'] == 'meeting_' + expected['since_segment'], actual


def search(client, anchor):
    result = client.http('GET', '/app/search/api/search?q=lanservingqual%20' + anchor + '&limit=100')
    assert result['total'] == 1, result
    hits = [hit for day in result['days'] for hit in day['results']]
    assert len(hits) == 1 and hits[0]['path'] == SOURCE, hits
    return result


def provenance(args):
    root = Path(__file__).resolve().parents[2]
    dirty = subprocess.check_output(['git', 'status', '--porcelain'], cwd=root, text=True)
    assert not dirty, f'qualification requires committed source: {dirty}'
    paths = subprocess.check_output(['git', 'ls-files', '-z'], cwd=root).split(b'\0')
    paths += subprocess.check_output(['git', 'ls-files', '--others', '--exclude-standard', '-z'], cwd=root).split(b'\0')
    hashes = {}
    for raw in paths:
        if raw:
            path = Path(raw.decode())
            if (root / path).is_file():
                hashes[str(path)] = hashlib.sha256((root / path).read_bytes()).hexdigest()
    result = {'head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(),
              'files': hashes, 'captured_at': time.time(),
              'client_sha256': hashlib.sha256(args.client.read_bytes()).hexdigest()}
    (args.output / 'source.json').write_text(json.dumps(result, sort_keys=True, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--host', required=True, help='Phone Wi-Fi IPv4 address')
    parser.add_argument('--port', type=int, default=37657)
    parser.add_argument('--suite', required=True, help='Fresh synthetic journal namespace')
    parser.add_argument('--cycles', type=int, default=120)
    parser.add_argument('--interval', type=float, default=5)
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    provenance(args)
    phone = Phone(args)
    client = Client(args)
    started = time.time()
    expected = {}
    try:
        phone.launch()
        phone.control('serve-status')
        phone.owner('GET', '/init/api/state')
        phone.owner('GET', '/init/mark')
        phone.owner('POST', '/init/mark/lock', {})
        phone.owner('POST', '/init/finalize', {'name': 'Synthetic iOS LAN', 'timezone': 'America/Denver'})
        status = native(phone.control('serve-status'))
        assert status['door_address'] == f'{args.host}:{args.port}', status
        phone.owner('POST', '/app/settings/api/facet', {'title': 'Work'})
        phone.control('serve-stop')
        baseline = native(phone.control('serve-inventory'))
        phone.control('serve-start', count=args.port)
        link = phone.owner('POST', '/app/network/pair-start', {'device_label': 'ios-serving-laptop'})
        paired = client.call('pair', link=link['pair_link'], label='ios-serving-laptop')
        assert paired['ok'], paired
        identity = client.http('GET', '/app/network/api/identity')
        # A live window admits only pairing, never a protected write or search.
        phone.owner('POST', '/app/network/pair-start', {'device_label': 'unused-negative-window'})
        for auth in ('missing', 'invalid'):
            for method, uri, body in [('GET', '/app/search/api/search?q=lanservingqual', None),
                                      ('POST', RECORDS, body_for('denied' + auth))]:
                refused = client.call('request', auth=auth, method=method, uri=uri,
                                      body='' if body is None else json.dumps(body))
                assert not refused['ok'] or refused['value']['status'] in (401, 403), refused
        # Invalid bootstrap nonce cannot create a client.
        refused = client.call('request', auth='missing', method='POST', uri='/app/network/pair',
                              body=json.dumps({'nonce': '00' * 16, 'csr': 'invalid', 'device_label': 'bad-nonce'}))
        assert refused['ok'] and refused['value']['status'] == 410, refused
        assert json.loads(refused['value']['body'])['reason_code'] == 'operation_no_longer_available', refused

        first = body_for('firstrecord')
        reconcile(client.http('POST', RECORDS, first), first)
        expected['firstrecord'] = first
        indexed = phone.control('serve-index')
        host_root = next(row['value']['reply']['root'] for row in phone.rows
                         if row['event'] == 'serve.open')
        assert indexed['root'] == host_root, (indexed['root'], host_root)
        assert '/Shared/AppGroup/' in host_root, host_root
        search(client, 'firstrecord')
        for loss in ('before', 'after'):
            anchor = 'loss' + loss
            body = body_for(anchor)
            lost = client.call('loss', loss=loss, method='POST', uri=RECORDS, body=json.dumps(body))
            assert lost['ok'] and lost['value']['loss'] == loss, lost
            if loss == 'before':
                client.http('GET', record_uri(anchor), status=404)
                reconcile(client.http('POST', RECORDS, body), body)
            else:
                reconcile(client.http('GET', record_uri(anchor)), body)
                client.http('POST', RECORDS, body, status=409)
            expected[anchor] = body
        phone.control('serve-restart-worker')
        restarted = next(row['value']['reply'] for row in reversed(phone.rows)
                         if row['event'] == 'serve.restarted')
        assert restarted['root'] == host_root, restarted
        assert client.http('GET', '/app/network/api/identity') == identity
        for anchor, body in expected.items():
            reconcile(client.http('GET', record_uri(anchor)), body)
        phone.control('serve-index')
        search(client, 'firstrecord')
        phone.launch()  # --terminate-existing kills the actual observed host.
        reopened = next(row['value']['reply'] for row in reversed(phone.rows)
                        if row['event'] == 'serve.open')
        assert reopened['root'] == host_root, reopened
        assert client.http('GET', '/app/network/api/identity') == identity
        for anchor, body in expected.items():
            reconcile(client.http('GET', record_uri(anchor)), body)
        search(client, 'firstrecord')

        workload_started = time.time()
        for number in range(args.cycles):
            anchor = f'record{number:05d}'
            body = body_for(anchor)
            reconcile(client.http('POST', RECORDS, body), body)
            expected[anchor] = body
            phone.control('serve-index')
            search(client, anchor)
            if number % 10 == 0:
                assert client.call('connect')['ok']
                assert client.call('held', method='GET', uri='/app/network/api/identity')['ok']
                assert client.call('disconnect')['ok']
            phone.control('serve-status')
            time.sleep(args.interval)
        workload_seconds = time.time() - workload_started
        # Revocation must close a carrier that was already authorized.
        assert client.call('connect')['ok']
        assert client.call('held', method='GET', uri='/app/network/api/identity')['ok']
        phone.owner('POST', '/app/network/unpair', {'device_label': 'ios-serving-laptop'})
        closed = client.call('closed')
        assert closed['ok'] and closed['value']['closed'], closed
        for op in ('held', 'request'):
            revoked = client.call(op, method='GET', uri='/app/search/api/search?q=lanservingqual')
            assert not revoked['ok'] or revoked['value']['status'] in (401, 403), revoked
        phone.control('serve-stop')
        inventory = native(phone.control('serve-inventory'))
        rows = [json.loads(line) for line in inventory['files'][SOURCE].splitlines() if line.strip()]
        assert len(rows) == len(expected)
        assert {row['id'] for row in rows} == {'meeting_' + a for a in expected}
        for row in rows:
            reconcile(row, expected[row['id'].removeprefix('meeting_')])
        # Control against a checker that accepts a wrong payload with the right count.
        wrong = dict(rows[0], title='wrong')
        try:
            reconcile(wrong, expected[wrong['id'].removeprefix('meeting_')])
        except AssertionError:
            pass
        else:
            raise AssertionError('source checker accepted wrong payload')
        phone.control('export')
        exported = next(row['value']['reply'] for row in reversed(phone.rows) if row['event'] == 'export')
        dbpath = args.output / 'journal.sqlite'
        dbpath.write_bytes(base64.b64decode(exported['indexer/journal.sqlite'], validate=True))
        if 'indexer/journal.sqlite-wal' in exported:
            dbpath.with_name(dbpath.name + '-wal').write_bytes(base64.b64decode(exported['indexer/journal.sqlite-wal'], validate=True))
        with sqlite3.connect(dbpath.as_uri() + '?mode=ro', uri=True) as db:
            assert db.execute('pragma integrity_check').fetchall() == [('ok',)]
            chunks = db.execute('select path,content from chunks where chunks match ?', ('lanservingqual',)).fetchall()
            assert len(chunks) == len(expected), chunks
            assert {path for path, _ in chunks} == {SOURCE}
            for anchor in expected:
                assert sum(anchor in content for _, content in chunks) == 1, anchor
        phone.control('serve-finish')
        beats = [row for row in phone.rows if row['event'] == 'host.heartbeat']
        assert beats and all(row['value']['state'] == 0 for row in beats), 'host left foreground'
        workers = [row['value']['reply']['journal']['pid'] for row in phone.rows if row['event'] == 'open']
        hosts = [row['host_pid'] for row in phone.rows if row['event'] == 'host.start']
        assert len(set(workers)) >= 3 and len(set(hosts)) == 2, (workers, hosts)
        result = {'passed': True, 'started_at': started, 'finished_at': time.time(),
                  'workload_seconds': workload_seconds, 'workload_cycles': args.cycles,
                  'activity_population': len(expected), 'baseline_storage': baseline,
                  'final_storage': inventory, 'worker_pids': workers, 'host_pids': hosts,
                  'shared_root': host_root,
                  'thermal_states': sorted({row['value']['thermal'] for row in beats}),
                  'foreground_heartbeats': len(beats), 'negative_control': 'wrong source payload refused'}
        (args.output / 'result.json').write_text(json.dumps(result, indent=2, sort_keys=True))
        print(json.dumps({k: v for k, v in result.items() if k not in ('baseline_storage', 'final_storage')}))
    finally:
        client.process.stdin.close()
        client.process.wait(timeout=35)
        for process in phone.processes:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)


if __name__ == '__main__':
    main()
