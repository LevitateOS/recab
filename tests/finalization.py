#!/usr/bin/env python3
"""Real ext4/vfat CLI integration for sealing/publication, not a kernel boot.
Uses an existing UEFI kernel/module, native ELF programs and temporary GPT disks.
The initramfs and observed firmware/boot records are structural fixtures; only
an actual VM boot can establish PID 1, hardware-driver and service behavior.
"""
import argparse,copy,errno,gzip,hashlib,json,os,re,shutil,signal,struct,subprocess,tempfile,time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--binary',required=True,type=Path);p.add_argument('--kernel',required=True,type=Path);p.add_argument('--module',required=True,type=Path);p.add_argument('--init',choices=['systemd','openrc'],default='systemd');p.add_argument('--stage-recstrap',type=Path,help='also exercise destructive staging through the shared finalizer');args=p.parse_args()
if os.geteuid()!=0:p.error('root with mount/loop access is required')
BIN=str(args.binary.resolve(strict=True));KERNEL=args.kernel.resolve(strict=True);MODULE=args.module.resolve(strict=True)
GUID='4a67b082-0a4c-41cf-b6c7-440b29bb8c4f'
def check(ok,message):
    if not ok:raise RuntimeError(message)
def run(*argv,**kw):return subprocess.run(argv,check=True,capture_output=True,**kw)
def cli(env,*argv,code=0,contains=None):
    q=subprocess.run([BIN,'--json',*argv],env=env,text=True,capture_output=True,timeout=120)
    check(q.returncode==code,f'{argv}: exit {q.returncode}, expected {code}: {q.stderr}')
    if contains:check(contains in q.stderr,f'missing {contains}: {q.stderr}')
    return json.loads(q.stdout) if q.stdout.strip() else None
def packed(o):return json.dumps(o,separators=(',',':')).encode()
def sha(b):return hashlib.sha256(b).hexdigest()
def digest(inv):
    inv=copy.deepcopy(inv)
    for n in inv['entries']:
        path=bytes(n['path']);n['extents']=None
        if path in (b'etc/fstab',b'usr/lib/recab/immutable',b'usr/lib/recab'):n['modified']=n['modified_ns']=0
        if n['normalized'] is not None:n['content']=n['normalized'];n['normalized']=None
        if path in (b'etc/fstab',b'usr/lib/recab/immutable'):n['size']=0
    return sha(packed(inv))
def cpio(entries):
    out=bytearray()
    for ino,(name,mode,data) in enumerate([*entries,('TRAILER!!!',0,b'')],1):
        name=name.encode()+b'\0';fields=[ino,mode,0,0,1,0,len(data),0,0,0,0,len(name),0]
        out+=b'070701'+b''.join(f'{v:08x}'.encode() for v in fields)+name
        out+=b'\0'*((-len(out))%4);out+=data;out+=b'\0'*((-len(out))%4)
    return gzip.compress(out,mtime=0)
def install_elf(root,source,target):
    dest=root/target;dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(source,dest);dest.chmod(0o755)
    q=run('ldd',str(source)).stdout.decode()
    for name in re.findall(r'(/[^\s()]+)',q):
        src=Path(name)
        if src.is_file():
            dest=root/name.lstrip('/');dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(src,dest);dest.chmod(0o755)
