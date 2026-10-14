#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Build a standalone development phone check from an explicit callable archive."""
import argparse,datetime,hashlib,json,pathlib,subprocess,sys
root=pathlib.Path(__file__).parent
p=argparse.ArgumentParser();p.add_argument('--native-work',type=pathlib.Path,required=True);p.add_argument('--device',required=True);p.add_argument('--team',required=True);p.add_argument('--keychain',type=pathlib.Path);p.add_argument('--signing-project',type=pathlib.Path);a=p.parse_args()
if (root/'Native/librestic_bridge.h').exists():raise ValueError('local syntax header must not shadow generated native header')
native=a.native_work.resolve();archive=native/'artifacts/librestic_bridge.a';header=native/'artifacts/librestic_bridge.h'
receipt=json.loads((native/'receipts/backup-bridge-build.json').read_text())
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
if not (receipt.get('status')=='passed' and receipt.get('complete') is True and receipt.get('exit')==0 and receipt.get('target')=='ios'):raise ValueError('complete passed iOS archive receipt required')
if receipt.get('archive_sha256')!=sha(archive) or receipt.get('header_sha256')!=sha(header):raise ValueError('archive/header identity does not match receipt')
def run(argv,log=None):
 print(argv,flush=True)
 if log:
  with log.open('wb') as f:r=subprocess.run(argv,cwd=root,stdout=f,stderr=subprocess.STDOUT)
  if r.returncode:print('\n'.join(log.read_text(errors='replace').splitlines()[-60:]));raise SystemExit(r.returncode)
 else:subprocess.run(argv,cwd=root,check=True)
if a.signing_project:run(['make','-C',str(a.signing_project.resolve()),'unlock-signing'])
run(['xcodegen','generate'])
run(['xcodebuild','-project','BackupProbe.xcodeproj','-scheme','BackupProbe','-configuration','Debug','-destination','generic/platform=iOS','-derivedDataPath',str(root/'build'),'-allowProvisioningUpdates',f'BACKUP_ARCHIVE={archive}',f'BACKUP_HEADERS={archive.parent}',f'DEVELOPMENT_TEAM={a.team}',*([f'OTHER_CODE_SIGN_FLAGS=--keychain {a.keychain.resolve()}'] if a.keychain else []),'build'],root/'build.log')
app=root/'build/Build/Products/Debug-iphoneos/BackupProbe.app'
run(['codesign','--verify','--deep','--strict',str(app)])
manifest=[{'path':str(f.relative_to(app)),'sha256':sha(f),'bytes':f.stat().st_size} for f in sorted(app.rglob('*')) if f.is_file()]
source=[{'path':str(f.relative_to(root)),'sha256':sha(f)} for folder in ['Host','Native','Shared'] for f in sorted((root/folder).rglob('*')) if f.is_file()]
(root/'app-manifest.json').write_text(json.dumps({'time':datetime.datetime.now(datetime.timezone.utc).isoformat(),'files':manifest,'source':source,'native_receipt_sha256':sha(native/'receipts/backup-bridge-build.json'),'boundary':'Standalone local callable qualification app. No production backup admission or remote qualification.'},indent=2)+'\n')
run(['xcrun','devicectl','device','install','app','--device',a.device,str(app)])
print('BUILD_SIGN_INSTALL_PASS',flush=True)
