#!/usr/bin/env python3
"""Focused CLI integration, explicitly invoked; never part of a unit suite.

Uses two real ext4 partitions and actual recab filesystem mutations. EFI variables
and the observed boot-generation records are scratch fixtures: P04 boot validation
is outside this check. Only the verified temporary loop image is formatted.
Requirements: Python 3, sfdisk, losetup, blkid, mkfs.ext4, mount, umount, fallocate.
"""
import argparse, os, sys, json, hashlib, copy, subprocess, tempfile, shutil, time, signal
from pathlib import Path
parser = argparse.ArgumentParser(description='Run recab P03 against temporary GPT/ext4 loop partitions; no system build or boot.')
parser.add_argument('--binary', type=Path, required=True)
args = parser.parse_args()
BIN = str(args.binary.resolve(strict=True))
if os.geteuid() != 0:
    parser.error('root with mount/loop-device access is required')
GUID='4a67b082-0a4c-41cf-b6c7-440b29bb8c4f'
def run(*args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, **kwargs)
def check(ok, message):
    if not ok: raise RuntimeError(message)
def sha(b): return hashlib.sha256(b).hexdigest()
def packed(obj): return json.dumps(obj,separators=(',',':'),ensure_ascii=False).encode()
def cli(env,*args,code=0,contains=None):
    p=subprocess.run([BIN,'--json',*args],env=env,text=True,capture_output=True,timeout=90)
    check(p.returncode==code,f'{args}: expected {code}, got {p.returncode}: {p.stderr}')
    if contains: check(contains in p.stderr,f'{args}: missing {contains}: {p.stderr}')
    return json.loads(p.stdout) if p.stdout.strip() else None

def inv_digest(inventory):
    canonical=copy.deepcopy(inventory)
    for n in canonical['entries']:
        p=bytes(n['path'])
        if p in (b'etc/fstab',b'usr/lib/recab/immutable',b'usr/lib/recab'): n['modified']=n['modified_ns']=0
        n['extents']=None
        if n['normalized'] is not None: n['content']=n['normalized'];n['normalized']=None
        if p in (b'etc/fstab',b'usr/lib/recab/immutable'): n['size']=0
    return sha(packed(canonical))

def fill(path,byte,count):
    with open(path,'wb') as f:
        for _ in range(count): f.write(bytes([byte])*1024*1024)