base=Path(tempfile.mkdtemp(prefix='recab-p04-ext4-'));loop=None;mounts=set();child=None;success=False
try:
    image=base/'scratch.img'
    root_mib=2560 if args.stage_recstrap else 128
    with image.open('wb') as f:f.truncate((root_mib*2+144)*1024*1024)
    run('sfdisk',str(image),input=f'label:gpt\nsize={root_mib}M,type=L,name=TEST_A\nsize={root_mib}M,type=L,name=TEST_B\nsize=32M,type=L,name=TEST_STATE\nsize=96M,type=U,name=TEST_ESP\n'.encode())
    loop=run('losetup','--find','--show','--partscan',str(image)).stdout.decode().strip()
    check(Path(run('losetup','-n','-O','BACK-FILE',loop).stdout.decode().strip()).resolve()==image.resolve(),'loop is not the scratch image')
    run('udevadm','settle')
    dev={s:loop+'p'+str(i) for i,s in enumerate(['A','B','state','esp'],1)}
    uuid={s:run('blkid','-c','/dev/null','-s','PARTUUID','-o','value',d).stdout.decode().strip() for s,d in dev.items()}
    roots={s:base/s for s in dev}
    for s in roots:
        roots[s].mkdir();run(*(['mkfs.fat','-F','32',dev[s]] if s=='esp' else ['mkfs.ext4','-q','-F',dev[s]]));run('mount',dev[s],str(roots[s]));mounts.add(s)
    with KERNEL.open('rb') as f:
        header=f.read(4096);f.seek(int.from_bytes(header[0x20e:0x210],'little')+512);release=f.read(256).split()[0].decode()
    module=MODULE.read_bytes();kernel_hash=sha(KERNEL.read_bytes())
    stripped=base/'recab';shutil.copyfile(BIN,stripped);run('strip',str(stripped));stripped.chmod(0o755)
    dep=b'kernel/test.ko:\n';builtin=b''
    template=(Path(__file__).resolve().parents[2]/'recinit/templates/init_installed.template').read_text()
    variables={'IMMUTABLE_INIT_MARKER':'# recab-immutable-v3','KERNEL_RELEASE':release,'BOOT_MODULE_PATHS':'kernel/test.ko','MODULES_DIR':'/usr/lib/modules','FAIL_REBOOT_OPTION':'rd.emergency=reboot','IMMUTABLE_OPTION':'recab.immutable','IMMUTABLE_CONFIG':'usr/lib/recab/immutable','BOOT_RECORD':'usr/lib/recab/boot-generation','SLOT_STATE_DIR':'lib/recab/slots','NEWROOT':'/newroot','SWITCH_ROOT_INIT':'/sbin/init','MEDIA_DEADLINE_SECS':'30','BOOT_BANNER':'P04 structural fixture'}
    for key,val in variables.items():template=template.replace('{{'+key+'}}',val)
    check('{{' not in template,'unfilled installed init template')
    entries=[('init',0o100755,template.encode()),('usr/bin/busybox',0o100755,Path('/usr/bin/true').read_bytes()),(f'usr/lib/modules/{release}/kernel/test.ko',0o100644,module),(f'usr/lib/modules/{release}/modules.dep',0o100644,dep),(f'usr/lib/modules/{release}/modules.builtin',0o100644,builtin)]
    initrd=base/'installed.img';initrd.write_bytes(cpio(entries));initrd_hash=sha(initrd.read_bytes())
    esp=roots['esp'];state=roots['state']/'recab';state.mkdir();proc=base/'proc';(proc/'self').mkdir(parents=True);(proc/'filesystems').write_text(Path('/proc/filesystems').read_text());efi=base/'efi';(efi/'efivars').mkdir(parents=True)
    (esp/'loader/entries').mkdir(parents=True);(esp/'legacy').mkdir();shutil.copyfile(KERNEL,esp/'legacy/vmlinuz');shutil.copyfile(initrd,esp/'legacy/initrd.img')
    env=dict(os.environ,RECAB_BOOT_ROOT=str(esp),RECAB_STATE_DIR=str(state),RECAB_PROC_PATH=str(proc),RECAB_SYS_FIRMWARE_EFI_PATH=str(efi),RECAB_EFIVARS_PATH=str(efi/'efivars'),RECAB_LOCK_PATH=str(base/'lock'),RECAB_STAGE_DIR=str(base/'staging'))
    for slot in ['A','B']:
        root=roots[slot]
        for directory in ['etc','usr/lib/recab','usr/lib64','usr/bin','usr/lib/systemd','sbin','var','root','run','tmp','dev','proc','sys','boot','home']:(root/directory).mkdir(parents=True,exist_ok=True)
        (root/'tmp').chmod(0o1777);(root/'root').chmod(0o700)
        for link,target in [('lib','usr/lib'),('lib64','usr/lib64'),('bin','usr/bin')]: (root/link).symlink_to(target)
        install_elf(root,Path('/usr/bin/true'),'usr/lib/systemd/systemd' if args.init=='systemd' else 'sbin/openrc-init')
        (root/'sbin/init').symlink_to('../usr/lib/systemd/systemd' if args.init=='systemd' else 'openrc-init')
        install_elf(root,Path('/usr/bin/true'),'usr/bin/sh');install_elf(root,stripped,'usr/bin/recab')
        (root/'etc/os-release').write_text(f'ID=lev\nNAME=LevitateOS\nREC_INIT={args.init}\nREC_LIBC=glibc\nREC_FLAVOUR=base\nREC_PACKAGES=rpm\nREC_PRIVILEGE=sudo\nREC_DESKTOP=plasma\nREC_KERNEL_LINE=latest\n')
        (root/'etc/fstab').write_text(f'PARTUUID={uuid[slot]} / ext4 ro 0 0\nPARTUUID={uuid["state"]} /var ext4 defaults 0 0\n/var/roothome /root none bind 0 0\ntmpfs /tmp tmpfs mode=1777,nosuid,nodev 0 0\nPARTUUID={uuid["esp"]} /boot vfat defaults 0 0\n')
        (root/'usr/lib/recab/immutable').write_text(f'{slot} PARTUUID={uuid["state"]}\n')
        (root/'etc/machine-id').write_text('123456789abcdef0123456789abcdef0\n');(root/'etc/machine-id').chmod(0o444)
        for file,text,mode in [('passwd','root:x:0:0:root:/root:/bin/sh\n',0o644),('shadow','root:!:20000:0:99999:7:::\n',0o600),('group','root:x:0:\n',0o644)]: (root/f'etc/{file}').write_text(text);(root/f'etc/{file}').chmod(mode)
        if args.init=='systemd':(root/'etc/gshadow').write_text('root:*::\n');(root/'etc/gshadow').chmod(0o600)
        else:(root/'etc/conf.d').mkdir();(root/'etc/conf.d/hwclock').write_text('clock_adjfile="YES"\n')
        (root/'etc/config').write_text(slot+'\n')
        modules=root/f'usr/lib/modules/{release}';(modules/'kernel').mkdir(parents=True);(modules/'kernel/test.ko').write_bytes(module);(modules/'modules.dep').write_bytes(dep);(modules/'modules.builtin').write_bytes(builtin)
        cli(dict(env,RECAB_ROOT=str(root)),'install-unit','--root',str(root),'--init',args.init)
        check((root/'usr/lib/recab/runtime.json').exists(),'producer runtime compatibility record missing')
    def generation(slot):
        root=roots[slot];g={'inventory_digest':digest(cli(dict(env,RECAB_ROOT=str(root)),'inventory','--root',str(root))),'kernel_digest':kernel_hash,'initrd_digest':initrd_hash}
        g['id']=sha(packed([g['inventory_digest'],g['kernel_digest'],g['initrd_digest']]))
        (root/'usr/lib/recab/generation.json').write_bytes(packed({'version':1,'generation':g['id'],'slot':slot,'partuuid':uuid[slot]}))
        (root/'usr/lib/recab/boot-generation').write_text(f'1 {slot} {uuid[slot]} {g["id"]} {uuid["B" if slot=="A" else "A"]}\n')
        return g
    # Use a valid host label on the scratch root inode when SELinux assigns
    # labels after extraction. Synchronization must preserve it, not request
    # removal of a kernel-owned label from an otherwise unlabeled fixture.
    try:label=os.getxattr(base,'security.selinux')
    except OSError as error:
        if error.errno not in (errno.ENODATA,errno.ENOTSUP):raise
    else:
        for s in ['A','B']:os.setxattr(roots[s],'security.selinux',label)
    ga=generation('A');gb=generation('B')
    if args.stage_recstrap:
        release_image=base/'fixture.erofs';run('mkfs.erofs',str(release_image),str(roots['A']))
        env['RECAB_RECSTRAP_BIN']=str(args.stage_recstrap.resolve(strict=True))
    def legacy_entry(slot,g):
        (esp/f'loader/entries/lev-{slot.lower()}.conf').write_text(f'title {slot}\nlinux /legacy/vmlinuz\ninitrd /legacy/initrd.img\noptions root=PARTUUID={uuid[slot]} ro recab.immutable recab.slot={slot} recab.generation={g["id"]}\n')
    for slot,g in [('A',ga),('B',gb)]:legacy_entry(slot,g)
    ledger={'schema':{'major':2,'minor':1},'slots':[{'slot':s,'partuuid':uuid[s],'generation':g,'readiness':'committed' if s=='A' else 'ready'} for s,g in [('A',ga),('B',gb)]],'known_good_slot':'A','previous_good_slot':'B'}
    (state/'state.json').write_bytes(packed(ledger));(esp/'loader/loader.conf').write_text('default lev-a.conf\ntimeout 5\n')
    def variable(name,value):
        file=efi/f'efivars/LoaderEntry{name}-{GUID}'
        if value is None:file.unlink(missing_ok=True)
        else:file.write_bytes((7).to_bytes(4,'little')+(value+'\0').encode('utf-16le'))
    variable('Default','lev-a.conf')
    run('umount',str(roots['B']));mounts.remove('B');run('mount','-o','remount,ro',str(roots['A']));run('mount','--bind',str(roots['state']),str(roots['A']/'var'));mounts.add('A/var')
    for s in ['A','B']:run('blockdev','--setro',dev[s])
    def running(slot,g):
        (proc/'cmdline').write_text(f'root=PARTUUID={uuid[slot]} ro recab.immutable recab.slot={slot} recab.generation={g["id"]}\n');(proc/'self/mountinfo').write_text(Path('/proc/self/mountinfo').read_text());return dict(env,RECAB_ROOT=str(roots[slot]))
    active=running('A',ga)
    if args.stage_recstrap:
        cli(dict(active,RECAB_RECSTRAP_BIN='/usr/bin/false'),'stage','--rootfs',str(release_image),'--kernel',str(esp/'legacy/vmlinuz'),'--initrd',str(initrd),code=22)
        failed_stage=cli(active,'status')['preparation'];check(run('blockdev','--getro',dev['B']).stdout.strip()==b'1' and not (esp/'loader/entries/lev-b.conf').exists(),'failed extraction left candidate writable/bootable')
        cli(active,'discard','--preparation',failed_stage['id'],'--revision',str(failed_stage['revision']))
        cli(active,'stage','--rootfs',str(release_image),'--kernel',str(esp/'legacy/vmlinuz'),'--initrd',str(initrd))
        stage_status=cli(active,'status');check(stage_status['slots'][1]['generation_ready'],'staging bypassed/failed finalization')
        check(run('blockdev','--getro',dev['B']).stdout.strip()==b'1','staging left candidate unprotected')
        staged=stage_status['preparation'];cli(active,'discard','--preparation',staged['id'],'--revision',str(staged['revision']))
    prepared=cli(active,'prepare');check(prepared['phase']=='prepared','preparation did not complete')
    # Simulate an explicitly prepared system edit until P06 owns editing.
    run('blockdev','--setrw',dev['B']);run('mount','-o','rw',dev['B'],str(roots['B']));mounts.add('B')
    (roots['B']/'etc/config').write_text('local X\n');run('umount',str(roots['B']));mounts.remove('B');run('blockdev','--setro',dev['B'])
    # Bad archive refuses before any finalization state/root mutation.
    invalid=base/'bad.img';invalid.write_bytes(initrd.read_bytes()[:-9]);before=(state/'state.json').read_bytes()
    cli(active,'finalize','--kernel',str(esp/'legacy/vmlinuz'),'--initrd',str(invalid),code=21)
    check(before==(state/'state.json').read_bytes() and not (esp/'loader/entries/lev-b.conf').exists(),'invalid archive changed candidate state')
    # Kill after durable finalization intent while audit output is blocked.
    audit=state/'audit.log';audit.unlink();os.mkfifo(audit)
    child=subprocess.Popen([BIN,'--json','finalize'],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    deadline=time.monotonic()+60
    while time.monotonic()<deadline:
        q=json.loads((state/'state.json').read_text())
        if q['preparation']['phase']=='finalizing':child.kill();child.communicate(timeout=10);break
        if child.poll() is not None:raise RuntimeError(child.communicate())
        time.sleep(.005)
    else:raise RuntimeError('finalization intent not observed')
    audit.unlink();interrupted=cli(active,'status')['preparation']
    check(not (esp/'loader/entries/lev-b.conf').exists() and run('blockdev','--getro',dev['A']).stdout.strip()==b'1','interruption exposed candidate or unprotected A')
    # Force publication failure with an actual read-only bind in the child's
    # private namespace, after its root-generation checkpoint was saved.
    child=subprocess.Popen([BIN,'--json','finalize','--preparation',interrupted['id'],'--revision',str(interrupted['revision'])],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    deadline=time.monotonic()+60;injected=False
    while time.monotonic()<deadline:
        q=json.loads((state/'state.json').read_text())['preparation']
        if q.get('finalization') and q['phase']=='finalizing':
            run('nsenter','-t',str(child.pid),'-m','mount','--bind',str(esp/'loader/entries'),str(esp/'loader/entries'))
            run('nsenter','-t',str(child.pid),'-m','mount','-o','remount,bind,ro',str(esp/'loader/entries'));injected=True;break
        if child.poll() is not None:raise RuntimeError('finalizer finished before fault injection: '+str(child.communicate()))
        time.sleep(.001)
    check(injected,'publication fault not injected');stdout,stderr=child.communicate(timeout=120);check(child.returncode==22,f'publication fault: {child.returncode}: {stderr}')
    failed=cli(active,'status')['preparation'];check(failed['phase']=='interrupted' and not (esp/'loader/entries/lev-b.conf').exists(),'failed publication left a bootable candidate')
    check(run('blockdev','--getro',dev['B']).stdout.strip()==b'1','failed publication left candidate writable')
    # Collection must preserve foreign references, including FAT case aliases.
    orphan=esp/('recab/objects/'+'1'*64+'.linux');foreign=esp/('recab/objects/'+'2'*64+'.linux')
    orphan.write_bytes(b'orphan');foreign.write_bytes(b'foreign')
    (esp/'loader/entries/foreign.CONF').write_text('title foreign\nlinux /RECAB/OBJECTS/'+foreign.name.upper()+'\n')
    complete=cli(active,'finalize','--preparation',failed['id'],'--revision',str(failed['revision']))
    check(not orphan.exists() and foreign.exists(),'collection deleted a foreign reference or retained orphan')
    check(complete['seal']['protocol']==3 and complete['reused_boot_files']==2,'retry did not reuse durable payloads')
    status=cli(active,'status');candidate=status['slots'][1];check(candidate['generation_ready'] and candidate['seal']['generation']==complete['generation']['id'],'candidate is not generation-ready')
    check('recab.slot=B' in (esp/'loader/entries/lev-b.conf').read_text() and 'recab/objects/' in candidate['kernel'],'entry not bound to slot/artifacts')
    try:run('mount','-t','ext4','-o','rw',dev['B'],str(roots['B']))
    except subprocess.CalledProcessError:pass
    else:
        mounts.add('B');table=Path('/proc/self/mountinfo').read_text().splitlines();entry=next(line.split() for line in table if line.split()[4]==str(roots['B']))
        check('rw' not in entry[5].split(','),'sealed candidate mounted writable');run('umount',str(roots['B']));mounts.remove('B')
    # Idempotence and stale revision refusal.
    same=cli(active,'finalize');check(same['generation']['id']==complete['generation']['id'],'idempotent finalization changed identity')
    current=cli(active,'status')['preparation'];cli(active,'finalize','--preparation',current['id'],'--revision','0',code=21,contains='stale')
    cli(active,'set-next','B');check((efi/f'efivars/LoaderEntryOneShot-{GUID}').exists(),'trial was not scheduled')
    # Supply observed boot records, without claiming a VM boot occurred.
    run('umount',str(roots['A']/'var'));mounts.remove('A/var');run('umount',str(roots['A']));mounts.remove('A')
    run('mount','-o','ro',dev['B'],str(roots['B']));mounts.add('B');run('mount','--bind',str(roots['state']),str(roots['B']/'var'));mounts.add('B/var')
    variable('OneShot',None);variable('Selected','lev-b.conf');bgen=complete['generation'];on_b=running('B',bgen);check((roots['B']/'etc/config').read_text()=='local X\n','finalization lost local X');cli(on_b,'commit')
    reverse=cli(on_b,'prepare');check(reverse['slot']=='A','reverse preparation picked wrong slot')
    back=cli(on_b,'finalize');check(back['boot_bytes_written']==0 and back['reused_boot_files']==2,'reverse copied unchanged boot payloads')
    check(run('blockdev','--getro',dev['A']).stdout.strip()==b'1' and run('blockdev','--getro',dev['B']).stdout.strip()==b'1','both roots are not protected')
    cli(on_b,'set-next','A');cli(on_b,'rollback');check(not (efi/f'efivars/LoaderEntryOneShot-{GUID}').exists(),'rollback did not cancel trial')
    # Root/boot-record and object tampering are refused before changing firmware.
    run('blockdev','--setrw',dev['A']);run('mount','-o','rw',dev['A'],str(roots['A']));mounts.add('A')
    record=roots['A']/'usr/lib/recab/boot-generation';saved=record.read_bytes();record.write_text('1 B wrong wrong wrong\n');run('umount',str(roots['A']));mounts.remove('A');run('blockdev','--setro',dev['A'])
    cli(on_b,'set-next','A',code=21,contains='boot record')
    run('blockdev','--setrw',dev['A']);run('mount','-o','rw',dev['A'],str(roots['A']));mounts.add('A');record.write_bytes(saved);run('umount',str(roots['A']));mounts.remove('A');run('blockdev','--setro',dev['A'])
    # Artifact corruption is refused even when its immutable name still exists.
    object=esp/candidate['kernel'].lstrip('/');saved_object=object.read_bytes();object.write_bytes(saved_object[:-1]+bytes([saved_object[-1]^1]))
    cli(on_b,'set-next','A',code=21,contains='kernel content');object.write_bytes(saved_object)
    cli(on_b,'set-next','A');cli(on_b,'rollback')
    success=True;print(json.dumps({'result':'PASS','init':args.init,'stage_checked':bool(args.stage_recstrap),'kernel_release':release,'interrupted_finalization_recovered':True,'publication_failure_recovered':True,'reverse_boot_bytes_written':back['boot_bytes_written'],'reused_boot_files':back['reused_boot_files'],'checks':['real ext4 roots and vfat ESP','full archive validation','runtime compatibility binding','durable intent','entry absent on interruption/failure','readonly block devices','generation-bound entries','set-next/commit/rollback in both directions','boot record tampering refusal','artifact reuse','local X preserved','foreign-reference collection','artifact tampering refusal']},indent=2))
finally:
    if child is not None and child.poll() is None:child.kill();child.communicate(timeout=10)
    for key in sorted(mounts,key=lambda p:p.count('/'),reverse=True):subprocess.run(['umount',str(base/key)],check=False)
    if loop:subprocess.run(['losetup','-d',loop],check=False)
    if success:shutil.rmtree(base)
    else:print('Failure artifacts:',base)
