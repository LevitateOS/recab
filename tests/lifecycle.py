"""Real CLI operation-catalog integration on finalization.py's disposable disks.
Boot identities are observed fixtures, not a VM/PID1 boot claim.
"""
import json,os,subprocess
from pathlib import Path
def observe(h,env,label):
 s=h['cli'](env,'status');h['check'](s['schema_version']==10 and s['operations'],'missing lifecycle contract')
 h['check'](s['active_slot']!=s['inactive_slot'],'directions collapsed')
 if os.environ.get('RECAB_LIFECYCLE_SNAPSHOTS'):
  out=Path(os.environ['RECAB_LIFECYCLE_SNAPSHOTS']);out.mkdir(parents=True,exist_ok=True);(out/(label+'.json')).write_text(json.dumps(s))
 return s
def execute(h,env,id,values=None,operation=None,code=0):
 s=observe(h,env,'before-'+id.replace(':','-'));op=operation or next(o for o in s['operations'] if o['id']==id)
 h['check'](operation is not None or op['verdict']=='ready',f'backend refused {id}: {op.get("reason")}')
 argv=list(op['argv'])
 for field in op['inputs']:
  value=(values or {}).get(field['name'])
  if value:
   if field['flag']:argv.append(field['flag'])
   argv.append(value)
 q=subprocess.run([h['BIN'],'--json','--machine-events',*argv],env=env,text=True,capture_output=True,timeout=120)
 h['check'](q.returncode==code,f'catalog {id}: {q.returncode}: {q.stderr}')
 events=[]
 for line in q.stderr.splitlines():
  try:
   item=json.loads(line)
   if 'event' in item:events.append(item)
  except ValueError:pass
 h['check'](events and events[0]['event']=='started' and events[-1]['event']==('finished' if code==0 else 'failed'),'missing operation boundary events')
 if code==0:return json.loads(q.stdout) if q.stdout.strip() else None
 h['check']('stale' in q.stderr,'stale request was not diagnosed')
def exercise(h,active,destination,final):
 check,run=h['check'],h['run'];roots,dev,mounts=h['roots'],h['dev'],h['mounts'];generations={active['RECAB_ROOT']:None};evidence=[]
 def switch(slot,g):
  for key in ['A','B']:
   if key+'/var' in mounts:run('umount',str(roots[key]/'var'));mounts.remove(key+'/var')
   if key in mounts:run('umount',str(roots[key]));mounts.remove(key)
  run('mount','-o','ro',dev[slot],str(roots[slot]));mounts.add(slot);run('mount','--bind',str(roots['state']),str(roots[slot]/'var'));mounts.add(slot+'/var')
  prefix='levitateos' if h['args'].release_builder else 'lev'
  h['variable']('OneShot',None);h['variable']('Selected',f'{prefix}-{slot.lower()}.conf');return h['running'](slot,g)
 for direction in range(2):
  s=observe(h,active,f'ready-{direction}');source=s['active_slot'];target=s['inactive_slot'];check(target==destination,'wrong prepared system')
  before=h['cli'](active,'inventory','--root',str(roots[source]));old=next(o for o in s['operations'] if o['id']=='set_next');old_finalize=next(o for o in s['operations'] if o['id']=='finalize')
  change=h['base']/f'change from {source}.txt';change.write_text(f'prepared X and Y from {source}\n')
  execute(h,active,'edit',{'path':'etc/config','file':str(change)})
  opened=observe(h,active,f'open-{direction}');check(opened['lifecycle']['state']=='open','edit did not reopen candidate')
  check(opened['preparation']['id']==s['preparation']['id'],'another change reseeded preparation')
  execute(h,active,'set_next',operation=old,code=21);execute(h,active,'finalize',operation=old_finalize,code=21)
  final=execute(h,active,'finalize');ready=observe(h,active,f'finalized-{direction}');check(ready['lifecycle']['state']=='ready','candidate not ready')
  check(h['cli'](active,'inventory','--root',str(roots[source]))==before,'catalog mutation changed running system')
  execute(h,active,'set_next');pending=observe(h,active,f'pending-{direction}');check(pending['lifecycle']['state']=='restart_set' and pending['lifecycle']['reboot_needed'],'trial not exposed')
  active=switch(target,final['generation']);trial=observe(h,active,f'on-trial-{direction}');check(trial['lifecycle']['state']=='on_trial' and trial['lifecycle']['other']=='previous','trial or protected fallback role not exposed after observed boot')
  execute(h,active,'commit');kept=observe(h,active,f'kept-{direction}');check(kept['active_slot']==target and kept['committed'],'commit targeted wrong direction')
  check((roots[target]/'etc/config').read_text()==change.read_text(),'prepared change lost during activation')
  evidence.append(f'{source}->{target}: catalog edit/finalize/trial/commit, stale revision refusal, active root preserved')
  if direction==0:
   execute(h,active,'prepare');opened=observe(h,active,'reverse-prepared');check(opened['lifecycle']['state']=='open','reverse preparation missing');final=execute(h,active,'finalize');destination=source
 return {'result':'PASS','checks':evidence,'scope':'real GPT/ext4/vfat and CLI; observed boot records; no VM/full build'}
