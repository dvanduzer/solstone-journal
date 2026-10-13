#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Physical production audio ingest, local transcription and authenticated search."""
import argparse
import ast
import base64
import hashlib
import io
import json
import math
from pathlib import Path
import sqlite3
import subprocess
import struct
import time
import wave
import zipfile

from qualify_serving import Phone, Client, native, provenance

DAY = '20261012'
TEXT = 'The copper kite carries a silver lantern. The quiet river flows beside the garden.'


def fixtures(directory):
    directory.mkdir()
    aiff = directory / 'speech.aiff'
    subprocess.run(['say', '-v', 'Samantha', '-r', '145', '-o', str(aiff), TEXT], check=True)
    for name, options in [('speech.wav', ['-ar', '48000', '-ac', '2']),
                          ('speech.flac', ['-ar', '44100', '-ac', '1']),
                          ('speech.m4a', ['-c:a', 'aac', '-b:a', '64k', '-ar', '48000', '-ac', '2']),
                          ('fault.wav', ['-ar', '16000', '-ac', '1'])]:
        subprocess.run(['ffmpeg', '-nostdin', '-v', 'error', '-y', '-i', str(aiff),
                        *options, str(directory / name)], check=True)
    for name, seconds in [('silence.wav', 4), ('overbound.wav', 31)]:
        with wave.open(str(directory / name), 'wb') as wav:
            wav.setnchannels(1)
            wav.setsampwidth(2)
            wav.setframerate(16000)
            wav.writeframes(b'\0' * (seconds * 16000 * 2))
    (directory / 'malformed.wav').write_bytes(b'not an audio container')
    (directory / 'truncated.m4a').write_bytes((directory / 'speech.m4a').read_bytes()[:32])
    return {p.name: p for p in directory.iterdir() if p.suffix != '.aiff'}


def multipart(directory, segment, media):
    boundary = 'journal-audio-qualification-boundary'
    envelope = {'day': DAY, 'segment': segment, 'source': 'audio',
                'files': [{'submitted': media.name}]}
    payload = (f'--{boundary}\r\nContent-Disposition: form-data; name="envelope"\r\n\r\n'
               + json.dumps(envelope) + f'\r\n--{boundary}\r\nContent-Disposition: form-data; '
               + f'name="files"; filename="{media.name}"\r\n\r\n').encode()
    payload += media.read_bytes() + f'\r\n--{boundary}--\r\n'.encode()
    path = directory / (segment + '.multipart')
    path.write_bytes(payload)
    return {'method': 'POST', 'uri': '/app/devices/ingest', 'body_file': str(path),
            'content_type': 'multipart/form-data; boundary=' + boundary, 'protocol': '3'}


def audio_status(row):
    if row.get('event') != 'audio.status':
        return None
    return native(row['value']['reply'])['audio']


def wait_audio(phone, predicate, seconds=240):
    return audio_status(phone.wait(lambda row: audio_status(row) is not None
                                   and predicate(audio_status(row)), seconds=seconds))


