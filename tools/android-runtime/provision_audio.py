#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Provision pinned native models into a separate development application's files."""
import argparse
import hashlib
import json
import pathlib
import re
import subprocess

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--adb',default='adb');p.add_argument('--serial',required=True)
p.add_argument('--package',default='app.solstone.development.journal.audio')
p.add_argument('--parakeet',type=pathlib.Path,required=True)
a=p.parse_args()
if a.package != 'app.solstone.development.journal.audio': p.error('audio development application required')
repo=pathlib.Path(__file__).resolve().parents[2]
catalog=(repo/'core/crates/solstone-core-assets/src/lib.rs').read_text()
block=next(b for b in catalog.split('Artifact {') if 'unit: "parakeet-model"' in b)
field=lambda key:re.search(rf'{key}: "([^"]+)"',block)[1]
if a.parakeet.name!=field('filename'):p.error('maintained model filename required')
with a.parakeet.open('rb') as f:
 if hashlib.file_digest(f,'sha256').hexdigest()!=field('sha256'):p.error('model differs from maintained catalog pin')
models=[a.parakeet,*[repo/'core/models/assets'/name for name in ['silero_vad_v6.onnx','wespeaker-resnet34-256.onnx','pyannote-segmentation-3.0.onnx']]]
adb=[a.adb,'-s',a.serial]
def run(*argv):return subprocess.check_output([*adb,*argv],stderr=subprocess.STDOUT,timeout=300).decode()
run('shell','am','force-stop',a.package)
run('shell','run-as',a.package,'mkdir','-p','files/audio-models')
staging='/data/local/tmp/journal-audio-model-provision-'+a.package
run('shell','mkdir','-p',staging)
receipts=[]
try:
 for model in models:
  sha=hashlib.file_digest(model.open('rb'),'sha256').hexdigest()
  target='files/audio-models/'+model.name
  run('push',str(model),staging+'/'+model.name)
  run('shell','run-as',a.package,'cp',staging+'/'+model.name,target)
  actual=run('shell','run-as',a.package,'sha256sum',target).split()[0]
  if actual!=sha:raise RuntimeError('model transfer digest mismatch: '+model.name)
  receipts.append(dict(name=model.name,sha256=sha,bytes=model.stat().st_size))
finally:
 for model in models:run('shell','rm','-f',staging+'/'+model.name)
 run('shell','rmdir',staging)
print(json.dumps(dict(package=a.package,serial=a.serial,models=receipts)))
