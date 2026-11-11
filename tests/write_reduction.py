"""P13 real Recipe/Ginux + GPT/ext4/vfat write and fidelity integration.
The very same probe sources drive ab-update in a VM. No mocked producer,
filesystem, boot selection, or counters; only boot identities are observations.
"""
import errno,shutil,fcntl,hashlib,json,os,signal,stat,struct,subprocess,threading,time
from pathlib import Path
from changes import Changes
RANGE=1024*1024
LARGE=32*RANGE
PROBE='usr/share/recab-p13'
def seed(root):
 p=root/'etc/p13-local';p.mkdir();a=p/'hard-a';a.write_bytes(b'local hardlinks and metadata\n');os.link(a,p/'hard-b');a.chmod(0o640);os.chown(a,1001,1002);os.setxattr(a,'user.p13',b'local metadata')
 # Genuine Linux extended ACL, including an explicit named user and mask.
 acl=struct.pack('<I',2)+b''.join(struct.pack('<HHI',tag,perm,uid) for tag,perm,uid in [(1,6,0xffffffff),(2,4,1003),(4,4,0xffffffff),(16,4,0xffffffff),(32,0,0xffffffff)])
 os.setxattr(a,'system.posix_acl_access',acl)
 (p/'symlink').symlink_to('hard-a')
 os.link(p/'symlink',p/'symlink-linked',follow_symlinks=False)
 cap=p/'capability';cap.write_bytes(b'capability metadata payload');os.setxattr(cap,'security.capability',struct.pack('<IIIII',0x02000001,1<<10,0,0,0))
 sparse=p/'sparse'
 with sparse.open('wb') as f:f.write(b'head');f.seek(16*RANGE-4);f.write(b'tail')
 raw=os.fsencode(p)+b'/raw-\xff';fd=os.open(raw,os.O_WRONLY|os.O_CREAT,0o600);os.write(fd,b'non-UTF8 system name\n');os.close(fd)