def hits(result):
    return [hit for day in result.get('days', []) for hit in day['results']]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--host', required=True)
    parser.add_argument('--port', type=int, default=37657)
    parser.add_argument('--suite', required=True)
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cycles', type=int, default=18)
    parser.add_argument('--smoke', action='store_true')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    provenance(args)
    media = fixtures(args.output / 'fixtures')
    manifest = {name: {'sha256': hashlib.sha256(path.read_bytes()).hexdigest(),
                       'bytes': path.stat().st_size} for name, path in media.items()}
    (args.output / 'fixtures.json').write_text(json.dumps({'text': TEXT, 'files': manifest}, indent=2))
    phone, client = Phone(args), Client(args)
    submitted, processed, errors, responsiveness = {}, [], [], []
    started = time.time()
    sequence = 0

    def new_upload(name):
        nonlocal sequence
        sequence += 1
        segment = f'12{sequence // 60:02d}{sequence % 60:02d}_15'
        fields = multipart(args.output, segment, media[name])
        return segment, fields

    def upload(name, fault=None):
        segment, fields = new_upload(name)
        reply = client.call('loss' if fault else 'request', **fields, **({'loss': fault} if fault else {}))
        assert reply['ok'], reply
        if not fault:
            assert reply['value']['status'] == 200, reply
            assert json.loads(reply['value']['body'])['status'] == 'ok', reply
        submitted[segment] = name
        return segment, fields, reply

    def completed(segment):
        status = wait_audio(phone, lambda s: s.get('last', {}).get('input', '').endswith(
            '/' + segment + '/' + submitted[segment]) and s['last'].get('indexed'))
        processed.append(status['last'])
        result = client.http('GET', '/app/search/api/search?q=silver%20lantern&limit=100')
        assert any('/' + segment + '/' in hit['path'] for hit in hits(result)), result
        return status

    def ping():
        begin = time.monotonic()
        client.http('GET', '/app/network/api/identity')
        responsiveness.append((time.monotonic() - begin) * 1000)

    try:
        phone.launch(['--audio'])
        phone.owner('GET', '/init/api/state')
        phone.owner('GET', '/init/mark')
        phone.owner('POST', '/init/mark/lock', {})
        phone.owner('POST', '/init/finalize', {'name': 'Synthetic iOS audio', 'timezone': 'America/Denver'})
        link = phone.owner('POST', '/app/network/pair-start', {'device_label': 'ios-audio-laptop'})
        assert client.call('pair', link=link['pair_link'], label='ios-audio-laptop')['ok']
        wait_audio(phone, lambda s: s['state'] == 'ready')
        first, fields, _ = upload('speech.m4a')
        wait_audio(phone, lambda s: s['state'] == 'processing')
        ping()
        completed(first)
        duplicate = client.call('request', **fields)
        assert duplicate['ok'] and json.loads(duplicate['value']['body'])['status'] == 'duplicate', duplicate
        if not args.smoke:
            for auth in ('missing', 'invalid'):
                _, denied = new_upload('speech.flac')
                reply = client.call('request', auth=auth, **denied)
                assert not reply['ok'] or reply['value']['status'] in (401, 403), reply
                reply = client.call('request', auth=auth, method='GET', uri='/app/search/api/search?q=silver')
                assert not reply['ok'] or reply['value']['status'] in (401, 403), reply
            # Incomplete transport body cannot acquire an accepted custody receipt.
            segment, fields, _ = upload('fault.wav', 'before')
            phone.launch(['--audio'])
            retry = client.call('request', **fields)
            assert retry['ok'] and json.loads(retry['value']['body'])['status'] == 'ok', retry
            completed(segment)
            segment, fields, _ = upload('fault.wav', 'after')
            retry = client.call('request', **fields)
            assert retry['ok'] and json.loads(retry['value']['body'])['status'] == 'duplicate', retry
            completed(segment)
            # Kill the real foreground process while native processing owns accepted input.
            segment, _, _ = upload('speech.flac')
            wait_audio(phone, lambda s: s['state'] == 'processing' and '/' + segment + '/' in s['input'])
            phone.launch(['--audio'])
            completed(segment)
            # Cancel native processing before publication, then resume the retained input.
            segment, _, _ = upload('speech.wav')
            wait_audio(phone, lambda s: s['state'] == 'processing' and '/' + segment + '/' in s['input'])
            native(phone.control('audio-cancel'))
            wait_audio(phone, lambda s: s['state'] == 'cancelled')
            ping()
            native(phone.control('audio-resume'))
            completed(segment)
            for name in ('malformed.wav', 'truncated.m4a', 'overbound.wav'):
                segment, _, _ = upload(name)
                status = wait_audio(phone, lambda s: (s['state'] == 'failed' and '/' + segment + '/' in s.get('input', '')) or (s.get('last', {}).get('outcome') == 'Failed' and '/' + segment + '/' in s['last']['input']))
                errors.append(status)
                ping()
            segment, _, _ = upload('silence.wav')
            silent = wait_audio(phone, lambda s: s.get('last', {}).get('input', '').endswith('/' + segment + '/silence.wav'))
            processed.append(silent['last'])
            for mode in ('missing-model', 'corrupt-model'):
                phone.launch(['--audio', '--audio-mode', mode])
                status = wait_audio(phone, lambda s: s['state'] == 'failed')
                assert 'model' in status['error'].lower() or 'gguf' in status['error'].lower(), status
                errors.append(status)
                segment, _, _ = upload('speech.m4a')
                ping()
                phone.launch(['--audio'])
                completed(segment)
            for number in range(args.cycles):
                segment, _, _ = upload(('speech.m4a', 'speech.flac', 'speech.wav')[number % 3])
                wait_audio(phone, lambda s: s['state'] == 'processing' and '/' + segment + '/' in s['input'])
                ping()
                completed(segment)
            assert client.call('connect')['ok']
            assert client.call('held', method='GET', uri='/app/network/api/identity')['ok']
            phone.owner('POST', '/app/network/unpair', {'device_label': 'ios-audio-laptop'})
            assert client.call('closed')['value']['closed']
            _, denied = new_upload('speech.m4a')
            for op, fields in [('request', denied), ('held', {'method': 'GET', 'uri': '/app/search/api/search?q=silver'})]:
                reply = client.call(op, **fields)
                assert not reply['ok'] or reply['value']['status'] in (401, 403), reply
        phone.control('serve-stop')
        inventory = native(phone.control('audio-inventory'))
        (args.output / 'inventory.json').write_text(json.dumps(inventory, indent=2))
        files = inventory['files']
        for segment, name in submitted.items():
            originals = [f for f in files if f['path'].endswith('/' + segment + '/' + name)]
            assert len(originals) == 1 and originals[0]['sha256'] == manifest[name]['sha256'], (segment, originals)
            events = [json.loads(line) for f in files if f['path'].endswith('/' + segment + '/events.jsonl')
                      for line in f['text'].splitlines() if line.strip()]
            assert any(row.get('record_type') == 'device_ingest' and row.get('outcome') == 'accepted' for row in events), events
        transcripts = []
        for file in files:
            if file['path'].endswith(('/speech.jsonl', '/fault.jsonl')):
                rows = [json.loads(line) for line in file['text'].splitlines() if line.strip()]
                header = rows[0]
                assert header['backend'] == 'parakeet-cpp' and header['compute_type'] == 'q8_0', header
                assert header['model'] == 'tdt-0.6b-v3-q8_0' and header['device'] == 'auto', header
                assert header['_solstone_processing']['state'] == 'analyzed', header
                assert header['raw'] in ('speech.wav', 'speech.flac', 'speech.m4a', 'fault.wav'), header
                assert 0 < header['duration'] <= 30, header
                segment = file['path'].split('/')[-2]
                base = sum(int(value) * factor for value, factor in zip((segment[0:2], segment[2:4], segment[4:6]), (3600, 60, 1)))
                starts = [sum(int(value) * factor for value, factor in zip(row['start'].split(':'), (3600, 60, 1))) - base for row in rows[1:]]
                assert starts == sorted(starts) and all(0 <= offset <= header['duration'] + 1 for offset in starts), starts
                text = ' '.join(row.get('text', '') for row in rows).lower()
                assert 'silver lantern' in text and 'copper kite' in text, (file['path'], rows)
                transcripts.append({'path': file['path'], 'rows': rows})
        assert len(transcripts) == len([p for p in processed if 'silence' not in p['input']]), transcripts
        phone.control('export')
        exported = next(row['value']['reply'] for row in reversed(phone.rows) if row['event'] == 'export')
        npzs = []
        for name, encoded in exported.items():
            if name.endswith('.npz'):
                data = base64.b64decode(encoded, validate=True)
                with zipfile.ZipFile(io.BytesIO(data)) as archive:
                    assert set(archive.namelist()) == {'embeddings.npy', 'encoder.npy', 'statement_ids.npy', 'durations_s.npy'}
                    arrays = {}
                    for entry in archive.namelist():
                        raw = archive.read(entry)
                        assert raw.startswith(b'\x93NUMPY\x01\x00'), entry
                        length = struct.unpack('<H', raw[8:10])[0]
                        header = ast.literal_eval(raw[10:10+length].decode())
                        assert not header['fortran_order']
                        arrays[entry] = (header, raw[10+length:])
                    shape = arrays['embeddings.npy'][0]['shape']
                    assert len(shape) == 2 and shape[0] > 0 and shape[1] == 256, shape
                    assert arrays['embeddings.npy'][0]['descr'] == '<f4'
                    values = struct.unpack('<' + 'f' * (shape[0] * 256), arrays['embeddings.npy'][1])
                    assert all(math.isfinite(value) for value in values)
                    for entry in ('statement_ids.npy', 'durations_s.npy'):
                        assert arrays[entry][0]['shape'] == (shape[0],)
                    ids = struct.unpack('<' + 'i' * shape[0], arrays['statement_ids.npy'][1])
                    assert len(set(ids)) == len(ids)
                    assert 'wespeaker-resnet34-256' == arrays['encoder.npy'][1].decode('utf-32-le')
                    durations = struct.unpack('<' + 'f' * shape[0], arrays['durations_s.npy'][1])
                    assert all(math.isfinite(value) and value > 0 for value in durations)
                    transcript = next(t for t in transcripts if t['path'] == name[:-4] + '.jsonl')
                    assert set(ids) <= {row['sentence_id'] for row in transcript['rows'][1:]}

                npzs.append({'path': name, 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()})
                out = args.output / 'sidecars' / name
                out.parent.mkdir(parents=True, exist_ok=True)
                out.write_bytes(data)
        assert npzs, 'no production embedding sidecar exported'
        dbpath = args.output / 'journal.sqlite'
        dbpath.write_bytes(base64.b64decode(exported['indexer/journal.sqlite'], validate=True))
        if 'indexer/journal.sqlite-wal' in exported:
            dbpath.with_name(dbpath.name + '-wal').write_bytes(base64.b64decode(exported['indexer/journal.sqlite-wal'], validate=True))
        with sqlite3.connect(dbpath.as_uri() + '?mode=ro', uri=True) as db:
            assert db.execute('pragma integrity_check').fetchall() == [('ok',)]
            chunks = db.execute('select path,content from chunks where chunks match ?', ('"silver lantern"',)).fetchall()
            assert {p for p, _ in chunks} == {t['path'] for t in transcripts}, chunks
        beats = [row for row in phone.rows if row['event'] == 'host.heartbeat']
        assert beats and all(row['value']['state'] == 0 for row in beats)
        result = {'passed': True, 'started': started, 'finished': time.time(), 'submitted': submitted,
                  'processed': processed, 'failures': errors, 'authenticated_identity_ms': responsiveness,
                  'transcripts': transcripts, 'npz': npzs,
                  'peak_footprint_bytes': max(row['value']['resources']['footprint_bytes'] for row in beats),
                  'thermal_states': sorted({row['value']['thermal'] for row in beats}),
                  'foreground_heartbeats': len(beats)}
        (args.output / 'result.json').write_text(json.dumps(result, indent=2))
        phone.control('serve-finish')
        print(json.dumps({k: v for k, v in result.items() if k not in ('transcripts', 'submitted')}))
    finally:
        client.process.stdin.close()
        client.process.wait(timeout=35)
        for process in phone.processes:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)


if __name__ == '__main__':
    main()
