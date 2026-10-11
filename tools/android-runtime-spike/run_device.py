#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Bench lock must already be held. Launch the app; wait for its completion event."""
import argparse,hashlib,json,os,pathlib,subprocess,threading,time
p=argparse.ArgumentParser();p.add_argument('scenario');p.add_argument('--install',action='store_true');a=p.parse_args()
run_id=str(time.time_ns())
root=pathlib.Path(__file__).resolve().parent
adb=[str(pathlib.Path(os.environ['ANDROID_HOME'])/'platform-tools/adb'),'-s','RZGL11XCS9D']
pkg='app.solstone.experiment.runtime'; out=root/'evidence'/f'{a.scenario}-{int(time.time())}';out.mkdir(parents=True)
receipts=[]
def run(name,args,binary=False):
 t=time.time();r=subprocess.run(adb+args,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 (out/(name+('.bin' if binary else '.txt'))).write_bytes(r.stdout);(out/(name+'.stderr')).write_bytes(r.stderr)
 receipts.append({'name':name,'argv':adb+args,'exit':r.returncode,'seconds':time.time()-t,'stdout_sha256':hashlib.sha256(r.stdout).hexdigest()})
 (out/'commands.json').write_text(json.dumps(receipts,indent=2))
 if r.returncode:raise RuntimeError((name,r.returncode,r.stderr.decode(errors='replace')))
 return r.stdout
run('device',['shell','getprop','ro.build.fingerprint'])
run('capture-before',['shell','dumpsys','package','app.solstone.observer.phone'])
if a.install:run('install',['install','-r',str(root/'build/journal-process-runtime.apk')])
path=run('package-path',['shell','pm','path',pkg]).decode().strip().removeprefix('package:')
installed=run('installed-apk',['exec-out','cat',path],True)
expected=hashlib.sha256((root/'build/journal-process-runtime.apk').read_bytes()).hexdigest()
assert hashlib.sha256(installed).hexdigest()==expected
(out/'installed-apk.bin').unlink()
done=threading.Event(); report=[]
log=subprocess.Popen(adb+['logcat','-T','1','-v','brief','RuntimeSpike:I','*:S'],stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True)
def drain():
 with (out/'app-log.txt').open('w') as f:
  for line in log.stdout:
   f.write(line);f.flush()
   if 'SPIKE_DONE:events-'+a.scenario+'-'+run_id+'.jsonl' in line:
    report.append(line.split('SPIKE_DONE:',1)[1].strip());done.set()
thread=threading.Thread(target=drain,daemon=True);thread.start()
started=time.time()
try:
 run('stop-experiment',['shell','am','force-stop',pkg])
 run('wake',['shell','input','keyevent','KEYCODE_WAKEUP'])
 run('dismiss-keyguard',['shell','wm','dismiss-keyguard'])
 run('launch',['shell','am','start','-W','-n',pkg+'/.MainActivity','--es','scenario',a.scenario,'--es','run_id',run_id])
 assert done.wait(600),'app completion timeout'
 data=run('events',['exec-out','run-as',pkg,'cat','files/'+report[-1]])
 events=[json.loads(line) for line in data.splitlines()]
 terminal=[e for e in events if e['event'] in ['complete','failed','host_killed_during_work','host_intentional_kill','host_finish']][-1]
 expected_terminal={'loss':'host_killed_during_work','exit':'host_intentional_kill','finish':'host_finish'}.get(a.scenario,'complete')
 assert terminal['event']==expected_terminal,terminal
 if a.scenario in ['loss','exit','finish']:
  time.sleep(5)
  run('worker-evidence',['exec-out','run-as',pkg,'cat','files/worker-evidence.jsonl'])
finally:
 log.terminate();log.wait(timeout=10);thread.join(timeout=10)
run('activity',['shell','dumpsys','activity','activities'])
run('screenshot',['exec-out','screencap','-p'],True)
run('processes',['shell','ps','-A','-o','USER,PID,PPID,NAME'])
run('capture-after',['shell','dumpsys','package','app.solstone.observer.phone'])
receipt={'scenario':a.scenario,'seconds':time.time()-started,'apk_sha256':expected,'event_file':report[-1],'events':len(events),'last':terminal,'source_commit':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip()}
(out/'receipt.json').write_text(json.dumps(receipt,indent=2));print(json.dumps({'out':str(out),**receipt}))