def exercise(h,active):
 check,run,cli=h['check'],h['run'],h['cli'];roots,dev,mounts=h['roots'],h['dev'],h['mounts']
 check(h['args'].probe_recipes is not None,'--write-reduction needs --probe-recipes from ab-update-fixture recipes')
 recipes=h['args'].probe_recipes.resolve(strict=True);changes=Changes(h)
 active.update({k:v for k,v in changes.env.items() if k.startswith(('GINUX_','RECIPE_','RECAB_RECIPE_')) or k=='TMPDIR'})
 evidence=[];reports=[]
 def status():return cli(active,'status')
 def sectors(slot):
  run('sync');return int((Path('/sys/class/block')/Path(dev[slot]).name/'stat').read_text().split()[6])
 def fidelity(root):
  p=root/'etc/p13-local';a=(p/'hard-a').stat();b=(p/'hard-b').stat()
  check(a.st_ino==b.st_ino and a.st_nlink==2,'hardlink group was split')
  check((a.st_uid,a.st_gid,stat.S_IMODE(a.st_mode))==(1001,1002,0o640),'numeric owner/mode changed')
  check(os.getxattr(p/'hard-a','user.p13')==b'local metadata' and os.getxattr(p/'hard-a','system.posix_acl_access')==struct.pack('<I',2)+b''.join(struct.pack('<HHI',tag,perm,uid) for tag,perm,uid in [(1,6,0xffffffff),(2,4,1003),(4,4,0xffffffff),(16,4,0xffffffff),(32,0,0xffffffff)]),'xattr/ACL changed')
  check(os.readlink(p/'symlink')=='hard-a','symlink was resolved or changed')
  check((p/'symlink').lstat().st_ino==(p/'symlink-linked').lstat().st_ino,'hardlinked symlink was split')
  check(os.getxattr(p/'capability','security.capability')==struct.pack('<IIIII',0x02000001,1<<10,0,0,0),'capability metadata changed')
  check((p/'sparse').stat().st_size==16*RANGE and (p/'sparse').stat().st_blocks*512<64*1024,'sparse file expanded')
  with open(os.fsencode(p)+b'/raw-\xff','rb') as f:check(f.read()==b'non-UTF8 system name\n','raw path was lost')
 def inspect(slot,callback):return changes.candidate(slot,callback)
 def switch(slot,g):
  for s in ['A','B']:
   if s+'/var' in mounts:run('umount',str(roots[s]/'var'));mounts.remove(s+'/var')
   if s in mounts:run('umount',str(roots[s]));mounts.remove(s)
  run('mount','-o','ro',dev[slot],str(roots[slot]));mounts.add(slot);run('mount','--bind',str(roots['state']),str(roots[slot]/'var'));mounts.add(slot+'/var')
  h['variable']('OneShot',None);h['variable']('Selected',f'lev-{slot.lower()}.conf');return dict(h['running'](slot,g),**{k:v for k,v in active.items() if k.startswith(('GINUX_','RECIPE_','RECAB_RECIPE_')) or k=='TMPDIR'})
 fs_ids={s:run('blkid','-c','/dev/null','-s','UUID','-o','value',dev[s]).stdout for s in ['A','B']}
 before=cli(active,'inventory','--root',str(roots['A']));writes=sectors('A')
 shared=roots['state']/'p13-shared-user-data';shared.write_bytes(b'per-user shared data is not rolled back\n')
 user_store=roots['state']/'independent-user-profile';user_recipe=recipes/'user-profile.rhai'
 ledger_before=(h['state']/'state.json').read_bytes()
 user=run(changes.recipe,'--json','--package-root',str(user_store),'--build-dir',str(h['base']/'user-build'),'--state-dir',str(h['base']/'user-build-state'),'install',str(user_recipe),env=active,cwd=h['base'])
 check((user_store/'profile/share/recab-p13-user.txt').read_text()=='independent user profile\n','independent Recipe profile was not activated')
 check((h['state']/'state.json').read_bytes()==ledger_before,'independent user package changed system state')


 # Stop after durable invalidation intent, before any candidate write.
 audit=h['state']/'audit.log';saved=audit.read_bytes() if audit.exists() else b''
 audit.unlink(missing_ok=True);os.mkfifo(audit)
 child=subprocess.Popen([h['BIN'],'--json','prepare'],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
 try:
  deadline=time.monotonic()+30
  while time.monotonic()<deadline:
   value=json.loads((h['state']/'state.json').read_text())
   if (value.get('preparation') or {}).get('phase')=='intent':
    os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10);break
   if child.poll() is not None:raise RuntimeError('prepare ended before invalidation boundary: '+str(child.communicate()))
   time.sleep(.001)
  else:raise RuntimeError('durable invalidation boundary not reached')
 finally:
  if child.poll() is None:os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10)
  audit.unlink();audit.write_bytes(saved)
 pending=status()['preparation'];check(pending['phase']=='intent','invalidation intent disappeared')
 cli(active,'set-next','B',code=21)
 opened=cli(active,'prepare','--preparation',pending['id'],'--revision',str(pending['revision']));reports.append({'operation':'invalidation-kill-recovery','report':opened})
 check(not (h['esp']/'loader/entries/lev-b.conf').exists(),'recovered unfinalized candidate has an entry')
 x=cli(active,'change','install',str(recipes/'probe.rhai'));reports.append({'operation':'Recipe-X','report':x})
 check(x['producer_io'] is not None and x['invocation_io'] is not None,'X omitted Recipe/invocation IO')
 view=changes.command(active,'installed')
 check('recab-p13-probe' not in view['active']['packages'] and 'recab-p13-probe' in view['prepared']['packages'],'prepared package became active in the running system')
 changes.command(active,'isinstalled','recab-p13-probe','--prepared')
 inspect('B',fidelity)
 def metadata(root,revision):
  file=root/'usr/share/recab-p13-metadata/file';md=file.stat()
  expected=(1003,1004,0o644) if revision==2 else (1001,1002,0o640)
  check((md.st_uid,md.st_gid,stat.S_IMODE(md.st_mode))==expected,'metadata-only owner/mode update lost')
  check(file.read_bytes()==b'unchanged metadata probe\n','metadata-only update changed file bytes')
  names=['user.recab-p13','system.posix_acl_access','security.capability']
  if revision==3:check(all(n not in os.listxattr(file) for n in names),'intentional xattr/ACL/capability removal lost')
  else:
   named=1005 if revision==2 else 1003
   acl=struct.pack('<I',2)+b''.join(struct.pack('<HHI',tag,perm,uid) for tag,perm,uid in [(1,6,0xffffffff),(2,4,named),(4,4,0xffffffff),(16,4,0xffffffff),(32,4 if revision==2 else 0,0xffffffff)])
   check(os.getxattr(file,names[0])==bytes([revision]) and os.getxattr(file,names[1])==acl,'metadata-only xattr/ACL update lost')
   cap=struct.pack('<IIIII',0x02000001,1<<(13 if revision==2 else 10),0,0,0)
   check(os.getxattr(file,names[2])==cap,'metadata-only capability update lost')
  return md.st_ino
 def meta_change(revision,slot):
  args=['install',str(recipes/'metadata-1.rhai')] if revision==1 else ['update','recab-p13-metadata','--producer-recipe',str(recipes/f'metadata-{revision}.rhai')]
  result=cli(active,'change',*args);reports.append({'operation':f'metadata-{revision}','report':result})
  check(result['report']['payload_bytes_written']<32768,'metadata-only update copied unrelated payloads')
  return inspect(slot,lambda root:metadata(root,revision))
 # Actual shared-state capacity loss during a producer must leave fallback safe.
 pressure=roots['state']/'p13-cache-pressure';space=os.statvfs(roots['state']);block=space.f_frsize
 try:
  with pressure.open('wb') as f:
   allocated=0
   for unit in [8*RANGE,block]:
    while True:
     try:os.posix_fallocate(f.fileno(),allocated,unit);allocated+=unit
     except OSError as e:
      if e.errno!=errno.ENOSPC:raise
      break
  q=subprocess.run([h['BIN'],'--json','change','install',str(recipes/'metadata-1.rhai')],env=active,text=True,capture_output=True,timeout=120)
  check(q.returncode in [21,22],'full producer cache did not fail safely: '+q.stderr)
  check(not (h['esp']/'loader/entries/lev-b.conf').exists(),'cache exhaustion exposed an incomplete candidate')
 finally:pressure.unlink(missing_ok=True)
 check(cli(active,'inventory','--root',str(roots['A']))==before and sectors('A')==writes,'cache exhaustion wrote the running root')
 failed=status();recovery=next((o for o in failed['operations'] if o['id']=='change_recover' and o['verdict']=='ready'),None)
 if recovery:cli(active,*recovery['argv'])
 else:cli(active,'prepare')
 inode=meta_change(1,'B');check(meta_change(2,'B')==inode,'metadata-only update replaced its data inode')
 # Exercise the exact reproduction producer through Ginux with a hashed input.
 bound=cli(active,'change','update','recab-p13-metadata','--producer-recipe',str(recipes/'metadata-release.rhai'),'--','--define','RECAB_RELEASE_BASE='+h['kernel_hash'],'--input','release='+str(h['KERNEL']))
 reports.append({'operation':'release-context-metadata-producer','report':bound})
 check(inspect('B',lambda root:metadata(root,2))==inode,'release-context metadata producer changed its data inode')
 failure=cli(active,'change','install',str(recipes/'failure.rhai'));reports.append({'operation':'prepared-failing-service','report':failure})
 service='etc/init.d/recab-p13-failure' if h['args'].init=='openrc' else 'etc/systemd/system/recab-p13-failure.service'
 inspect('B',lambda root:check((root/service).exists(),'failure service was not prepared'))
 repair=cli(active,'change','remove','recab-p13-failure');reports.append({'operation':'repair-prepared-failure','report':repair})
 inspect('B',lambda root:check(not (root/service).exists(),'failure service removal lost its ownership'))
 # A distinct, valid fixture initramfs forces an actual new ESP payload write.
 # The gzip timestamp changes its object digest without changing boot contents.
 updated_initrd=h['base']/'candidate-initrd';initrd=bytearray(h['initrd'].read_bytes());initrd[4:8]=(1).to_bytes(4,'little');updated_initrd.write_bytes(initrd)
 def finalize_after_flush_kill(label,boot_args=()):
  check(shutil.which('strace') is not None,'--write-reduction needs strace for the actual flush interruption')
  trace=h['base']/(label+'-flush.trace')
  child=subprocess.Popen(['strace','-qq','-f','-e','trace=syncfs','-e','inject=syncfs:signal=SIGSTOP:when=1','-o',str(trace),h['BIN'],'--json','finalize',*boot_args],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
  stopped=False
  try:
   deadline=time.monotonic()+45
   while time.monotonic()<deadline:
    if trace.exists() and 'SIGSTOP' in trace.read_text():
     value=status();prep=value['preparation']
     check(prep['phase']=='finalizing' and prep['finalization'] is not None,'flush stop did not reach captured generation')
     os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10);stopped=True;break
    if child.poll() is not None:raise RuntimeError('finalize ended before flush interruption: '+str(child.communicate()))
    time.sleep(.001)
   check(stopped,'real syncfs boundary not observed')
  finally:
   if child.poll() is None:os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10)
  pending=status()['preparation'];slot=pending['slot']
  check(not (h['esp']/f'loader/entries/lev-{slot.lower()}.conf').exists(),'flush interruption left a selectable candidate')
  cli(active,'set-next',slot,code=21)
  # Real read-only binds fail artifact metadata and entry publication after
  # preflight. The FIFO holds the actual writer at durable intent, not a timer.
  for boundary in (['recab/objects'] if boot_args else [])+['recab/generations','loader/entries']:
   audit=h['state']/'audit.log';old=audit.read_bytes();audit.unlink();os.mkfifo(audit)
   child=subprocess.Popen([h['BIN'],'--json','finalize'],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True);reader=None;rows=[];drainer=None;drained=threading.Event()
   try:
    deadline=time.monotonic()+30
    while time.monotonic()<deadline:
     if (status().get('preparation') or {}).get('phase')=='finalizing':break
     if child.poll() is not None:raise RuntimeError('finalization ended before durable intent: '+str(child.communicate()))
     time.sleep(.001)
    else:raise RuntimeError('finalization boundary not reached')
    point=str(h['esp']/boundary)
    run('nsenter','-t',str(child.pid),'-m','mount','--bind',point,point)
    run('nsenter','-t',str(child.pid),'-m','mount','-o','remount,bind,ro',point)
    reader=os.open(audit,os.O_RDONLY|os.O_NONBLOCK)
    def drain():
     while not drained.is_set():
      try:chunk=os.read(reader,65536)
      except BlockingIOError:chunk=b''
      if chunk:rows.append(chunk)
      else:time.sleep(.001)
    drainer=threading.Thread(target=drain);drainer.start()
    out,err=child.communicate(timeout=120)
    check(child.returncode==22 and 'Read-only file system' in err,'publication fault did not exercise real EROFS: '+out+err)
   finally:
    if child.poll() is None:os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10)
    drained.set()
    if drainer is not None:drainer.join(timeout=10)
    if reader is not None:
     while True:
      try:chunk=os.read(reader,65536)
      except BlockingIOError:break
      if not chunk:break
      rows.append(chunk)
     os.close(reader)
    audit.unlink();audit.write_bytes(old+b''.join(rows))
   pending=status()['preparation']
   check(pending['phase']=='interrupted' and not (h['esp']/f'loader/entries/lev-{slot.lower()}.conf').exists(),'failed publication exposed a candidate')
   cli(active,'set-next',slot,code=21)
   reports.append({'operation':label+'-'+boundary+'-fault','report':pending})
  result=cli(active,'finalize','--preparation',pending['id'],'--revision',str(pending['revision']))
  check(not result['accounting_complete'],'flush recovery invented complete accounting')
  reports.append({'operation':label+'-flush-kill-recovery','report':result});return result
 first=finalize_after_flush_kill('A-to-B',['--kernel',str(h['KERNEL']),'--initrd',str(updated_initrd)]);cli(active,'set-next','B')
 check(cli(active,'inventory','--root',str(roots['A']))==before and sectors('A')==writes,'X wrote active A')
 active=switch('B',first['generation']);cli(active,'commit');fidelity(roots['B']);metadata(roots['B'],2)
 check((roots['B']/PROBE/'large').stat().st_size==LARGE and (roots['B']/PROBE/'X').read_text()=='X\n','X was not activated')
 before=cli(active,'inventory','--root',str(roots['B']));writes=sectors('B')
 # Fill only excluded, hidden /var storage on the inactive scratch root.
 # This injects actual ENOSPC preflight without changing its system inventory.
 run('blockdev','--setrw',dev['A']);run('mount','-o','rw',dev['A'],str(roots['A']))
 try:
  space=os.statvfs(roots['A']);fill=max(0,space.f_bavail*space.f_frsize-8*RANGE)
  run('fallocate','-l',str(fill),str(roots['A']/'var/p13-space-pressure'))
 finally:run('umount',str(roots['A']));run('blockdev','--setro',dev['A'])
 old_state=(h['state']/'state.json').read_bytes();entry=h['esp']/'loader/entries/lev-a.conf';old_entry=entry.read_bytes()
 cli(active,'prepare',code=21,contains='free bytes')
 check((h['state']/'state.json').read_bytes()==old_state and entry.read_bytes()==old_entry,'space refusal invalidated the old generation')
 run('blockdev','--setrw',dev['A']);run('mount','-o','rw',dev['A'],str(roots['A']));(roots['A']/'var/p13-space-pressure').unlink();run('umount',str(roots['A']));run('blockdev','--setro',dev['A'])
 # Exhaust actual inactive-root inodes without modifying its system tree.
 run('blockdev','--setrw',dev['A']);run('mount','-o','rw',dev['A'],str(roots['A']))
 pressure=roots['A']/'var/p13-inode-pressure';pressure.mkdir()
 try:
  for n in range(os.statvfs(roots['A']).f_favail):
   try:(pressure/str(n)).touch()
   except OSError as e:
    if e.errno!=errno.ENOSPC:raise
    break
  check(os.statvfs(roots['A']).f_favail==0,'inode pressure did not exhaust actual inodes')
 finally:run('umount',str(roots['A']));run('blockdev','--setro',dev['A'])
 cli(active,'prepare',code=21,contains='inodes')
 check((h['state']/'state.json').read_bytes()==old_state and entry.read_bytes()==old_entry,'inode refusal invalidated old rollback')
 run('blockdev','--setrw',dev['A']);run('mount','-o','rw',dev['A'],str(roots['A']));shutil.rmtree(pressure);run('umount',str(roots['A']));run('blockdev','--setro',dev['A'])
 # Kill after a real payload write, then recover the same captured plan.
 child=subprocess.Popen([h['BIN'],'--json','prepare'],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
 try:
  deadline=time.monotonic()+45;interrupted=False
  while time.monotonic()<deadline:
   plans=list(h['state'].glob('synchronize-*.json'))
   if plans and child.poll() is None:
    ident=plans[0].stem.removeprefix('synchronize-');visible=Path(f'/proc/{child.pid}/root')/str(h['base']/'staging'/f'prepare-{ident}'/PROBE/'large').lstrip('/')
    try:
     with visible.open('rb') as f:changed=f.read(1)==b'a'
    except FileNotFoundError:changed=False
    if changed:
     os.kill(child.pid,signal.SIGSTOP);os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10);interrupted=True;break
   if child.poll() is not None:raise RuntimeError('copy finished before write interruption: '+str(child.communicate()))
   time.sleep(.001)
  check(interrupted,'actual range write was not observed')
 finally:
  if child.poll() is None:os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10)
 pending=status()['preparation'];check(pending['slot']=='A' and not entry.exists(),'interrupted candidate remained bootable')
 cli(active,'set-next','A',code=21,contains='incomplete')
 prepared=cli(active,'prepare','--preparation',pending['id'],'--revision',str(pending['revision']));reports.append({'operation':'B->A-prepare-after-real-write-kill','report':prepared['synchronization']})
 check(prepared['synchronization']['resumed'] and not prepared['synchronization']['accounting_complete'],'resume invented complete accounting')
 y_sectors=sectors('A')
 y=cli(active,'change','update','recab-p13-probe','--producer-recipe',str(recipes/'probe-Y.rhai'));reports.append({'operation':'Recipe-Y','report':y})
 check(0<y['report']['payload_bytes_written']<=2*RANGE,'Y rewrote a complete 32 MiB file: '+json.dumps(y))
 check(y['producer_io'] is not None and y['invocation_io'] is not None,'Y omitted staging/export IO')
 y_after=sectors('A');delta=(y_after-y_sectors)*512;check(0<delta<LARGE,'virtual root block writes did not avoid the full large-file rewrite');reports.append({'operation':'Y-root-block-IO','report':{'before_sectors':y_sectors,'after_sectors':y_after,'sector_bytes':512,'written_bytes':delta}})
 meta_change(3,'A')
 # Insufficient real FAT capacity must refuse before candidate publication.
 saved_state=(h['state']/'state.json').read_bytes();pressure=h['esp']/'p13-space-pressure'
 free=shutil.disk_usage(h['esp']).free
 try:
  try:
   with pressure.open('wb') as f:
    f.seek(free-1);f.write(b'\0');f.flush();os.fsync(f.fileno())
  except OSError as e:
   if e.errno!=errno.ENOSPC:raise
  cli(active,'finalize',code=21,contains='ESP has insufficient space')
  check((h['state']/'state.json').read_bytes()==saved_state and not entry.exists(),'ESP refusal changed candidate eligibility')
 finally:pressure.unlink(missing_ok=True)
 def y_contents(root):
  fidelity(root);metadata(root,3);check((root/PROBE/'Y').read_text()=='Y\n' and not (root/PROBE/'delete-me').exists(),'Y/deletion lost')
  with (root/PROBE/'large').open('rb') as f:check(f.read(16)==b'Y'+b'a'*15,'large-file edit lost')
 inspect('A',y_contents)
 final=finalize_after_flush_kill('B-to-A');check(final['boot_bytes_written']==0 and final['reused_boot_files']==2,'unchanged ESP payload recopied');cli(active,'set-next','A')
 check(cli(active,'inventory','--root',str(roots['B']))==before and sectors('B')==writes,'Y wrote active B')
 active=switch('A',final['generation']);cli(active,'commit');y_contents(roots['A'])
 # Now B already has the large file, and only one aligned range differs.
 repeated=cli(active,'prepare');reports.append({'operation':'A->B-repeat','report':repeated['synchronization']})
 check(repeated['synchronization']['payload_bytes_written']<=4*RANGE,'repeat copied a whole large file/root')
 inspect('B',y_contents)
 same=cli(active,'prepare');check(same['id']==repeated['id'] and same['revision']==repeated['revision'],'reopen reseeded work')
 # The actual recab flock serializes a second process; status remains readable.
 with open(h['env']['RECAB_LOCK_PATH'],'a') as held:
  fcntl.flock(held,fcntl.LOCK_EX);queued=subprocess.Popen([h['BIN'],'--json','prepare'],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
  try:
   time.sleep(.1);check(queued.poll() is None,'concurrent writer bypassed the lock')
   busy=status();check(busy['lifecycle']['state']=='working' and all(o['verdict']=='refused' for o in busy['operations']),'concurrent status did not refuse mutations')
  finally:fcntl.flock(held,fcntl.LOCK_UN)
  out,err=queued.communicate(timeout=30);check(queued.returncode==0,'serialized prepare failed: '+err);check(json.loads(out)['id']==same['id'],'serialized writer reseeded work')

 cli(active,'discard','--preparation',same['id'],'--revision','0',code=21,contains='stale')
 cli(active,'discard','--preparation',same['id'],'--revision',str(same['revision']));discarded=status();check(discarded['rollback_slot'] is None,'discard resurrected old rollback')
 zero=cli(active,'prepare');reports.append({'operation':'unchanged-after-discard','report':zero['synchronization']});check(zero['synchronization']['payload_bytes_written']==0,'identical continuation wrote payloads')
 final=cli(active,'finalize');cli(active,'set-next','B');active=switch('B',final['generation']);cli(active,'commit');y_contents(roots['B'])
 for s in ['A','B']:check(run('blkid','-c','/dev/null','-s','UUID','-o','value',dev[s]).stdout==fs_ids[s],'slot was reformatted')
 check(shared.read_bytes()==b'per-user shared data is not rolled back\n','shared user data changed')
 check((user_store/'profile/share/recab-p13-user.txt').read_text()=='independent user profile\n','continuation/rollback changed the independent Recipe profile')
 inputs={'runtime_reference':changes.env['GINUX_ROOT'],'architecture':os.uname().machine,'init':h['args'].init,'kernel_sha256':h['kernel_hash'],'initramfs_sha256':h['initrd_hash']}
 for recipe in sorted(recipes.glob('*.rhai')):inputs[recipe.name+'_sha256']=hashlib.sha256(recipe.read_bytes()).hexdigest()
 for name,path in [('recab',h['BIN']),('recipe',changes.recipe),('worker',h['base']/'recipe-worker'),('ginux',os.environ['GINUX_BIN']),('probe',recipes/'probe.rhai'),('Y_probe',recipes/'probe-Y.rhai')]:
  with open(path,'rb') as f:inputs[name+'_sha256']=hashlib.file_digest(f,'sha256').hexdigest()
 return {'result':'PASS','inputs':inputs,'checks':['real 32 MiB Recipe/Ginux publication','one-range Y update','bounded reverse synchronization','zero unchanged continuation','A/B root inventory and block-write protection','Recipe producer, projection, invocation and ESP accounting','mode/owner/ACL/xattr/capability, hardlinked files/symlinks, sparse and non-UTF8 fidelity','deletion and explicit discard semantics','filesystem UUID preservation','root bytes/inodes and ESP capacity refusals preserve eligibility','metadata-only owner/mode/ACL/capability updates and removals','release-context-bound metadata producer with hashed input','durable invalidation kill and recovery','actual flush interruption/recovery in both directions','new boot payload, artifact metadata and entry publication faults/recovery','producer cache exhaustion and recovery','kill after actual write and captured-plan recovery','concurrent actual flock/stale-session refusal','prepared failure service and ownership-based repair','shared user data preservation','independent Recipe profile isolation and preservation','active/prepared Recipe selection separation'],'reports':reports,'scope':'native real filesystems and producers; observed boot identities; no VM or physical SSD SMART claim'}
