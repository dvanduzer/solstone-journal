#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Build the isolated operator harness using SDK tools, no product Gradle tree."""
import argparse, hashlib, json, os, pathlib, shutil, subprocess, time, zipfile
p=argparse.ArgumentParser(); p.add_argument('--root',required=True); p.add_argument('--sdk',required=True); p.add_argument('--java',required=True); args=p.parse_args()
root=pathlib.Path(args.root); sdk=pathlib.Path(args.sdk); java=pathlib.Path(args.java); build=root/'build'; build.mkdir(exist_ok=True)
tools=sdk/'build-tools/35.0.0'; jar=sdk/'platforms/android-36/android.jar'; commands=[]
def run(argv):
  start=time.time(); out=subprocess.run([str(a) for a in argv],stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
  commands.append({'argv':[str(a) for a in argv],'exit':out.returncode,'elapsed_seconds':time.time()-start,'output':out.stdout.decode(errors='replace')})
  (build/'build-receipts.json').write_text(json.dumps(commands,indent=2))
  if out.returncode: raise RuntimeError(commands[-1])
classes=build/'classes'; classes.mkdir(exist_ok=True); dex=build/'dex'; dex.mkdir(exist_ok=True)
run([java/'bin/javac','-source','8','-target','8','-cp',jar,'-d',classes,*sorted((root/'src').rglob('*.java'))])
env=os.environ.copy(); env['JAVA_HOME']=str(java); env['PATH']=str(java/'bin')+':'+env.get('PATH',''); os.environ.update(env)
run([tools/'d8','--lib',jar,'--min-api','31','--output',dex,*sorted(classes.rglob('*.class'))])
unsigned=build/'unsigned.apk'
run([tools/'aapt2','link','-I',jar,'--manifest',root/'AndroidManifest.xml','--min-sdk-version','31','--target-sdk-version','36','--version-code','1','--version-name','experiment-1','-o',unsigned])
with zipfile.ZipFile(unsigned,'a',compression=zipfile.ZIP_DEFLATED) as z:
  z.write(dex/'classes.dex','classes.dex')
  for native in sorted((root/'lib').rglob('*.so')): z.write(native,str(native.relative_to(root)))
  for asset in sorted((root/'assets').rglob('*')):
    if asset.is_file(): z.write(asset,str(asset.relative_to(root)))
aligned=build/'aligned.apk'; run([tools/'zipalign','-P','16','-f','4',unsigned,aligned])
key=build/'debug-keystore.p12'
if not key.exists(): run([java/'bin/keytool','-genkeypair','-keystore',key,'-storepass','android','-keypass','android','-alias','androiddebugkey','-keyalg','RSA','-keysize','2048','-validity','365','-dname','CN=Isolated Native Experiment'])
signed=build/'journal-process-runtime.apk'
run([tools/'apksigner','sign','--ks',key,'--ks-pass','pass:android','--key-pass','pass:android','--ks-key-alias','androiddebugkey','--out',signed,aligned])
run([tools/'apksigner','verify','--verbose','--print-certs',signed]); run([tools/'zipalign','-c','-P','16','-v','4',signed])
members=[]
with zipfile.ZipFile(signed) as z:
  for i in z.infolist(): members.append({'name':i.filename,'bytes':i.file_size,'compressed_bytes':i.compress_size,'sha256':hashlib.sha256(z.read(i)).hexdigest()})
(build/'artifact-manifest.json').write_text(json.dumps({'apk':str(signed),'apk_sha256':hashlib.sha256(signed.read_bytes()).hexdigest(),'members':members},indent=2))
print(json.dumps({'apk':str(signed),'apk_sha256':hashlib.sha256(signed.read_bytes()).hexdigest(),'commands':len(commands)}))
