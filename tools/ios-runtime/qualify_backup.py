#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Qualify owner-managed encrypted backup through the app's normal backup routes.

The phone runs the ordinary serving composition. Every backup step goes through the
owner backup API; the mac reads the repository independently with restic and the
owner's recovery key, never the phone's daily key.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time
import wave

from qualify_audio import multipart
from qualify_serving import Client, Phone, native, provenance, body_for, reconcile, record_uri, RECORDS, SOURCE, DAY

ROUTE = '/app/backup'


def owner_raw(phone, method, uri, body=None):
    reply = phone.control('serve-owner', json.dumps({
        'method': method, 'uri': uri, 'body': '' if body is None else json.dumps(body)}))
    value = native(reply)
    text = value['body']
    return value['status'], (json.loads(text) if text else None)


def owner(phone, method, uri, body=None):
    status, value = owner_raw(phone, method, uri, body)
    assert 200 <= status < 300, (uri, status, value)
    return value


class Restic:
    """Independent repository reader using only the recovery key."""

    def __init__(self, args, repository, credentials, private):
        self.args = args
        self.repository = repository
        self.env = {**os.environ, 'AWS_ACCESS_KEY_ID': credentials['access_key_id'],
                    'AWS_SECRET_ACCESS_KEY': credentials['secret_access_key'],
                    'AWS_DEFAULT_REGION': credentials['region'], 'RESTIC_CACHE_DIR': str(private / 'cache')}
        self.private = private

    def run(self, name, password, *argv, check=True):
        secret = self.private / f'password-{name}'
        descriptor = os.open(secret, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(descriptor, 'w') as stream:
            stream.write(password)
        result = subprocess.run([str(self.args.restic), '-r', self.repository, '--password-file', str(secret),
                                 *argv], env=self.env, capture_output=True, text=True, timeout=600)
        secret.unlink()
        (self.args.output / f'restic-{name}.log').write_text(
            json.dumps({'argv': list(argv), 'exit': result.returncode, 'stdout': result.stdout[-65536:],
                        'stderr': result.stderr[-16384:]}, indent=2))
        if check:
            assert result.returncode == 0, (name, result.returncode, result.stderr[-2000:])
        return result

    def snapshots(self, name, key):
        return json.loads(self.run(name, key, 'snapshots', '--json', '--no-lock').stdout)

    def locks(self, name, key):
        return self.run(name, key, 'list', 'locks', '--no-lock').stdout.split()


def noise_wav(path, mib):
    with wave.open(str(path), 'wb') as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(16000)
        audio.writeframes(os.urandom(mib * 1024 * 1024))


TERMINAL = ('done', 'error', 'degraded', 'needs_subscription', 'refused')


def operate(phone, uri, body=None, kind=None, seconds=600):
    """Start an owner operation and return its HTTP status plus terminal view."""
    status, value = owner_raw(phone, 'POST', ROUTE + uri, body)
    if not 200 <= status < 300:
        return {'http': status, 'body': value}
    deadline = time.monotonic() + seconds
    while True:
        operation = (value or {}).get('operation')
        if operation is None and kind is None:
            return {'http': status, 'body': value}
        if operation and (kind is None or operation.get('kind') == kind) and operation.get('phase') in TERMINAL:
            return {'http': status, 'operation': operation}
        assert time.monotonic() < deadline, ('operation did not settle', uri, value)
        time.sleep(2)
        value = owner(phone, 'GET', ROUTE + '/status')


def last_backup(phone):
    return owner(phone, 'GET', ROUTE + '/status').get('last_backup') or {}


def wait_backup(phone, previous_time, seconds=900):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        current = last_backup(phone)
        if current.get('time') not in (None, previous_time):
            return current
        time.sleep(3)
    raise AssertionError('backup did not record a new outcome')


def restore_phase(args, phone, restic, repository, storage, canonical, wrong, snapshots, expected, check):
    """A7: restore the latest snapshot into a separate fresh journal on the same phone."""
    source_identity = owner(phone, 'GET', '/app/network/api/identity')
    phone.control('serve-stop')
    args.suite = args.restore_suite
    phone.launch()
    owner(phone, 'GET', '/init/mark')
    owner(phone, 'POST', '/init/mark/lock', {})
    owner(phone, 'POST', '/init/finalize', {'name': 'Synthetic iOS restore', 'timezone': 'America/Denver'})
    fresh_identity = owner(phone, 'GET', '/app/network/api/identity')
    credentials = {'access_key_id': storage['access_key_id'], 'secret_access_key': storage['secret_access_key']}
    body = {'repository': repository, 'backend': 's3', 'credentials': credentials}
    refused = operate(phone, '/restore', {**body, 'recovery_key': wrong}, kind='restore')
    absent = owner_raw(phone, 'GET', record_uri('backuporiginal'))
    assert refused.get('operation', {}).get('phase') != 'done', refused
    assert absent[0] == 404, absent
    restored = operate(phone, '/restore', {**body, 'recovery_key': canonical}, kind='restore', seconds=1800)
    status = owner(phone, 'GET', ROUTE + '/status')
    check('A7_restore', {'wrong_key': refused, 'wrong_key_record_status': absent[0], 'restore': restored,
                         'last_restore': status.get('last_restore'), 'source_identity': source_identity,
                         'fresh_identity': fresh_identity, 'snapshots': [s['id'] for s in snapshots]})
    assert restored.get('operation', {}).get('phase') == 'done', restored
    assert status['last_restore'].get('status') == 'ok', status['last_restore']
    for anchor, body_value in expected.items():
        reconcile(owner(phone, 'GET', record_uri(anchor)), body_value)
    restored_identity = owner(phone, 'GET', '/app/network/api/identity')
    phone.control('serve-index')
    hits = owner(phone, 'GET', '/app/search/api/search?q=lanservingqual%20backupchanged&limit=100')
    phone.launch()
    relaunched_identity = owner(phone, 'GET', '/app/network/api/identity')
    reconcile(owner(phone, 'GET', record_uri('backupchanged')), expected['backupchanged'])
    check('A7_restored', {'restored_identity': restored_identity, 'relaunched_identity': relaunched_identity,
                          'search_total': hits.get('total'),
                          'search_paths': [h['path'] for d in hits.get('days', []) for h in d['results']]})
    assert hits.get('total') == 1, hits
    assert relaunched_identity == restored_identity, (relaunched_identity, restored_identity)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--host', required=True, help='Phone Wi-Fi IPv4 address')
    parser.add_argument('--port', type=int, default=37657)
    parser.add_argument('--suite', required=True, help='Fresh synthetic journal namespace')
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--restic', type=Path, required=True, help='Independent restic reader on this mac')
    parser.add_argument('--storage', type=Path, required=True,
                        help='Private JSON: endpoint, region, bucket, access_key_id, secret_access_key')
    parser.add_argument('--prefix', required=True, help='Fresh repository prefix inside the owned bucket')
    parser.add_argument('--bulk-mib', type=int, default=24)
    parser.add_argument('--bulk-files', type=int, default=4)
    parser.add_argument('--restore-suite', help='Fresh second journal for the separate restore phase')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    private = args.output / 'private'
    private.mkdir(mode=0o700)
    provenance(args)
    storage = json.loads(args.storage.read_text())
    host = storage['endpoint'].removeprefix('https://').rstrip('/')
    repository = f"s3:https://{host}/{storage['bucket']}/{args.prefix.strip('/')}"
    restic = Restic(args, repository, storage, private)
    phone, client = Phone(args), Client(args)
    results = {'suite': args.suite, 'repository': repository, 'checks': {}}

    def check(name, value):
        results['checks'][name] = value
        (args.output / 'result.json').write_text(json.dumps(results, indent=2))

    try:
        phone.launch()
        check('A1_startup', [r['value'] for r in phone.rows if r.get('event') == 'backup.admission'])
        admission = results['checks']['A1_startup']
        assert admission and admission[-1].get('admitted') is True, admission
        owner(phone, 'GET', '/init/mark')
        owner(phone, 'POST', '/init/mark/lock', {})
        owner(phone, 'POST', '/init/finalize', {'name': 'Synthetic iOS backup', 'timezone': 'America/Denver'})
        owner(phone, 'POST', '/app/settings/api/facet', {'title': 'Work'})
        status = native(phone.control('serve-status'))
        assert status['door_address'] == f'{args.host}:{args.port}', status

        # A2: keys, independent recovery key, wrong confirmation, correct confirmation.
        owner(phone, 'POST', ROUTE + '/keys/generate')
        revealed = owner(phone, 'POST', ROUTE + '/recovery-key/reveal')
        recovery = revealed['recovery_key_display']
        canonical = ''.join(c for c in recovery.upper() if c.isalnum()).replace('I', '1').replace('L', '1').replace('O', '0')
        (private / 'recovery-key').write_text(recovery)
        (private / 'recovery-key').chmod(0o600)
        wrong = ('2' if canonical[0] != '2' else '3') + canonical[1:]
        refused = owner_raw(phone, 'POST', ROUTE + '/confirm', {'recovery_key': wrong})
        assert refused[0] == 400, refused
        owner(phone, 'POST', ROUTE + '/confirm', {'recovery_key': recovery})

        destination = owner(phone, 'POST', ROUTE + '/destination', {
            'repository': repository, 'backend': 's3',
            'credentials': {'access_key_id': storage['access_key_id'],
                            'secret_access_key': storage['secret_access_key']}})
        check('destination', destination)
        reach = destination.get('destination_status') or {}
        assert reach.get('reachable') is True and reach.get('repo_exists') is False, reach
        enabled = operate(phone, '/enable', kind='enable')
        check('A2_enable', enabled)
        assert enabled.get('operation', {}).get('phase') == 'done', enabled
        assert restic.run('config-recovery', canonical, 'cat', 'config', '--no-lock').returncode == 0
        wrong_read = restic.run('config-wrong', wrong, 'cat', 'config', '--no-lock', check=False)
        check('A4_wrong_key', {'restic_exit': wrong_read.returncode})
        assert wrong_read.returncode != 0
        restore_wrong = operate(phone, '/restore', {
            'repository': repository, 'backend': 's3', 'recovery_key': wrong,
            'credentials': {'access_key_id': storage['access_key_id'],
                            'secret_access_key': storage['secret_access_key']}}, kind='restore')
        check('A8_restore_wrong_key', restore_wrong)
        assert restore_wrong.get('operation', {}).get('phase') != 'done', restore_wrong

        # A3: original and changed snapshots, read back independently.
        suite_root = next(row['value']['reply']['root'] for row in phone.rows if row['event'] == 'serve.open')
        first = body_for('backuporiginal')
        owner(phone, 'POST', RECORDS, first)
        previous = last_backup(phone).get('time')
        owner(phone, 'POST', ROUTE + '/backup-now')
        original = wait_backup(phone, previous)
        assert original.get('status') == 'ok', original
        changed_body = body_for('backupchanged')
        owner(phone, 'POST', RECORDS, changed_body)
        owner(phone, 'POST', ROUTE + '/backup-now')
        changed = wait_backup(phone, original['time'])
        assert changed.get('status') == 'ok', changed
        snapshots = restic.snapshots('snapshots-a3', canonical)
        ids = [s['id'] for s in snapshots]
        assert original['snapshot_id'] in ids or any(i.startswith(original['snapshot_id']) for i in ids), (original, ids)
        path = f'{suite_root}/{SOURCE}'
        dumps = {}
        for name, snapshot in (('original', original), ('changed', changed)):
            listed = restic.run(f'find-{name}', canonical, 'find', '--no-lock', '--snapshot',
                                snapshot['snapshot_id'], '--json', f'{DAY}.jsonl').stdout
            matches = [m['path'] for group in json.loads(listed or '[]') for m in group.get('matches', [])]
            assert matches, (name, listed)
            dumps[name] = restic.run(f'dump-{name}', canonical, 'dump', '--no-lock',
                                     snapshot['snapshot_id'], matches[0]).stdout
        assert 'backuporiginal' in dumps['original'] and 'backupchanged' not in dumps['original'], dumps
        assert 'backupchanged' in dumps['changed'] and 'backuporiginal' in dumps['changed'], dumps
        check('A3_snapshots', {'original': original, 'changed': changed, 'remote_ids': ids,
                               'record_path': path})

        # A5: Stop through the app's owner ingress while a backup holds the repository.
        link = owner(phone, 'POST', '/app/network/pair-start', {'device_label': 'ios-backup-laptop'})
        assert client.call('pair', link=link['pair_link'], label='ios-backup-laptop')['ok']
        media = args.output / 'bulk'
        media.mkdir()
        for index in range(args.bulk_files):
            wav = media / f'bulk{index}_audio.wav'
            noise_wav(wav, args.bulk_mib)
            fields = multipart(media, f'13{index:02d}00_15', wav)
            reply = client.call('request', **fields)
            assert reply['ok'] and reply['value']['status'] == 200, reply
            wav.unlink()
        held_before = last_backup(phone)
        owner(phone, 'POST', ROUTE + '/backup-now')
        deadline = time.monotonic() + 300
        locks = []
        while time.monotonic() < deadline and not locks:
            locks = restic.locks('locks-held', canonical)
            if not locks:
                assert last_backup(phone).get('time') == held_before.get('time'), 'backup finished before Stop'
                time.sleep(0.5)
        assert locks, 'no repository lock observed while backup ran'
        stop_sent = time.time()
        stop_reply = phone.control('serve-stop')
        stop_returned = time.time()
        locks_after = restic.locks('locks-after-stop', canonical)
        after_stop = restic.snapshots('snapshots-after-stop', canonical)
        check('A5_stop_held', {'lock_seen': locks, 'stop_seconds': round(stop_returned - stop_sent, 3),
                               'stop_reply': stop_reply, 'locks_after': locks_after,
                               'snapshots_after': len(after_stop), 'snapshots_before': len(snapshots)})
        assert native(stop_reply).get('stopped') is True, stop_reply
        assert not locks_after, locks_after
        assert len(after_stop) == len(snapshots), 'cancelled backup published a snapshot'

        # A6: serving restarts in place, then the host process restarts; backup resumes.
        phone.control('serve-start', count=args.port)
        resumed_prior = last_backup(phone)
        check('A6_after_stop_status', resumed_prior)
        owner(phone, 'POST', ROUTE + '/backup-now')
        resumed = wait_backup(phone, resumed_prior.get('time'), seconds=1800)
        assert resumed.get('status') == 'ok', resumed
        phone.launch()
        after_launch = last_backup(phone)
        assert after_launch.get('snapshot_id') == resumed.get('snapshot_id'), (after_launch, resumed)
        owner(phone, 'POST', ROUTE + '/backup-now')
        relaunched = wait_backup(phone, after_launch.get('time'))
        assert relaunched.get('status') == 'ok', relaunched
        integrity = restic.run('check', canonical, 'check')
        final = restic.snapshots('snapshots-final', canonical)
        check('A6_restart', {'resumed': resumed, 'relaunched': relaunched, 'check_exit': integrity.returncode,
                             'snapshots': [s['id'] for s in final]})
        # A8 last: these must refuse; if one regressed it could not damage the checks above.
        def key_ids(name):
            listed = json.loads(restic.run(name, canonical, 'key', 'list', '--json', '--no-lock').stdout)
            return sorted(key['id'] for key in listed)
        keys_before = key_ids('keys-before')
        refusals = {}
        for uri, kind in (('/recovery-key/rotate', 'rotate'), ('/teardown', 'teardown')):
            refusals[uri] = operate(phone, uri, kind=kind)
            assert refusals[uri].get('operation', {}).get('phase') != 'done', refusals[uri]
        refusals['/offload/enable'] = owner_raw(phone, 'POST', ROUTE + '/offload/enable')
        offload = owner(phone, 'GET', ROUTE + '/status').get('offload') or {}
        assert offload.get('enabled') is not True, offload
        keys_after = key_ids('keys-after')
        check('A8_refusals', {'responses': refusals, 'keys_unchanged': keys_before == keys_after})
        assert keys_before == keys_after
        assert len(restic.snapshots('snapshots-a8', canonical)) == len(final)

        if args.restore_suite:
            restore_phase(args, phone, restic, repository, storage, canonical, wrong, final,
                          {'backuporiginal': first, 'backupchanged': changed_body}, check)
        results['passed'] = True
    except BaseException as error:
        results['passed'] = False
        results['error'] = repr(error)[:4000]
        raise
    finally:
        (args.output / 'result.json').write_text(json.dumps(results, indent=2))
        try:
            phone.control('serve-stop')
        except Exception as error:  # Record, never mask the original outcome.
            results['final_stop'] = repr(error)[:2000]
            (args.output / 'result.json').write_text(json.dumps(results, indent=2))
        for process in phone.processes:
            process.terminate()


if __name__ == '__main__':
    main()