base=Path(tempfile.mkdtemp(prefix='recab-p03-ext4-'))
loop=None; child=None; mounts=set(); success=False; interrupted_payload=False
try:
    disk=base/'scratch.img'
    with open(disk,'wb') as f:f.truncate(544*1024*1024)
    run('sfdisk',str(disk),input=b'label: gpt\nsize=256M,type=L,name=RECAB_TEST_A\nsize=256M,type=L,name=RECAB_TEST_B\n')
    loop=run('losetup','--find','--show','--partscan',str(disk)).stdout.decode().strip()
    check(loop.startswith('/dev/loop'),'unexpected loop path')
    backing=run('losetup','--noheadings','--output','BACK-FILE',loop).stdout.decode().strip()
    check(Path(backing).resolve()==disk.resolve(),'loop backing file does not match scratch image')
    devices={s:loop+'p'+str(i) for i,s in enumerate(['A','B'],1)}
    uuids={s:run('blkid','-s','PARTUUID','-o','value',d).stdout.decode().strip() for s,d in devices.items()}
    roots={s:base/s for s in ['A','B']}
    for s in roots:
        roots[s].mkdir();run('mkfs.ext4','-q','-F',devices[s]);run('mount','-t','ext4',devices[s],str(roots[s]));mounts.add(s)
    a,b=roots['A'],roots['B']
    for root,slot in [(a,'A'),(b,'B')]:
        for d in ['etc','usr/lib/recab','usr/bin','var/cache','home','root','boot','run','tmp','proc','sys','dev']:(root/d).mkdir(parents=True,exist_ok=True)
        (root/'usr/lib/os-release').write_text('ID=lev\nNAME=LevitateOS\nREC_INIT=systemd\nREC_FLAVOUR=base\n')
        (root/'etc/os-release').symlink_to('/usr/lib/os-release')
        (root/'etc/fstab').write_text(f'PARTUUID={uuids[slot]} / ext4 ro 0 0\nPARTUUID=cccc-3 /var ext4 defaults 0 0\n')
        (root/'usr/lib/recab/immutable').write_text(f'{slot} PARTUUID=cccc-3\n')
        (root/'etc/config').write_bytes(b'config=old\n')
        (root/'usr/meta').write_bytes(b'only metadata changes\n');os.setxattr(root/'usr/meta',b'user.obsolete',b'remove this')
        fill(root/'usr/ranges',0x61,3)
        with open(root/'usr/ranges','ab') as f:f.write(b'TAIL'*33)
        fill(root/'usr/zz-interrupt',0x44 if slot=='A' else 0x55,16)
        (root/'var/cache/excluded').write_bytes(f'private {slot}'.encode())
        (root/'home/user').mkdir();(root/'home/user/keep').write_bytes(f'home {slot}'.encode())
    # A's desired metadata and links. B intentionally starts with differences.
    os.chmod(a/'usr/meta',0o640);os.chown(a/'usr/meta',1001,1002)
    os.setxattr(a/'usr/meta',b'user.recab',b'attribute only');os.removexattr(a/'usr/meta',b'user.obsolete')
    (a/'usr/h0').write_bytes(b'hardlinked content');os.link(a/'usr/h0',a/'usr/h1')
    (b/'usr/h0').write_bytes(b'old 0');(b/'usr/h1').write_bytes(b'old 1')
    (a/'usr/typefile').write_bytes(b'file replaces directory');(b/'usr/typefile').mkdir();(b/'usr/typefile/old').write_bytes(b'old child')
    (a/'usr/typedir').mkdir();(a/'usr/typedir/leaf').write_bytes(b'directory replaces file');(b/'usr/typedir').write_bytes(b'old file')
    (a/'usr/typelink').symlink_to('/usr/bin/tool');(b/'usr/typelink').write_bytes(b'old file')
    (b/'usr/obsolete').mkdir();(b/'usr/obsolete/old').write_bytes(b'delete this')
    raw=b'/raw-\xff';os.close(os.open(os.fsencode(a/'usr')+raw,os.O_WRONLY|os.O_CREAT,0o600))
    (a/'usr/aliased').write_bytes(b'new system file')
    (b/'usr/aliased').write_bytes(b'cached publication must survive');os.link(b/'usr/aliased',b/'var/cache/alias')
    (a/'usr/sparse').touch()
    with open(a/'usr/sparse','r+b') as f:f.write(b'A'*4096);f.seek(10*1024*1024);f.write(b'B'*4096);f.truncate(12*1024*1024)
    fill(b/'usr/sparse',0x70,12)
    esp=base/'esp';state=base/'state';proc=base/'proc';efi=base/'efi'
    for d in [esp/'loader/entries',esp/'lev-a',esp/'lev-b',state,proc/'self',efi/'efivars']:d.mkdir(parents=True,exist_ok=True)
    kernel=b'P03 matching boot artifact bytes\n'
    for s in ['a','b']:(esp/f'lev-{s}/vmlinuz').write_bytes(kernel)
    env=dict(os.environ,RECAB_BOOT_ROOT=str(esp),RECAB_STATE_DIR=str(state),RECAB_PROC_PATH=str(proc),RECAB_SYS_FIRMWARE_EFI_PATH=str(efi),RECAB_EFIVARS_PATH=str(efi/'efivars'),RECAB_LOCK_PATH=str(base/'lock'),RECAB_STAGE_DIR=str(base/'staging'))
    def generation(slot):
        inventory=cli(dict(env,RECAB_ROOT=str(roots[slot])),'inventory','--root',str(roots[slot]))
        d=inv_digest(inventory);g={'id':sha(packed([d,sha(kernel),None])),'inventory_digest':d,'kernel_digest':sha(kernel),'initrd_digest':None}
        (roots[slot]/'usr/lib/recab/generation.json').write_bytes(packed({'version':1,'generation':g['id'],'slot':slot,'partuuid':uuids[slot]}))
        return g
    import struct
    # ACLs, capabilities, and hardlinked symlinks are real filesystem metadata.
    acl=struct.pack('<I',2)+b''.join(struct.pack('<HHI',tag,perm,uid) for tag,perm,uid in [(1,6,0xffffffff),(2,4,1005),(4,0,0xffffffff),(16,4,0xffffffff),(32,0,0xffffffff)])
    os.setxattr(a/'usr/meta',b'system.posix_acl_access',acl)
    for root in [a,b]: (root/'usr/cap').write_bytes(b'capability payload')
    os.setxattr(a/'usr/cap',b'security.capability',struct.pack('<IIIII',0x02000001,1<<10,0,0,0))
    for root in [a,b]:
        (root/'usr/sym0').symlink_to('/usr/bin/tool'); os.link(root/'usr/sym0',root/'usr/sym1',follow_symlinks=False)
        (root/'usr/cache-link').symlink_to('/cache-link')
    os.link(b/'usr/cache-link',b/'var/cache/link-alias',follow_symlinks=False)
    os.chown(a/'usr/cache-link',1004,1004,follow_symlinks=False)
    (a/'usr/mountpoint').mkdir()
    ga=generation('A'); oldgb=generation('B')
    def entry(slot,gen):
        letter=slot.lower();(esp/f'loader/entries/lev-{letter}.conf').write_text(f'title lev {slot}\nlinux /lev-{letter}/vmlinuz\noptions root=PARTUUID={uuids[slot]} ro recab.generation={gen["id"]}\n')
    def record(active,gen,othergen=None):
        other='B' if active=='A' else 'A'
        ledger={'schema':{'major':2,'minor':0},'slots':[{'slot':s,'partuuid':uuids[s],'generation':gen if s==active else othergen,'readiness':'committed' if s==active else ('ready' if othergen else 'incomplete')} for s in ['A','B']], 'known_good_slot':active,'previous_good_slot':other if othergen else None}
        (state/'state.json').write_bytes(packed(ledger))
        entry(active,gen)
        if othergen:entry(other,othergen)
        (esp/'loader/loader.conf').write_text(f'default lev-{active.lower()}.conf\ntimeout 5\n')
        (efi/f'efivars/LoaderEntryDefault-{GUID}').write_bytes((7).to_bytes(4,'little')+f'lev-{active.lower()}.conf\0'.encode('utf-16le'))
        (proc/'cmdline').write_text(f'root=PARTUUID={uuids[active]} recab.generation={gen["id"]}\n')
        (proc/'self/mountinfo').write_text(Path('/proc/self/mountinfo').read_text())
        return dict(env,RECAB_ROOT=str(roots[active]))
    run('mount','-o','remount,ro',str(a));run('umount',str(b));mounts.remove('B')
    active=record('A',ga,oldgb)
    # Same-device bind mounts are rejected before inventory traversal.
    run('mount','--bind',str(a/'usr/typedir'),str(a/'usr/mountpoint'))
    try: cli(active,'inventory','--root',str(a),code=1,contains='unexpected mount')
    finally: run('umount',str(a/'usr/mountpoint'))
    # A special destination file is a refusal before state/entry invalidation.
    run('mount','-t','ext4',devices['B'],str(b));mounts.add('B');os.mkfifo(b/'usr/unsupported')
    run('umount',str(b));mounts.remove('B')
    original_ledger=(state/'state.json').read_bytes()
    cli(active,'prepare',code=21,contains='unsupported special')
    check((state/'state.json').read_bytes()==original_ledger and (esp/'loader/entries/lev-b.conf').exists(),'special-file refusal changed state')
    run('mount','-t','ext4',devices['B'],str(b));mounts.add('B');(b/'usr/unsupported').unlink();run('umount',str(b));mounts.remove('B')
    # Kill after the deterministic plan exists, while payload synchronization runs.
    child=subprocess.Popen([BIN,'--json','prepare'],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    deadline=time.monotonic()+45
    while time.monotonic()<deadline:
        plans=list(state.glob('synchronize-*.json'))
        if plans and child.poll() is None:
            preparation_id=plans[0].stem.removeprefix('synchronize-')
            # Read through the child's mount namespace, without keeping an FD
            # alive after killing it. Wait until an actual payload write occurred.
            visible=Path(f'/proc/{child.pid}/root')/str(base/'staging'/f'prepare-{preparation_id}'/'usr/zz-interrupt').lstrip('/')
            try:
                with open(visible,'rb') as f: changed=f.read(1)==b'D'
            except FileNotFoundError: changed=False
            if changed:
                os.kill(child.pid,signal.SIGSTOP);child.kill();child.communicate(timeout=10)
                interrupted_payload=True;break
        if child.poll() is not None:raise RuntimeError('synchronization finished before interruption: '+str(child.communicate()))
        time.sleep(.001)
    else:child.kill();raise RuntimeError('synchronization plan not observed')
    interrupted=cli(active,'status')['preparation'];check(interrupted['phase']=='awaiting_synchronization','durable incomplete state missing')
    check(not (esp/'loader/entries/lev-b.conf').exists(),'incomplete B remains published')
    first=cli(active,'prepare','--preparation',interrupted['id'],'--revision',str(interrupted['revision']))
    check(first['phase']=='prepared' and first['synchronized'],'resume did not complete')
    check(first['synchronization']['resumed'] and not first['synchronization']['accounting_complete'],'interrupted counters claimed completeness')
    check(json.loads((state/'state.json').read_text())['schema']=={'major':2,'minor':2},'minor safety format was not upgraded')
    cli(active,'set-next','B',code=21,contains='incomplete')
    run('mount','-t','ext4',devices['B'],str(b));mounts.add('B')
    check((b/'var/cache/alias').read_bytes()==b'cached publication must survive','excluded hardlink alias was patched')
    check((b/'usr/aliased').read_bytes()==b'new system file','aliased system file not synchronized')
    check((b/'usr/h0').stat().st_ino==(b/'usr/h1').stat().st_ino,'hardlink group lost')
    check(not (b/'usr/obsolete').exists() and (b/'usr/typefile').is_file() and (b/'usr/typedir/leaf').is_file(),'deletion/type changes failed')
    check((b/'usr/typelink').readlink()==Path('/usr/bin/tool'),'symlink changed')
    check((b/'usr/sparse').stat().st_blocks*512<1024*1024,'sparse holes were not preserved')
    check((b/'usr/meta').stat().st_uid==1001 and (b/'usr/meta').stat().st_gid==1002,'ownership lost')
    check(b'user.obsolete' not in [os.fsencode(x) for x in os.listxattr(b/'usr/meta')],'obsolete xattr retained')
    check((b/'home/user/keep').read_bytes()==b'home B' and (b/'var/cache/excluded').read_bytes()==b'private B','shared data copied')
    check(uuids['B'] in (b/'etc/fstab').read_text() and (b/'usr/lib/recab/immutable').read_text().startswith('B '),'B identity overwritten')
    check(os.getxattr(b/'usr/cap',b'security.capability')==os.getxattr(a/'usr/cap',b'security.capability'),'capability lost')
    check(os.getxattr(b/'usr/meta',b'system.posix_acl_access')==os.getxattr(a/'usr/meta',b'system.posix_acl_access'),'ACL lost')
    check((b/'usr/sym0').lstat().st_ino==(b/'usr/sym1').lstat().st_ino,'hardlinked symlinks lost')
    check((b/'usr/cache-link').lstat().st_uid==1004 and (b/'var/cache/link-alias').lstat().st_uid==0,'excluded symlink alias metadata was modified')
    old_afstab=(a/'etc/fstab').read_bytes()
    before=(a/'usr/ranges').read_bytes();(b/'etc/config').write_bytes(b'config=X\n')
    with open(b/'usr/ranges','r+b') as f:f.seek(1024*1024+17);f.write(b'X')
    os.chmod(b/'usr/meta',0o600);os.setxattr(b/'usr/meta',b'user.recab',b'changed without payload')
    reopened=cli(active,'prepare','--preparation',first['id'],'--revision',str(first['revision']))
    check(reopened['id']==first['id'] and (b/'etc/config').read_bytes()==b'config=X\n','reopening reseeded X')
    check((a/'usr/ranges').read_bytes()==before,'active A changed')
    # P04 finalization/boot is outside this check. Supply the next committed
    # generation record explicitly, while using actual ext4 roots for P03.
    gb=generation('B')
    run('mount','-o','remount,ro',str(b));run('umount',str(a));mounts.remove('A')
    reverse=record('B',gb,ga)
    expected_afstab=(b/'etc/fstab').read_bytes().replace(uuids['B'].encode(),uuids['A'].encode())
    back=cli(reverse,'prepare');r=back['synchronization']
    check(back['slot']=='A' and back['source_slot']=='B','reverse direction used wrong slot')
    check(r['payload_bytes_written']==1024*1024+len(b'config=X\n')+(len(expected_afstab) if expected_afstab!=old_afstab else 0),f'not bounded range writes: {r}')
    run('mount','-t','ext4',devices['A'],str(a));mounts.add('A')
    check((a/'etc/config').read_bytes()==b'config=X\n' and (a/'usr/ranges').read_bytes()==(b/'usr/ranges').read_bytes(),'reverse lost X or file edit')
    check((a/'usr/meta').stat().st_mode & 0o7777==0o600 and os.getxattr(a/'usr/meta',b'user.recab')==b'changed without payload','metadata-only edit lost')
    check(uuids['A'] in (a/'etc/fstab').read_text(),'A identity overwritten')
    run('mount','-o','remount,ro',str(a));run('umount',str(b));mounts.remove('B')
    identical=record('A',gb,gb)
    third=cli(identical,'prepare');r0=third['synchronization']
    check(r0['payload_bytes_written']==0 and r0['range_writes']==0 and r0['identity_bytes_written']==0,f'identical roots rewrote payloads: {r0}')
    # Predictable space refusal before boot invalidation, preserving the ledger.
    run('mount','-o','remount,rw',str(a));fill(a/'usr/growth',0x4e,8);gc=generation('A');run('mount','-o','remount,ro',str(a))
    run('mount','-t','ext4',devices['B'],str(b));mounts.add('B')
    available=os.statvfs(b).f_bavail*os.statvfs(b).f_frsize
    run('fallocate','-l',str(available-1024*1024),str(b/'var/filler'))
    run('umount',str(b));mounts.remove('B');insufficient=record('A',gc,gb)
    ledger_before=(state/'state.json').read_bytes();entry_before=(esp/'loader/entries/lev-b.conf').read_bytes()
    cli(insufficient,'prepare',code=21,contains='free bytes')
    check((state/'state.json').read_bytes()==ledger_before and (esp/'loader/entries/lev-b.conf').read_bytes()==entry_before,'space refusal invalidated rollback')
    success=True
    print(json.dumps({'result':'PASS','interrupted_after_payload_write':interrupted_payload,'first_resume':first['synchronization'],'reverse':r,'identical':r0,'checks':['real GPT/ext4 roots','kill and resume','both directions','one changed 1 MiB range','metadata/xattr/ownership/ACL/capabilities','sparse/hardlinks/symlinks/raw paths','excluded alias unchanged','deletions/type changes','reopen preserves X','slot identities','zero identical payload writes','space refusal preserves state and entry','same-device mount and special-file refusals','symlink hardlinks and excluded metadata aliases']},indent=2))
finally:
    if child is not None and child.poll() is None:
        child.kill();child.communicate(timeout=10)
    for slot in list(mounts):
        subprocess.run(['umount',str(base/slot)],check=False)
    if loop:subprocess.run(['losetup','-d',loop],check=False)
    if success:shutil.rmtree(base)
    else:print('Failure artifacts:',base,file=sys.stderr)
