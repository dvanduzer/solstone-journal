#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Failure controls for the maintained Android audio snapshot oracles."""
import hashlib
import io
import json
import pathlib
import sqlite3
import subprocess
import tarfile
import tempfile
import unittest
from unittest import mock

from qualify_audio import (AudioQualification, capture_audio_snapshot, matching_audio_failure,
                           require_absent_audio_artifacts, snapshot_file_names, snapshot_members,
                           verify_snapshot_databases)


FILES = {'files/synthetic-serving-journal/note.txt': b'synthetic note',
         'files/worker-evidence.jsonl': b'{}\n'}


def archive(entries=None):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode='w') as bundle:
        for name, content, kind in entries or [(n, b, tarfile.REGTYPE) for n, b in FILES.items()]:
            member = tarfile.TarInfo(name)
            member.type = kind
            member.size = len(content) if kind == tarfile.REGTYPE else 0
            bundle.addfile(member, io.BytesIO(content) if member.isfile() else None)
    return stream.getvalue()


class Phone:
    """Controlled command boundary; invokes the real capture orchestration."""
    def __init__(self, out, *, fault=None, files=None):
        self.out, self.fault = out, fault
        self.adb = ['controlled-adb']
        self.files = FILES if files is None else files
        self.raw = archive([(n, b, tarfile.REGTYPE) for n, b in self.files.items()])
        self.finds = self.type_finds = 0
        self.calls = []
        self.injected = False

    def result(self, stdout=b'', returncode=0, stderr=b''):
        return subprocess.CompletedProcess([], returncode, stdout, stderr)

    def shell(self, *args, **kwargs):
        self.calls.append(args)
        if 'sh' in args:
            if 'snapshot' in args:
                self.paths = args[args.index('snapshot') + 1:args.index('snapshot') + 4]
                witness = args[args.index('snapshot') + 4]
                if self.fault == 'timeout':
                    self.injected = True
                    raise subprocess.TimeoutExpired('controlled creation', 120,
                                                    output=b'partial creation', stderr=b'partial diagnostic')
                if self.fault == 'tar_status':
                    self.injected = True
                    return self.result((witness + ':2\n').encode(), returncode=2)
                if self.fault == 'disconnect':
                    self.injected = True
                    return self.result(returncode=1, stderr=b'error: device disconnected\n')
                if self.fault == 'controller_disagreement':
                    self.injected = True
                    return self.result((witness + ':0\n').encode(), returncode=1,
                                       stderr=b'late transport failure')
                return self.result((witness + ':0\n').encode())
            if 'write-list' in args:
                self.list_data = kwargs['data']
                return self.result()
            if 'cleanup' in args:
                self.cleaned = True
                if self.fault == 'cleanup':
                    self.injected = True
                    return self.result(returncode=1)
                return self.result()
        if 'sha256sum' in args:
            if self.fault == 'disconnect_digest':
                self.injected = True
                return self.result(returncode=1, stderr=b'device disconnected')
            digest = hashlib.sha256(self.raw).hexdigest()
            if self.fault == 'digest':
                self.injected = True
                digest = '0' * 64
            return self.result((digest + '  owned.tar\n').encode())
        if 'stat' in args:
            if self.fault == 'disconnect_size':
                self.injected = True
                return self.result(returncode=1, stderr=b'device disconnected')
            size = len(self.raw)
            if self.fault == 'size':
                self.injected = True
                size += 512
            return self.result(str(size).encode())
        if 'find' in args:
            if '!' in args:
                self.type_finds += 1
                if self.fault == 'symlink':
                    self.injected = True
                    return self.result(b'files/synthetic-serving-journal/unsafe-link\0')
                if self.fault == 'disconnect_types_after' and self.type_finds == 2:
                    self.injected = True
                    return self.result(returncode=1, stderr=b'device disconnected')
                return self.result()
            if args[args.index('-type') + 1] == 's':
                return self.result(b'files/synthetic-serving-journal/health/callosum.sock\0')
            self.finds += 1
            if self.fault == 'disconnect_after' and self.finds == 2:
                self.injected = True
                return self.result(returncode=1, stderr=b'device disconnected')
            names = list(self.files)
            if self.fault == 'population' and self.finds == 2:
                self.injected = True
                names.append('files/synthetic-serving-journal/omitted.txt')
            return self.result(b''.join(n.encode() + b'\0' for n in names))
        raise AssertionError(('unexpected controlled shell', args, kwargs))

    def command(self, args, **kwargs):
        self.calls.append(tuple(args))
        if args[-1] == self.paths[1]:
            if self.fault == 'disconnect_stderr':
                self.injected = True
                return self.result(b'partial stderr', returncode=1, stderr=b'device disconnected')
            return self.result(b'tar: ignored socket\n')
        if args[-1] == self.paths[0]:
            if self.fault == 'disconnect_archive':
                self.injected = True
                return self.result(b'partial archive', returncode=1, stderr=b'device disconnected')
            return self.result(self.raw)
        raise AssertionError(('unexpected controlled command', args))


