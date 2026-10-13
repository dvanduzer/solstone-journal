#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Qualify the synthetic Android server using private USB controls and direct LAN SPL."""
import argparse
import hashlib
import json
import pathlib
import selectors
import shlex
import sqlite3
import subprocess
import tarfile
import time
import uuid
from datetime import datetime, timezone

PACKAGE = 'app.solstone.development.journal'
ACTIVITY = PACKAGE + '/.ServingActivity'


class Qualification:
    def __init__(self, args):
        self.args = args
        self.out = pathlib.Path(args.output).resolve()
        self.out.mkdir(parents=True, exist_ok=False)
        self.log = (self.out / 'events.jsonl').open('w')
        self.adb = [args.adb, '-s', args.serial]
        self.client = subprocess.Popen([args.client], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                       stderr=(self.out / 'client.stderr').open('w'), text=True, bufsize=1)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.client.stdout, selectors.EVENT_READ)
        self.credential = str(self.out / 'credential.json')
        self.marker = 'androidnetwork' + uuid.uuid4().hex[:12]
        self.day = datetime.now(timezone.utc).strftime('%Y%m%d')
        self.records = f'/app/activities/api/day/{self.day}/records?facet=work'
        self.expected = []

    def event(self, kind, **data):
        row = dict(event=kind, epoch=time.time(), **data)
        self.log.write(json.dumps(row) + '\n')
        self.log.flush()

    def command(self, argv, *, data=None, check=True):
        p = subprocess.run(argv, input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
        if check and p.returncode:
            raise RuntimeError(f'{argv}: exit {p.returncode}: {p.stderr.decode(errors="replace")}')
        return p

    def shell(self, *args, **kwargs):
        return self.command(self.adb + ['shell', shlex.join(args)], **kwargs)

    def control(self, op, **kwargs):
        identifier = uuid.uuid4().hex
        command = dict(op=op, id=identifier, **kwargs)
        self.shell('run-as', PACKAGE, 'sh', '-c',
                   'cat > files/serving-command.stage && mv files/serving-command.stage files/serving-command.json',
                   data=json.dumps(command).encode())
        deadline = time.monotonic() + 95
        while time.monotonic() < deadline:
            p = self.command(self.adb + ['exec-out', 'run-as', PACKAGE, 'cat', 'files/serving-reply.json'], check=False)
            try:
                row = json.loads(p.stdout)
            except (ValueError, UnicodeError):
                row = {}
            if row.get('id') == identifier:
                self.event('control', command=command, reply=row)
                if not row.get('native', {}).get('ok'):
                    raise RuntimeError(f'owner control failed: {row}')
                return row
            time.sleep(.25)  # Bounded private IPC reply wait, not build/status polling.
        raise TimeoutError(f'private owner control timed out: {command}')

    def owner(self, method, uri, body=None, expected=200):
        reply = self.control('owner', method=method, uri=uri,
                             body='' if body is None else json.dumps(body))['native']['value']
        assert reply['status'] == expected, reply
        try:
            return json.loads(reply['body'])
        except ValueError:
            return reply['body']

    def network(self, op='request', expected=200, allow_error=False, **kwargs):
        command = dict(op=op, host=self.args.host, port=self.args.port, credential=self.credential, **kwargs)
        self.client.stdin.write(json.dumps(command) + '\n')
        self.client.stdin.flush()
        if not self.selector.select(40):
            raise TimeoutError(f'SPL client reply timed out: {op}')
        reply = json.loads(self.client.stdout.readline())
        self.event('network', command=command, reply=reply)
        if allow_error:
            assert not reply['ok'] or reply.get('value', {}).get('status', 200) in (401, 403), reply
            return reply
        assert reply['ok'], reply
        value = reply['value']
        if 'status' in value:
            assert value['status'] == expected, value
            return json.loads(value['body'])
        return value

    def text(self, suffix):
        anchor = self.marker + suffix
        return dict(activity='note', title='Synthetic LAN note ' + anchor,
                    description='Synthetic journal network qualification',
                    details=self.marker + ' ' + anchor + ' synthetic text committed through the production activity writer.',
                    since_segment=anchor)

    def read(self, suffix, expected=200):
        return self.network(method='GET', uri=f'/app/activities/api/day/{self.day}/record/note_{self.marker}{suffix}?facet=work', expected=expected)

    def write(self, suffix):
        result = self.network(method='POST', uri=self.records, body=json.dumps(self.text(suffix)))
        assert result['record']['details'] == self.text(suffix)['details'], result
        self.expected.append('note_' + self.marker + suffix)

    def query(self):
        result = self.network(method='GET', uri='/app/search/api/search?q=' + self.marker)
        assert result['total'] == len(self.expected), (result, self.expected)
        return result

    def sample(self, label):
        row = self.control('status')
        for service in ('thermalservice', 'battery'):
            output = self.shell('dumpsys', service).stdout.decode(errors='replace')
            (self.out / f'{label}-{service}.txt').write_text(output)
        proc = self.shell('run-as', PACKAGE, 'cat', f'/proc/{row["pid"]}/status').stdout.decode()
        (self.out / f'{label}-worker-status.txt').write_text(proc)
        storage = self.shell('run-as', PACKAGE, 'du', '-sk', 'files/synthetic-serving-journal').stdout.decode()
        (self.out / f'{label}-storage.txt').write_text(storage)
        activity = self.shell('dumpsys', 'activity', 'activities').stdout.decode(errors='replace')
        resumed = [line for line in activity.splitlines() if 'mResumedActivity:' in line or 'topResumedActivity=' in line]
        assert any(ACTIVITY in line or PACKAGE + '/.ServingActivity' in line for line in resumed), resumed
        self.event('resources', label=label, process=row, resumed=resumed)
        return row

    def launch(self):
        self.shell('input', 'keyevent', 'KEYCODE_WAKEUP')
        self.shell('wm', 'dismiss-keyguard')
        self.shell('am', 'start', '-n', ACTIVITY)
        time.sleep(2)

    def run(self):
        self.launch()
        initial = self.control('status')
        self.event('source', head=self.command(['git', 'rev-parse', 'HEAD']).stdout.decode().strip(),
                   marker=self.marker, day=self.day, client_sha256=hashlib.sha256(pathlib.Path(self.args.client).read_bytes()).hexdigest())
        apk_path = self.shell('pm', 'path', PACKAGE).stdout.decode().strip().removeprefix('package:')
        apk = self.command(self.adb + ['exec-out', 'cat', apk_path]).stdout
        digest = hashlib.sha256(apk).hexdigest()
        assert digest == self.args.apk_sha256, (digest, self.args.apk_sha256)
        self.event('installed_apk', sha256=digest, bytes=len(apk))
        mark = self.owner('GET', '/init/mark')
        if not mark['locked']:
            locked = self.owner('POST', '/init/mark/lock', {})
            assert locked['mark'] == mark['mark'], (locked, mark)
        self.owner('POST', '/init/finalize', {})
        state = self.control('status')['native']['value']
        assert state['door_address'] == f'{self.args.host}:{self.args.port}', state
        identity = self.owner('GET', '/app/network/api/identity')
        self.event('identity', identity=identity)
        pair = self.owner('POST', '/app/network/pair-start', {'device_label': self.marker, 'role': ''})
        self.network('pair', link=pair['pair_link'], label=self.marker)
        self.network(method='GET', uri='/api/system/status', auth='invalid', allow_error=True)
        self.network(method='GET', uri='/api/system/status', auth='missing', allow_error=True)
        # An open consent window allows certless TLS, but protected routes remain denied.
        self.owner('POST', '/app/network/pair-start', {'device_label': 'synthetic-unpaired', 'role': ''})
        self.network(method='GET', uri='/api/system/status', auth='missing', allow_error=True)
        facets = self.network(method='GET', uri='/app/settings/api/facets')
        if not any(facet.get('name') == 'work' for facet in facets['facets']):
            self.network(method='POST', uri='/app/settings/api/facet', body=json.dumps({'title': 'Work'}), expected=201)
        activities = self.network(method='GET', uri='/app/settings/api/facet/work/activities')
        if not any(a.get('id') == 'note' for a in activities['activities']):
            self.network(method='POST', uri='/app/settings/api/facet/work/activities', body=json.dumps({'id': 'note', 'name': 'Note'}), expected=201)
        self.write('initial')
        self.control('index')
        self.query()
        self.network('loss', loss='before', method='POST', uri=self.records, body=json.dumps(self.text('before')))
        time.sleep(.2)
        self.read('before', expected=404)
        self.write('before')
        self.network('loss', loss='after', method='POST', uri=self.records, body=json.dumps(self.text('after')))
        assert self.read('after')['record']['details'] == self.text('after')['details']
        self.expected.append('note_' + self.marker + 'after')
        self.network(method='POST', uri=self.records, body=json.dumps(self.text('after')), expected=409)
        self.control('index')
        self.query()
        old = self.control('status')
        self.shell('run-as', PACKAGE, 'kill', '-9', str(old['pid']))
        time.sleep(3)
        new = self.control('status')
        assert old['pid'] != new['pid'] and new['uid'] == old['uid'], (old, new)
        self.query()
        self.event('worker_recovery', before=old, after=new)
        self.shell('am', 'force-stop', PACKAGE)
        self.launch()
        restarted = self.control('status')
        assert old['host_pid'] != restarted['host_pid'], (old, restarted)
        assert self.owner('GET', '/app/network/api/identity') == identity
        self.query()
        self.event('host_recovery', after=restarted)
        self.sample('start')
        started = time.monotonic()
        cycles = 0
        while time.monotonic() - started < self.args.duration:
            self.write(f'cycle{cycles:05}')
            self.control('index')
            self.query()
            self.network('connect')
            self.network('held', method='GET', uri='/api/system/status')
            self.network('disconnect')
            cycles += 1
            if cycles % 12 == 0:
                self.sample(f'cycle{cycles:05}')
            time.sleep(self.args.interval)
        self.sample('finish')
        self.event('repeated_use', cycles=cycles, seconds=time.monotonic()-started,
                   source_records=len(self.expected), diagnostic_gc=False)
        self.network('connect')
        self.network('held', method='GET', uri='/api/system/status')
        self.owner('POST', '/app/network/unpair', {'device_label': self.marker})
        self.network('closed')
        self.network('disconnect')
        self.network(method='GET', uri='/api/system/status', allow_error=True)
        self.control('stop')
        self.network(method='GET', uri='/api/system/status', allow_error=True)
        self.shell('am', 'force-stop', PACKAGE)
        snapshot = self.command(self.adb + ['exec-out', 'run-as', PACKAGE, 'tar', '-cf', '-', 'files/synthetic-serving-journal', 'files/worker-evidence.jsonl']).stdout
        (self.out / 'files.tar').write_bytes(snapshot)
        self.verify_snapshot()
        self.event('complete', passed=True, source_records=len(self.expected))

    def verify_snapshot(self):
        destination = self.out / 'snapshot'
        with tarfile.open(self.out / 'files.tar') as archive:
            archive.extractall(destination, filter='data')
        root = destination / 'files/synthetic-serving-journal'
        source = root / f'facets/work/activities/{self.day}.jsonl'
        rows = [json.loads(line) for line in source.read_text().splitlines()]
        ids = [row['id'] for row in rows if self.marker in row['id']]
        assert sorted(ids) == sorted(self.expected), (ids, self.expected)
        for row in rows:
            if self.marker in row['id']:
                suffix = row['id'].removeprefix('note_' + self.marker)
                assert row['details'] == self.text(suffix)['details'], row
        databases = list(root.rglob('*.sqlite'))
        checks = []
        for db in databases:
            connection = sqlite3.connect(f'file:{db}?mode=ro', uri=True)
            check = connection.execute('pragma integrity_check').fetchone()[0]
            assert check == 'ok', (db, check)
            fts = connection.execute('select count(*) from chunks where chunks match ?', (self.marker,)).fetchone()[0]
            assert fts == len(self.expected), (fts, self.expected)
            checks.append(dict(path=str(db.relative_to(root)), integrity=check, fts_records=fts))
            connection.close()
        assert checks, 'no independent SQLite snapshot found'
        result = dict(marker=self.marker, expected_ids=self.expected, exact_unique_source_records=True,
                      source_sha256=hashlib.sha256(source.read_bytes()).hexdigest(), sqlite=checks,
                      journal_bytes=sum(p.stat().st_size for p in root.rglob('*') if p.is_file()))
        (self.out / 'independent-verification.json').write_text(json.dumps(result, indent=2) + '\n')

    def close(self):
        self.client.stdin.close()
        try:
            self.client.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.client.terminate()
            self.client.wait(timeout=5)
        self.selector.close()
        self.log.close()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--adb', default='adb')
    p.add_argument('--serial', default='RZGL11XCS9D')
    p.add_argument('--package', default=PACKAGE)
    p.add_argument('--host', required=True)
    p.add_argument('--port', type=int, default=7657)
    p.add_argument('--client', required=True)
    p.add_argument('--apk-sha256', required=True)
    p.add_argument('--output', required=True)
    p.add_argument('--duration', type=float, default=1200)
    p.add_argument('--interval', type=float, default=5)
    args = p.parse_args()
    if args.package not in ('app.solstone.development.journal','app.solstone.development.journal.audio'):
        p.error('development application required')
    globals()['PACKAGE'] = args.package
    globals()['ACTIVITY'] = args.package + '/app.solstone.development.journal.ServingActivity'
    q = Qualification(args)
    try:
        q.run()
    except BaseException as error:
        q.event('failed', error=str(error))
        raise
    finally:
        q.close()


if __name__ == '__main__':
    main()
