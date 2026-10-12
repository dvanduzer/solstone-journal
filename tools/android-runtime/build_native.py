#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Build the selected Journal library and JNI shim on the Android build host."""
import hashlib,json,os,pathlib,subprocess,sys,time
from provenance import source_manifest
root=pathlib.Path(__file__).resolve().parent
repo=root.parents[1]
ndk=pathlib.Path(os.environ.get('JOURNAL_NDK','/var/tmp/android-runtime-toolchain/android-ndk-r30'))
bins=ndk/'toolchains/llvm/prebuilt/linux-x86_64/bin'
env=os.environ.copy()
env.update(CARGO_TARGET_DIR=str(root/'target'),CARGO_BUILD_JOBS='2',CARGO_INCREMENTAL='0',CARGO_PROFILE_DEV_DEBUG='0',CC_aarch64_linux_android=str(bins/'aarch64-linux-android31-clang'),AR_aarch64_linux_android=str(bins/'llvm-ar'),CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=str(bins/'aarch64-linux-android31-clang'),CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS='-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384')
out=root/'evidence';out.mkdir(exist_ok=True)
(out/'source-manifest.json').write_text(json.dumps(source_manifest(repo),indent=2)+'\n')
steps=[]
def run(name,argv):
 start=time.time();log=out/(name+'.log')
 with log.open('wb') as f:p=subprocess.run(list(map(str,argv)),cwd=repo,env=env,stdin=subprocess.DEVNULL,stdout=f,stderr=subprocess.STDOUT,timeout=1800)
 row={'name':name,'argv':list(map(str,argv)),'exit':p.returncode,'seconds':time.time()-start,'log_sha256':hashlib.sha256(log.read_bytes()).hexdigest()}
 steps.append(row);(out/'native-build.json').write_text(json.dumps({'source':subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip(),'environment':{k:v for k,v in env.items() if k.startswith(('CARGO_','CC_','AR_'))},'steps':steps},indent=2))
 print(json.dumps(row),flush=True)
 if p.returncode:print(log.read_text(errors='replace')[-12000:]);sys.exit(p.returncode)
run('cargo-build',['cargo','build','--locked','--offline','--manifest-path',root/'native/Cargo.toml','--target','aarch64-linux-android'])
lib=root/'lib/arm64-v8a';lib.mkdir(parents=True,exist_ok=True)
import shutil
shutil.copy2(root/'target/aarch64-linux-android/debug/libandroid_journal_runtime.so',lib/'libandroid_journal_runtime.so')
run('jni-build',[bins/'aarch64-linux-android31-clang','-shared','-fPIC','-Wl,-z,max-page-size=16384','-Wl,-z,common-page-size=16384',root/'bridge.c','-L'+str(lib),'-landroid_journal_runtime','-o',lib/'libjournalbridge.so'])