class CaptureTests(unittest.TestCase):
    def test_explicit_null_list_preserves_newline_name_and_omits_only_socket(self):
        with tempfile.TemporaryDirectory() as directory:
            files = {**FILES, 'files/synthetic-serving-journal/with\nnewline': b'newline fixture'}
            phone = Phone(pathlib.Path(directory), files=files)
            capture_audio_snapshot(phone)
            self.assertEqual(set(phone.list_data[:-1].decode().split('\0')), set(files))
            launch = next(call for call in phone.calls if 'snapshot' in call)
            self.assertIn('tar --null -T', launch[4])
            receipt = json.loads((phone.out / 'snapshot-capture.json').read_text())
            self.assertTrue(receipt['passed'])
            self.assertEqual(receipt['omitted_socket_paths'],
                             ['files/synthetic-serving-journal/health/callosum.sock'])

    def test_separate_warning_and_actual_complete_archive(self):
        with tempfile.TemporaryDirectory() as directory:
            phone = Phone(pathlib.Path(directory))
            capture_audio_snapshot(phone)
            receipt = json.loads((phone.out / 'snapshot-capture.json').read_text())
            self.assertTrue(receipt['passed'])
            self.assertEqual(receipt['actual_tar_exit'], 0)
            self.assertEqual(receipt['cleanup'], 'removed_and_absence_checked')
            self.assertEqual((phone.out / 'files.tar').read_bytes(), phone.raw)
            self.assertEqual((phone.out / 'snapshot-tar.stderr').read_bytes(), b'tar: ignored socket\n')

    def test_capture_failures_never_report_success(self):
        for fault, error, message in [('tar_status', RuntimeError, 'tar exited 2'),
                                      ('digest', ValueError, 'transport digest or size differs'),
                                      ('size', ValueError, 'transport digest or size differs'),
                                      ('population', ValueError, 'population changed'),
                                      ('symlink', ValueError, 'unsupported nonregular'),
                                      ('cleanup', RuntimeError, 'cleanup failed')]:
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as directory:
                phone = Phone(pathlib.Path(directory), fault=fault)
                with self.assertRaisesRegex(error, message):
                    capture_audio_snapshot(phone)
                self.assertTrue(phone.injected, 'negative must reach its injected boundary')
                receipt = json.loads((phone.out / 'snapshot-capture.json').read_text())
                self.assertFalse(receipt['passed'])
                if fault == 'tar_status':
                    self.assertEqual((phone.out / 'files.tar').read_bytes(), phone.raw)
                    self.assertEqual(phone.finds, 1)

    def test_timeout_retains_partial_evidence_without_retry_or_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            phone = Phone(pathlib.Path(directory), fault='timeout')
            with self.assertRaises(subprocess.TimeoutExpired):
                capture_audio_snapshot(phone)
            self.assertTrue(phone.injected)
            self.assertEqual(sum('snapshot' in call for call in phone.calls), 1)
            self.assertFalse(any('cleanup' in call for call in phone.calls))
            receipt = json.loads((phone.out / 'snapshot-capture.json').read_text())
            self.assertFalse(receipt['passed'])
            self.assertEqual(receipt['remote_settlement'], 'uncertain_after_timeout')
            self.assertEqual(receipt['cleanup'], 'not_attempted')
            self.assertEqual(receipt['owned_paths'], list(phone.paths))
            self.assertEqual((phone.out / 'snapshot-timeout.stdout').read_bytes(), b'partial creation')
            self.assertEqual((phone.out / 'snapshot-timeout.stderr').read_bytes(), b'partial diagnostic')

    def test_disconnect_without_remote_completion_retains_owned_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            phone = Phone(pathlib.Path(directory), fault='disconnect')
            with self.assertRaisesRegex(RuntimeError, 'completion is uncertain'):
                capture_audio_snapshot(phone)
            self.assertTrue(phone.injected)
            self.assertEqual(sum('snapshot' in call for call in phone.calls), 1)
            self.assertFalse(any('cleanup' in call for call in phone.calls))
            receipt = json.loads((phone.out / 'snapshot-capture.json').read_text())
            self.assertFalse(receipt['passed'])
            self.assertNotIn('actual_tar_exit', receipt)
            self.assertEqual(receipt['creation_controller_exit'], 1)
            self.assertEqual(receipt['remote_settlement'], 'uncertain_without_completion_witness')
            self.assertEqual(receipt['cleanup'], 'not_attempted')
            self.assertEqual((phone.out / 'snapshot-launch.stderr').read_bytes(), b'error: device disconnected\n')

    def test_disconnect_after_tar_completion_retains_evidence_and_cache(self):
        for boundary in ('stderr', 'digest', 'size', 'archive', 'after', 'types_after'):
            with self.subTest(boundary=boundary), tempfile.TemporaryDirectory() as directory:
                phone = Phone(pathlib.Path(directory), fault='disconnect_' + boundary)
                with self.assertRaisesRegex(RuntimeError, 'command failed: 1'):
                    capture_audio_snapshot(phone)
                self.assertTrue(phone.injected)
                self.assertFalse(any('cleanup' in call for call in phone.calls))
                self.assertEqual(sum('snapshot' in call for call in phone.calls), 1)
                receipt = json.loads((phone.out / 'snapshot-capture.json').read_text())
                self.assertFalse(receipt['passed'])
                self.assertEqual(receipt['actual_tar_exit'], 0)
                self.assertTrue(receipt['remote_settlement'].startswith('uncertain_after_'))
                self.assertEqual(receipt['cleanup'], 'not_attempted')
                self.assertEqual(receipt['owned_paths'], list(phone.paths))
                if boundary == 'archive':
                    self.assertEqual((phone.out / 'files.tar').read_bytes(), b'partial archive')
                elif boundary == 'stderr':
                    self.assertEqual((phone.out / 'snapshot-read-stderr.stdout').read_bytes(), b'partial stderr')

    def test_controller_disagreement_retains_paths_without_transfer_or_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            phone = Phone(pathlib.Path(directory), fault='controller_disagreement')
            with self.assertRaisesRegex(RuntimeError, 'controller status differs'):
                capture_audio_snapshot(phone)
            self.assertTrue(phone.injected)
            self.assertFalse(any('cleanup' in call or 'exec-out' in call for call in phone.calls))
            receipt = json.loads((phone.out / 'snapshot-capture.json').read_text())
            self.assertFalse(receipt['passed'])
            self.assertEqual(receipt['actual_tar_exit'], 0)
            self.assertEqual(receipt['creation_controller_exit'], 1)
            self.assertEqual(receipt['cleanup'], 'not_attempted')


