#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Qualify operated encrypted backup and restore through the app's normal backup routes.

The owner approves each services portal link out of band, through the ordinary
consent page. This driver writes each link to its private output directory and
waits for the app's own handoff to settle; it never touches storage directly.
"""
import argparse
import json
import os
from pathlib import Path
import time

from qualify_backup import ROUTE, TERMINAL, last_backup, owner, owner_raw, wait_backup
from qualify_serving import Phone, RECORDS, body_for, native, provenance, reconcile, record_uri


def init_journal(phone, name):
    owner(phone, 'GET', '/init/mark')
    owner(phone, 'POST', '/init/mark/lock', {})
    owner(phone, 'POST', '/init/finalize', {'name': name, 'timezone': 'America/Denver'})


def publish_portal(args, name, url, **context):
    assert isinstance(url, str) and url.startswith('https://'), url
    path = args.output / 'private' / f'portal-{name}.url'
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as stream:
        stream.write(url)
    print(json.dumps({'portal_ready': str(path), **context}), flush=True)


def settle(phone, kinds, seconds):
    """Poll status until an operation of one of these kinds reaches a terminal phase."""
    deadline = time.monotonic() + seconds
    seen = []
    while True:
        operation = owner(phone, 'GET', ROUTE + '/status').get('operation') or {}
        view = (operation.get('kind'), operation.get('phase'))
        if not seen or seen[-1] != view:
            seen.append(view)
        if operation.get('kind') in kinds and operation.get('phase') in TERMINAL:
            return operation, seen
        assert time.monotonic() < deadline, ('operation did not settle', kinds, seen)
        time.sleep(3)


def restore_phase(args, phone, check, source_identity, expected):
    """O4: restore the operated backup into a separate fresh journal on the same phone."""
    phone.control('serve-stop')
    args.suite = args.restore_suite
    phone.launch()
    init_journal(phone, 'Synthetic iOS operated restore')
    fresh_identity = owner(phone, 'GET', '/app/network/api/identity')
    absent = owner_raw(phone, 'GET', record_uri('operatedoriginal'))
    assert absent[0] == 404, absent
    recovery = (args.output / 'private' / 'recovery-key').read_text()
    prepared = owner(phone, 'POST', ROUTE + '/restore-hosted/prepare')
    capability = prepared['capability']
    keyed = owner(phone, 'POST', ROUTE + '/restore-hosted/key', {'capability': capability, 'recovery_key': recovery})
    publish_portal(args, 'restore', keyed['portal_url'], select=source_identity['instance_id'])
    owner(phone, 'POST', ROUTE + '/restore-hosted/arm', {'capability': capability})
    owner(phone, 'POST', ROUTE + '/restore-hosted/activate', {'capability': capability})
    restored, seen = settle(phone, ('restore_hosted', 'restore'), args.consent_seconds + 1800)
    status = owner(phone, 'GET', ROUTE + '/status')
    check('O4_restore', {'operation': restored, 'phases': seen, 'last_restore': status.get('last_restore'),
                         'fresh_identity': fresh_identity['instance_id'], 'hosted': status.get('hosted')})
    assert restored.get('phase') == 'done', restored
    assert (status.get('last_restore') or {}).get('status') == 'ok', status.get('last_restore')
    for anchor, body_value in expected.items():
        reconcile(owner(phone, 'GET', record_uri(anchor)), body_value)
    restored_identity = owner(phone, 'GET', '/app/network/api/identity')
    phone.launch()
    relaunched = owner(phone, 'GET', '/app/network/api/identity')
    reconcile(owner(phone, 'GET', record_uri('operatedchanged')), expected['operatedchanged'])
    check('O4_restored', {'source_identity': source_identity['instance_id'],
                          'restored_identity': restored_identity['instance_id'],
                          'relaunched_identity': relaunched['instance_id']})
    assert restored_identity['instance_id'] == source_identity['instance_id'], (restored_identity, source_identity)
    assert relaunched == restored_identity


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--host', required=True, help='Phone Wi-Fi IPv4 address')
    parser.add_argument('--port', type=int, default=37657)
    parser.add_argument('--suite', required=True, help='Fresh synthetic journal namespace')
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--restore-suite', help='Fresh second journal for the separate restore phase')
    parser.add_argument('--consent-seconds', type=int, default=1800)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    private = args.output / 'private'
    private.mkdir(mode=0o700)
    provenance(args)
    phone = Phone(args)
    results = {'suite': args.suite, 'checks': {}}

    def check(name, value):
        results['checks'][name] = value
        (args.output / 'result.json').write_text(json.dumps(results, indent=2))

    try:
        phone.launch()
        admission = [r['value'] for r in phone.rows if r.get('event') == 'backup.admission']
        check('O0_startup', admission)
        assert admission and admission[-1].get('admitted') is True, admission
        init_journal(phone, 'Synthetic iOS operated backup')
        owner(phone, 'POST', '/app/settings/api/facet', {'title': 'Work'})
        owner(phone, 'POST', ROUTE + '/keys/generate')
        recovery = owner(phone, 'POST', ROUTE + '/recovery-key/reveal')['recovery_key_display']
        descriptor = os.open(private / 'recovery-key', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'w') as stream:
            stream.write(recovery)
        owner(phone, 'POST', ROUTE + '/confirm', {'recovery_key': recovery})
        identity = owner(phone, 'GET', '/app/network/api/identity')

        # O1: the owner approves the operated binding through the ordinary portal page.
        status, value = owner_raw(phone, 'POST', ROUTE + '/enable-hosted')
        assert 200 <= status < 300, (status, value)
        publish_portal(args, 'enable', (value.get('operation') or {}).get('portal_url'))
        enabled, seen = settle(phone, ('enable_hosted',), args.consent_seconds)
        state = owner(phone, 'GET', ROUTE + '/status')
        check('O1_enable_hosted', {'operation': enabled, 'phases': seen, 'mode': state.get('mode'),
                                   'hosted': state.get('hosted'), 'instance': identity['instance_id']})
        assert enabled.get('phase') == 'done', enabled
        assert (state.get('hosted') or {}).get('bound') is True, state.get('hosted')

        # O2: original and changed snapshots through create-only operated writes.
        original_body = body_for('operatedoriginal')
        owner(phone, 'POST', RECORDS, original_body)
        previous = last_backup(phone).get('time')
        owner(phone, 'POST', ROUTE + '/backup-now')
        original = wait_backup(phone, previous, seconds=1800)
        changed_body = body_for('operatedchanged')
        owner(phone, 'POST', RECORDS, changed_body)
        owner(phone, 'POST', ROUTE + '/backup-now')
        changed = wait_backup(phone, original.get('time'), seconds=1800)
        check('O2_backups', {'original': original, 'changed': changed})
        assert original.get('status') == 'ok' and changed.get('status') == 'ok', (original, changed)
        assert original.get('snapshot_id') != changed.get('snapshot_id')

        # O3: the host process restarts and operated backup resumes (fresh credential, new lock).
        phone.launch()
        after_launch = last_backup(phone)
        owner(phone, 'POST', ROUTE + '/backup-now')
        relaunched = wait_backup(phone, after_launch.get('time'), seconds=1800)
        check('O3_relaunch', {'relaunched': relaunched})
        assert relaunched.get('status') == 'ok', relaunched

        if args.restore_suite:
            restore_phase(args, phone, check, identity,
                          {'operatedoriginal': original_body, 'operatedchanged': changed_body})
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
