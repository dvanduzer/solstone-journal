#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Install an exact development APK after checking signer and UID continuity."""

import argparse
import copy
import hashlib
import json
import pathlib
import re
import shutil
import subprocess
import tempfile


class Refusal(RuntimeError):
    pass


def sha256(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def signer(output):
    certificates = re.findall(r'^Signer #\d+ certificate SHA-256 digest: ([0-9a-f]{64})$', output, re.M)
    if len(certificates) != 1:
        raise Refusal('one verified APK signer required')
    return certificates[0]


def install(args, run, receipt):
    # Freeze the selected bytes before verification. A concurrent build must
    # not replace the APK between signer checks and the actual ADB invocation.
    with tempfile.TemporaryDirectory(prefix='journal-apk-candidate-') as temp:
        staged = pathlib.Path(temp) / 'candidate.apk'
        shutil.copyfile(args.apk, staged)
        selected = copy.copy(args)
        selected.apk = staged
        _install_staged(selected, run, receipt)


def _install_staged(args, run, receipt):
    if sha256(args.apk) != args.apk_sha256:
        raise Refusal('candidate APK digest differs')
    tools = args.sdk / 'build-tools/35.0.0'
    candidate_signer = signer(run([tools / 'apksigner', 'verify', '--verbose', '--print-certs', args.apk]))
    receipt['candidate_signer_sha256'] = candidate_signer
    if candidate_signer != args.signer_sha256:
        raise Refusal('candidate signer differs from expected retained signer')
    badging = run([tools / 'aapt2', 'dump', 'badging', args.apk])
    packages = re.findall(r"^package: name='([^']+)'", badging, re.M)
    if packages != [args.application_id]:
        raise Refusal('candidate application identity differs')
    adb = [args.adb, '-s', args.serial]

    def uid():
        result = run(adb + ['shell', 'cmd', 'package', 'list', 'packages', '-U', args.application_id])
        rows = re.findall(r'^package:' + re.escape(args.application_id) + r' uid:(\d+)\s*$', result, re.M)
        if len(rows) > 1 or (result.strip() and not rows):
            raise Refusal('installed application UID is ambiguous')
        return rows[0] if rows else None

    prior_uid = uid()
    receipt['prior_uid'] = prior_uid
    if prior_uid is not None:
        paths = run(adb + ['shell', 'pm', 'path', args.application_id]).splitlines()
        if len(paths) != 1 or not paths[0].startswith('package:/'):
            raise Refusal('one installed base APK required')
        with tempfile.TemporaryDirectory(prefix='journal-apk-continuity-') as temp:
            installed = pathlib.Path(temp) / 'installed.apk'
            run(adb + ['pull', paths[0][len('package:'):], installed])
            receipt['installed_apk_sha256'] = sha256(installed)
            installed_signer = signer(run([tools / 'apksigner', 'verify', '--verbose', '--print-certs', installed]))
        receipt['installed_signer_sha256'] = installed_signer
        if installed_signer != candidate_signer:
            raise Refusal('installed signer conflicts; installation refused and no uninstall requested')
        if uid() != prior_uid:
            raise Refusal('installed UID changed during preflight')
    # Never uninstall or retry an install failure. Incremental installation has
    # another transport/fallback path, so this tool requests the streamed path.
    run(adb + ['install', '--no-incremental', '-r', args.apk])
    after_uid = uid()
    receipt['after_uid'] = after_uid
    if after_uid is None or (prior_uid is not None and after_uid != prior_uid):
        raise Refusal('install did not preserve the existing application UID')
    paths = run(adb + ['shell', 'pm', 'path', args.application_id]).splitlines()
    if len(paths) != 1 or not paths[0].startswith('package:/'):
        raise Refusal('one installed base APK required after install')
    with tempfile.TemporaryDirectory(prefix='journal-apk-result-') as temp:
        observed = pathlib.Path(temp) / 'installed.apk'
        run(adb + ['pull', paths[0][len('package:'):], observed])
        receipt['after_apk_sha256'] = sha256(observed)
    if receipt['after_apk_sha256'] != args.apk_sha256 or uid() != after_uid:
        raise Refusal('installed bytes or UID changed; exact installation unconfirmed')
    receipt['continuity'] = 'same-signer-update' if prior_uid is not None else 'new-install'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--apk', required=True, type=pathlib.Path)
    parser.add_argument('--apk-sha256', required=True)
    parser.add_argument('--signer-sha256', required=True)
    parser.add_argument('--application-id', required=True)
    parser.add_argument('--sdk', required=True, type=pathlib.Path)
    parser.add_argument('--adb', required=True)
    parser.add_argument('--serial', required=True)
    parser.add_argument('--receipt', required=True, type=pathlib.Path)
    args = parser.parse_args()
    for value in (args.apk_sha256, args.signer_sha256):
        if re.fullmatch('[0-9a-f]{64}', value) is None:
            parser.error('SHA-256 identities require 64 lowercase hexadecimal characters')
    if re.fullmatch(r'[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+', args.application_id) is None:
        parser.error('invalid application identity')
    if args.receipt.exists():
        parser.error('fresh receipt path required')
    receipt = {'apk_sha256': args.apk_sha256, 'serial': args.serial,
               'application_id': args.application_id, 'commands': []}

    def run(argv):
        command = list(map(str, argv))
        row = {'argv': command, 'exit': None}
        receipt['commands'].append(row)
        try:
            result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                    stderr=subprocess.PIPE, text=True, timeout=180)
        except subprocess.TimeoutExpired as error:
            def text(value):
                return value.decode(errors='replace') if isinstance(value, bytes) else value
            row.update(timeout=True, stdout=text(error.stdout), stderr=text(error.stderr))
            raise Refusal('command timed out; no retry or uninstall requested') from error
        except OSError as error:
            row['error'] = str(error)
            raise
        row.update(exit=result.returncode, stdout=result.stdout, stderr=result.stderr)
        if result.returncode:
            raise Refusal('command failed; no retry or uninstall requested')
        return result.stdout

    try:
        install(args, run, receipt)
    except (Refusal, OSError, subprocess.SubprocessError) as error:
        receipt.update(exit=1, refusal=str(error))
    else:
        receipt['exit'] = 0
    args.receipt.parent.mkdir(parents=True, exist_ok=True)
    args.receipt.write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({key: value for key, value in receipt.items() if key != 'commands'}))
    return receipt['exit']


if __name__ == '__main__':
    raise SystemExit(main())