class ArchiveTests(unittest.TestCase):
    def test_unsafe_or_duplicate_source_names_refused_before_tar(self):
        for name in ('files/synthetic-serving-journal/../outside',
                     '/files/synthetic-serving-journal/absolute',
                     next(iter(FILES))):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, 'unsafe|duplicate'):
                snapshot_file_names(b''.join(n.encode() + b'\0' for n in [*FILES, name]))
    def test_regular_population(self):
        self.assertEqual(len(snapshot_members(archive(), FILES)), 2)

    def test_unsafe_duplicate_nonregular_or_incomplete_refused(self):
        cases = [('files/synthetic-serving-journal/../escape', tarfile.REGTYPE, 'unsafe'),
                 ('/files/synthetic-serving-journal/escape', tarfile.REGTYPE, 'unsafe'),
                 ('files/worker-evidence.jsonl/child', tarfile.REGTYPE, 'unsafe'),
                 (next(iter(FILES)), tarfile.REGTYPE, 'duplicate'),
                 ('files/synthetic-serving-journal/link', tarfile.SYMTYPE, 'nonregular')]
        for name, kind, message in cases:
            with self.subTest(name=name, kind=kind):
                entries = [(n, b, tarfile.REGTYPE) for n, b in FILES.items()]
                entries.append((name, b'x', kind))
                with self.assertRaisesRegex(ValueError, message):
                    snapshot_members(archive(entries), FILES)
        with self.assertRaisesRegex(ValueError, 'population differs'):
            snapshot_members(archive(), [*FILES, 'files/synthetic-serving-journal/missing'])

    def test_worker_evidence_directory_cannot_replace_regular_file(self):
        entries = [(next(iter(FILES)), b'synthetic note', tarfile.REGTYPE),
                   ('files/worker-evidence.jsonl', b'', tarfile.DIRTYPE)]
        with self.assertRaisesRegex(ValueError, 'missing regular worker evidence'):
            snapshot_members(archive(entries), [next(iter(FILES))])


