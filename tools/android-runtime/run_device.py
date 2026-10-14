#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Bench lock must already be held. Launch the app; wait for its completion event."""
import argparse,hashlib,json,os,pathlib,subprocess,threading,time
p=argparse.ArgumentParser();p.add_argument('scenario');p.add_argument('--install',action='store_true');p.add_argument('--signer-sha256');p.add_argument('--package',default='app.solstone.development.journal');p.add_argument('--serial',default=os.environ.get('ANDROID_SERIAL','RZGL11XCS9D'));a=p.parse_args()
if a.package not in ('app.solstone.development.journal','app.solstone.development.journal.audio'):p.error('development application required')
run_id=str(time.time_ns())
root=pathlib.Path(__file__).resolve().parent
adb=[str(pathlib.Path(os.environ['ANDROID_HOME'])/'platform-tools/adb'),'-s',a.serial]
pkg=a.package; out=root/'evidence'/f'{a.scenario}-{int(time.time())}';out.mkdir(parents=True)
receipts=[]
def run(name,args,binary=False):
 t=time.time();r=subprocess.run(adb+args,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=120)
 (out/(name+('.bin' if binary else '.txt'))).write_bytes(r.stdout);(out/(name+'.stderr')).write_bytes(r.stderr)
 receipts.append({'name':name,'argv':adb+args,'exit':r.returncode,'seconds':time.time()-t,'stdout_sha256':hashlib.sha256(r.stdout).hexdigest()})
 (out/'commands.json').write_text(json.dumps(receipts,indent=2))
 if r.returncode:raise RuntimeError((name,r.returncode,r.stderr.decode(errors='replace')))
 return r.stdout
run('device',['shell','getprop','ro.build.fingerprint'])
run('capture-before',['shell','dumpsys','package','app.solstone.observer.phone'])
if a.install:
 if not a.signer_sha256:p.error('--signer-sha256 is required with --install')
 apk=root/'build/journal-process-runtime.apk'
 subprocess.run([os.sys.executable,str(root/'install_apk.py'),'--apk',str(apk),'--apk-sha256',hashlib.sha256(apk.read_bytes()).hexdigest(),'--signer-sha256',a.signer_sha256,'--application-id',pkg,'--sdk',os.environ['ANDROID_HOME'],'--adb',adb[0],'--serial',a.serial,'--receipt',str(out/'install.json')],check=True,stdin=subprocess.DEVNULL)
path=run('package-path',['shell','pm','path',pkg]).decode().strip().removeprefix('package:')
installed=run('installed-apk',['exec-out','cat',path],True)
expected=hashlib.sha256((root/'build/journal-process-runtime.apk').read_bytes()).hexdigest()
assert hashlib.sha256(installed).hexdigest()==expected
(out/'installed-apk.bin').unlink()
done=threading.Event(); report=[]
log=subprocess.Popen(adb+['logcat','-T','1','-v','brief','JournalRuntime:I','*:S'],stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True)
def drain():
 with (out/'app-log.txt').open('w') as f:
  for line in log.stdout:
   f.write(line);f.flush()
   if 'RUNTIME_DONE:events-'+a.scenario+'-'+run_id+'.jsonl' in line:
    report.append(line.split('RUNTIME_DONE:',1)[1].strip());done.set()
thread=threading.Thread(target=drain,daemon=True);thread.start()
started=time.time()
try:
 run('stop-experiment',['shell','am','force-stop',pkg])
 run('wake',['shell','input','keyevent','KEYCODE_WAKEUP'])
 run('dismiss-keyguard',['shell','wm','dismiss-keyguard'])
 run('launch',['shell','am','start','-W','-n',pkg+'/app.solstone.development.journal.MainActivity','--es','scenario',a.scenario,'--es','run_id',run_id])
 assert done.wait(2400 if a.scenario=="mixed" else 600),'app completion timeout'
 data=run('events',['exec-out','run-as',pkg,'cat','files/'+report[-1]])
 events=[json.loads(line) for line in data.splitlines()]
 terminal=[e for e in events if e['event'] in ['complete','failed','host_killed_during_work','host_intentional_kill','host_finish']][-1]
 expected_terminal={'loss':'host_killed_during_work','exit':'host_intentional_kill','finish':'host_finish'}.get(a.scenario,'complete')
 assert terminal['event']==expected_terminal,terminal
 if a.scenario in ['full','mixed']:assert terminal['data']['passed'] is True and terminal['data']['pending']==0,terminal
 if a.scenario in ['loss','exit','finish']:
  time.sleep(5)
  worker=[json.loads(line) for line in run('worker-evidence',['exec-out','run-as',pkg,'cat','files/worker-evidence.jsonl']).splitlines()]
  if a.scenario=='loss':
   pid=next(e['data']['pid'] for e in events if e['event']=='identity')
   assert any(e['event']=='reply_connection_lost' and e['pid']==pid and e['elapsed_ms']>=terminal['elapsed_ms'] for e in worker),'missing lost-reply evidence for this worker'
finally:
 log.terminate();log.wait(timeout=10);thread.join(timeout=10)
run('activity',['shell','dumpsys','activity','activities'])
run('screenshot',['exec-out','screencap','-p'],True)
run('processes',['shell','ps','-A','-o','USER,PID,PPID,NAME'])
run('capture-after',['shell','dumpsys','package','app.solstone.observer.phone'])
receipt={'scenario':a.scenario,'seconds':time.time()-started,'apk_sha256':expected,'event_file':report[-1],'events':len(events),'last':terminal,'source_commit':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip()}
(out/'receipt.json').write_text(json.dumps(receipt,indent=2));print(json.dumps({'out':str(out),**receipt}))
