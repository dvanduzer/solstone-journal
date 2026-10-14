#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import argparse
import pathlib
import tempfile
import unittest
from unittest.mock import patch
import json
import subprocess

import install_apk


class Continuity(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.apk = pathlib.Path(self.temp.name) / 'candidate.apk'
        self.apk.write_bytes(b'candidate')
        self.expected = 'a' * 64
        self.installed = self.expected
        self.calls = []
        self.uid_rows = ['package:app.solstone.development.journal uid:12345\n'] * 4
        self.install_failure = False
        self.replace_original = False
        self.args = argparse.Namespace(apk=self.apk, apk_sha256=install_apk.sha256(self.apk),
            signer_sha256=self.expected, application_id='app.solstone.development.journal',
            sdk=pathlib.Path('/sdk'), adb='adb', serial='test')

    def run_command(self, argv):
        args = list(map(str, argv))
        self.calls.append(args)
        if args[0].endswith('apksigner'):
            digest = self.expected if pathlib.Path(args[-1]).name == 'candidate.apk' else self.installed
            return 'Signer #1 certificate SHA-256 digest: ' + digest + '\n'
        if args[0].endswith('aapt2'):
            if self.replace_original:
                self.apk.write_bytes(b'replaced after staging')
            return "package: name='app.solstone.development.journal' versionCode='1'\n"
        if 'list' in args:
            return self.uid_rows.pop(0)
        if 'path' in args:
            return 'package:/data/app/base.apk\n'
        if 'pull' in args:
            pathlib.Path(args[-1]).write_bytes(b'candidate' if any('install' in call for call in self.calls) else b'installed')
            return ''
        if 'install' in args:
            if self.install_failure:
                raise install_apk.Refusal('synthetic install failure')
            return 'Success\n'
        self.fail(args)

    def assert_no_install(self):
        self.assertFalse(any('install' in call or 'uninstall' in call for call in self.calls))

    def test_signature_conflict_refuses_before_any_install(self):
        self.installed = 'b' * 64
        receipt = {}
        with self.assertRaisesRegex(install_apk.Refusal, 'conflicts'):
            install_apk.install(self.args, self.run_command, receipt)
        self.assert_no_install()
        self.assertEqual(receipt['prior_uid'], '12345')
        self.assertEqual(receipt['installed_signer_sha256'], self.installed)

    def test_wrong_digest_refuses_before_running_tools(self):
        self.args.apk_sha256 = 'f' * 64
        with self.assertRaisesRegex(install_apk.Refusal, 'digest'):
            install_apk.install(self.args, self.run_command, {})
        self.assertEqual(self.calls, [])

    def test_same_signer_update_is_single_streamed_install_with_same_uid(self):
        receipt = {}
        install_apk.install(self.args, self.run_command, receipt)
        installs = [call for call in self.calls if 'install' in call]
        self.assertEqual(len(installs), 1)
        self.assertEqual(installs[0][-4:-1], ['install', '--no-incremental', '-r'])
        self.assertNotEqual(installs[0][-1], str(self.apk))
        self.assertEqual(receipt['continuity'], 'same-signer-update')
        self.assertEqual(receipt['prior_uid'], receipt['after_uid'])
        self.assertEqual(receipt['after_apk_sha256'], self.args.apk_sha256)

    def test_rebuild_after_staging_cannot_change_installed_bytes(self):
        self.replace_original = True
        receipt = {}
        install_apk.install(self.args, self.run_command, receipt)
        self.assertNotEqual(install_apk.sha256(self.apk), self.args.apk_sha256)
        self.assertEqual(receipt['after_apk_sha256'], self.args.apk_sha256)

    def test_install_failure_never_retries_or_uninstalls(self):
        self.install_failure = True
        with self.assertRaises(install_apk.Refusal):
            install_apk.install(self.args, self.run_command, {})
        self.assertEqual(sum('install' in call for call in self.calls), 1)
        self.assertFalse(any('uninstall' in call for call in self.calls))

    def test_uid_race_refuses_before_install(self):
        self.uid_rows[1] = self.uid_rows[1].replace('12345', '12346')
        with self.assertRaisesRegex(install_apk.Refusal, 'during preflight'):
            install_apk.install(self.args, self.run_command, {})
        self.assert_no_install()

    def test_changed_uid_after_install_is_not_a_success(self):
        self.uid_rows[2] = self.uid_rows[2].replace('12345', '12346')
        with self.assertRaisesRegex(install_apk.Refusal, 'preserve'):
            install_apk.install(self.args, self.run_command, {})

    def test_production_wrapper_keeps_attempted_install_and_timeout_streams(self):
        receipt = pathlib.Path(self.temp.name) / 'receipt.json'
        argv = ['install_apk.py', '--apk', str(self.apk), '--apk-sha256', self.args.apk_sha256,
                '--signer-sha256', self.expected, '--application-id', self.args.application_id,
                '--sdk', '/sdk', '--adb', 'adb', '--serial', 'test', '--receipt', str(receipt)]

        def execute(command, **_kwargs):
            if 'install' in command:
                raise subprocess.TimeoutExpired(command, 180, output=b'partial stdout', stderr=b'partial stderr')
            return subprocess.CompletedProcess(command, 0, self.run_command(command), '')

        with patch('sys.argv', argv), patch.object(install_apk.subprocess, 'run', execute), patch('builtins.print'):
            self.assertEqual(install_apk.main(), 1)
        result = json.loads(receipt.read_text())
        attempts = [row for row in result['commands'] if 'install' in row['argv']]
        self.assertEqual(len(attempts), 1)
        self.assertIsNone(attempts[0]['exit'])
        self.assertTrue(attempts[0]['timeout'])
        self.assertEqual(attempts[0]['stdout'], 'partial stdout')
        self.assertEqual(attempts[0]['stderr'], 'partial stderr')
        self.assertFalse(any('uninstall' in row['argv'] for row in result['commands']))


if __name__ == '__main__':
    unittest.main()