class FailureEvidenceTests(unittest.TestCase):
    base = 'files/synthetic-serving-journal/chronicle/20261014/device_audio/120000_3/audio'

    def status(self):
        target = dict(generation=7, input='/data/data/package/' + self.base + '.wav',
                      error='size differs from pinned model')
        other = dict(generation=7, input='/data/data/package/older/audio.m4a',
                     error='duration exceeds 30 seconds')
        return dict(state='failed', **{k: other[k] for k in ('input', 'error')},
                    generation=7, recent_failures=[target, other])

    def test_overwritten_headline_uses_current_exact_attempt(self):
        state = self.status()
        failure = matching_audio_failure(state, self.base + '.wav', 'size differs')
        self.assertEqual(failure, state['recent_failures'][0])
        self.assertNotEqual(failure['input'], state['input'])

    def test_stale_wrong_input_wrong_error_or_malformed_history_refused(self):
        state = self.status()
        self.assertIsNone(matching_audio_failure(state, self.base + '.wav', 'open Parakeet'))
        self.assertIsNone(matching_audio_failure(state, self.base.replace('device_audio', 'other_stream') + '.wav', 'size differs'))
        state['generation'] = 8
        self.assertIsNone(matching_audio_failure(state, self.base + '.wav', 'size differs'))
        for field, value in [('generation', None), ('generation', True), ('recent_failures', None),
                             ('recent_failures', [*self.status()['recent_failures'], {}])]:
            state = self.status(); state[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                matching_audio_failure(state, self.base + '.wav', 'size differs')

    def test_real_wait_returns_correlated_record_and_checks_empty_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            base = str(pathlib.Path(directory) / 'audio')
            q = object.__new__(AudioQualification)
            q.input_path = lambda item: base
            state = self.status()
            state['recent_failures'][0]['input'] = '/phone/' + base + '.wav'
            q.control = lambda op: dict(native=dict(value=dict(audio=state)))
            q.event = mock.Mock()

            def shell(*args):
                # Execute the real POSIX absence probe against actual empty files.
                return subprocess.run(list(args[2:]), capture_output=True, check=True)

            q.shell = shell
            item = dict(fixture='speech.wav')
            result = q.wait_failure(item, 'size differs')
            self.assertEqual(result, state['recent_failures'][0])
            self.assertEqual(item['failure'], result)
            for suffix in ('.jsonl', '.npz'):
                path = pathlib.Path(base + suffix); path.write_bytes(b'')
                self.assertTrue(path.exists()); self.assertEqual(path.stat().st_size, 0)
                with self.subTest(suffix=suffix), self.assertRaisesRegex(ValueError, 'artifact exists'):
                    q.wait_failure(dict(fixture='speech.wav'), 'size differs')
                path.unlink()


class DatabaseTests(unittest.TestCase):
    def fixture(self, root):
        index = root / 'indexer/journal.sqlite'
        index.parent.mkdir()
        with sqlite3.connect(index) as db:
            db.executescript('CREATE TABLE files(path TEXT); '
                             'CREATE VIRTUAL TABLE chunks USING fts5(content);')
        secondary = root / 'imports/health-dedupe.sqlite'
        secondary.parent.mkdir()
        with sqlite3.connect(secondary) as db:
            db.execute('CREATE TABLE health_dedupe(id TEXT)')
        return index, secondary

    def test_mixed_databases_all_integrity_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            self.fixture(root)
            rows = verify_snapshot_databases(root)
            self.assertEqual([r['path'] for r in rows],
                             ['imports/health-dedupe.sqlite', 'indexer/journal.sqlite'])
            self.assertEqual(rows[0], dict(path='imports/health-dedupe.sqlite', integrity='ok'))
            self.assertEqual(rows[1]['raw_transcript_matches'], 0)

    def test_required_index_and_every_secondary_integrity(self):
        for fault in ('missing', 'schema', 'corrupt_secondary', 'raw_path', 'raw_phrase'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                index, secondary = self.fixture(root)
                if fault == 'missing':
                    index.unlink()
                    self.assertFalse(index.exists())
                    error, message = ValueError, 'missing canonical'
                elif fault == 'corrupt_secondary':
                    secondary.write_bytes(b'not SQLite')
                    self.assertEqual(secondary.read_bytes(), b'not SQLite')
                    error, message = sqlite3.DatabaseError, 'not a database'
                else:
                    with sqlite3.connect(index) as db:
                        if fault == 'schema':
                            db.execute('DROP TABLE files')
                            self.assertEqual(db.execute('PRAGMA integrity_check').fetchall(), [('ok',)])
                            error, message = sqlite3.OperationalError, 'no such table: files'
                        elif fault == 'raw_path':
                            db.execute('INSERT INTO files VALUES (?)', ('chronicle/input/audio.jsonl',))
                            self.assertEqual(db.execute('SELECT COUNT(*) FROM files').fetchone()[0], 1)
                            error, message = ValueError, 'raw audio search exclusion failed'
                        else:
                            db.execute('INSERT INTO chunks VALUES (?)', ('the quick brown fox',))
                            self.assertEqual(db.execute('SELECT COUNT(*) FROM chunks').fetchone()[0], 1)
                            error, message = ValueError, 'raw audio search exclusion failed'
                with self.assertRaisesRegex(error, message):
                    verify_snapshot_databases(root)


if __name__ == '__main__':
    unittest.main()
