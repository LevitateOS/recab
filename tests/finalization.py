#!/usr/bin/env python3
"""Real ext4/vfat CLI integration for sealing/publication, not a kernel boot.
Uses an existing UEFI kernel/module, native ELF programs and temporary GPT disks.
The initramfs and observed firmware/boot records are structural fixtures; only
an actual VM boot can establish PID 1, hardware-driver and service behavior.
"""
import argparse,copy,errno,gzip,hashlib,json,os,re,shutil,signal,struct,subprocess,tempfile,time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--binary',required=True,type=Path);p.add_argument('--kernel',required=True,type=Path);p.add_argument('--module',required=True,type=Path);p.add_argument('--init',choices=['systemd','openrc'],default='systemd');p.add_argument('--stage-recstrap',type=Path,help='also exercise destructive staging through the shared finalizer');p.add_argument('--installation',action='store_true');p.add_argument('--recsetup',type=Path);p.add_argument('--busybox',type=Path);args=p.parse_args()
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
def installation():
    """Actual recsetup/recab install on a temporary disk, without a kernel boot."""
    check(args.recsetup is not None and args.busybox is not None,'--installation needs --recsetup and --busybox')
    root=roots['A'];state_root=roots['state'];boot=roots['esp']
    (root/'home').rmdir();(root/'home').symlink_to('var/home');(state_root/'home').mkdir()
    (root/'root/personal').write_text('root home is shared from either first boot\n')
    # Real native busybox provides the chroot's shell/account tools; native
    # recab/ELF fixtures still describe a glibc target on this x86_64 host.
    busybox=args.busybox.resolve(strict=True)
    shutil.copyfile(busybox,root/'usr/bin/busybox');(root/'usr/bin/busybox').chmod(0o755)
    for applet in run(str(busybox),'--list').stdout.decode().splitlines():
        path=root/'usr/bin'/applet
        if not path.exists():path.symlink_to('busybox')
    (root/'usr/bin/sh').unlink();(root/'usr/bin/sh').symlink_to('busybox')
    (root/'etc/os-release').write_text((root/'etc/os-release').read_text().replace('REC_PACKAGES=rpm','REC_PACKAGES=apk'))
    setup=root/'usr/libexec/recipe-system-setup';setup.parent.mkdir(parents=True);shutil.copyfile(Path(__file__).resolve().parents[3]/'distro/builder/guest/installed/system-setup.sh',setup);setup.chmod(0o755)
    install_elf(root,Path('/usr/bin/ssh-keygen'),'usr/bin/ssh-keygen')
    (root/'etc/ssh').mkdir();run('ssh-keygen','-q','-t','ed25519','-N','','-f',str(root/'etc/ssh/ssh_host_ed25519_key'))
    if args.init=='systemd':
        unit=root/'usr/lib/systemd/system/sshd.service';unit.parent.mkdir(parents=True,exist_ok=True);unit.write_text('[Service]\nExecStart=/usr/bin/true\n')
        enabled=root/'etc/systemd/system/multi-user.target.wants';enabled.mkdir(parents=True,exist_ok=True);(enabled/'sshd.service').symlink_to('/usr/lib/systemd/system/sshd.service')
    else:
        (root/'etc/init.d/sshd').write_text('#!/bin/sh\nexit 0\n');(root/'etc/init.d/sshd').chmod(0o755)
        enabled=root/'etc/runlevels/default';enabled.mkdir(parents=True,exist_ok=True);(enabled/'sshd').symlink_to('/etc/init.d/sshd')
    loader=root/'usr/lib/systemd/boot/efi/systemd-bootx64.efi';loader.parent.mkdir(parents=True);shutil.copyfile('/usr/lib/systemd/boot/efi/systemd-bootx64.efi',loader)
    # Producer-style binding of this fixture's genuine cached kernel/module.
    module_root=root/f'usr/lib/modules/{release}';h=hashlib.sha256()
    def module_tree(directory,relative=Path()):
        for item in sorted(directory.iterdir()):
            path=relative/item.name;name=os.fsencode(path);h.update(len(name).to_bytes(8,'little'));h.update(name)
            if item.is_dir():h.update(b'd');module_tree(item,path)
            else:h.update(b'f');h.update(hashlib.sha256(item.read_bytes()).digest())
    module_tree(module_root)
    (root/'usr/lib/recab/kernel.json').write_bytes(packed({'version':1,'release':release,'kernel_digest':kernel_hash,'modules_digest':h.hexdigest()}))
    (root/'etc/NetworkManager/system-connections').mkdir(parents=True)
    network=root/'etc/NetworkManager/system-connections/personal.nmconnection';network.write_text('[connection]\nid=personal\n');network.chmod(0o600)
    with (root/'usr/share/personal-payload').open('wb') as output:
        for _ in range(16):output.write(b'personal X\0'*(1024*1024//11)+b'x'*(1024*1024%11))
    run('umount',str(roots['B']));mounts.remove('B')
    run('mount','--bind',str(state_root),str(root/'var'));mounts.add('A/var')
    run('mount','--bind',str(boot),str(root/'boot'));mounts.add('A/boot')
    os_release=base/'live-os-release';os_release.write_text('ID=recab-install-fixture\n')
    env.update(RECAB_ROOT='/',RECAB_OS_RELEASE_PATH=str(os_release),PATH=str(Path(BIN).parent)+':'+os.environ['PATH'])
    record=root/'var/lib/recab/installation.json';progress=root/'var/lib/recsetup/apply.json'
    request=base/'request.json';request.write_bytes(packed({'hostname':'original','root_password':True,'users':[{'name':'alice','admin':False,'password':True}],'boot':{'mode':'ab','slot_a':dev['A'],'slot_b':dev['B'],'kernel':str(KERNEL),'initrd':str(initrd)}}))
    command=[str(args.recsetup.resolve()),'--json','apply','--root',str(root),'--request',str(request)]
    def refresh(): (proc/'self/mountinfo').write_text(Path('/proc/self/mountinfo').read_text())
    def setup_run(passwords=False):
        refresh();return subprocess.run(command+(['--passwords-stdin'] if passwords else []),env=env,input='root:fixture-password\nalice:fixture-password\n' if passwords else None,text=True,capture_output=True,timeout=120)
    original_initrd=initrd.read_bytes();initrd.write_bytes(original_initrd[:-9])
    first=setup_run(True);check(first.returncode!=0,'truncated initrd was accepted')
    check(record.exists() and progress.exists(),'installation/personalization progress was not durable: '+first.stderr+first.stdout)
    check('fixture-password' not in progress.read_text(),'password leaked into installation progress')
    check((root/'etc/hostname').read_text().strip()=='original' and 'alice:' in (root/'etc/passwd').read_text(),'real personalization did not finish before init: '+first.stderr+first.stdout)
    shadow=(root/'etc/shadow').read_bytes();host_key=(root/'etc/ssh/ssh_host_ed25519_key').read_bytes();machine_id=(root/'etc/machine-id').read_bytes()
    (root/'etc/hostname').write_text('changed-after-failure\n');initrd.write_bytes(original_initrd)
    child=None
    try:
        refresh();child=subprocess.Popen(command,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
        deadline=time.monotonic()+60
        journal=root/f'var/lib/recab/synchronize-install-{uuid["B"]}.json'
        while time.monotonic()<deadline:
            if journal.exists():os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10);break
            if child.poll() is not None:raise RuntimeError('install ended before interruption: '+str(child.communicate()))
            time.sleep(.001)
        else:raise RuntimeError('copy intent not observed')
        check(not (boot/'loader/entries/lev-a.conf').exists() and not (boot/'loader/entries/lev-b.conf').exists(),'interrupted copy exposed entries')
        check(run('blockdev','--getro',dev['A']).stdout.strip()==b'1','source A was not protected during copy')
        # Fail publication using a real read-only bind inside recab's namespace.
        refresh();child=subprocess.Popen(command,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
        deadline=time.monotonic()+60;injected=False
        while time.monotonic()<deadline:
            if record.exists() and json.loads(record.read_text()).get('synchronization'):
                children=Path(f'/proc/{child.pid}/task/{child.pid}/children').read_text().split()
                if children:
                    pid=children[0];entries=str(root/'boot/loader/entries')
                    run('nsenter','-t',pid,'-m','mount','--bind',entries,entries);run('nsenter','-t',pid,'-m','mount','-o','remount,bind,ro',entries);injected=True;break
            if child.poll() is not None:raise RuntimeError('install ended before publication failure: '+str(child.communicate()))
            time.sleep(.001)
        check(injected,'publication fault was not injected');out,err=child.communicate(timeout=120);check(child.returncode!=0 and not json.loads(record.read_text())['complete'],'failed publication reported installation complete: '+out+err)
        check(all(run('blockdev','--getro',dev[s]).stdout.strip()==b'1' for s in ['A','B']),'failed publication left an unsealed root')
        recovered=setup_run();check(recovered.returncode==0,'installation recovery failed: '+recovered.stderr+recovered.stdout)
        complete=json.loads(record.read_text());check(complete['complete'],'successful installation did not mark completion')
        check(complete['synchronization']['payload_bytes_written']==0,'publication recovery recopied payloads')
        wrong=base/'wrong-kernel';wrong.write_bytes(b'wrong kernel')
        cli(env,'init','--root',str(root),'--slot-a',dev['A'],'--slot-b',dev['B'],'--kernel',str(wrong),'--initrd',str(initrd),code=21,contains='boot inputs')
        again=setup_run();check(again.returncode==0,'completed installer retry failed: '+again.stderr+again.stdout)
        check((root/'etc/shadow').read_bytes()==shadow and (root/'etc/machine-id').read_bytes()==machine_id and (root/'etc/ssh/ssh_host_ed25519_key').read_bytes()==host_key,'retry regenerated passwords/machine identity/host keys')
        check((state_root/'roothome/personal').read_text().startswith('root home is shared'),'initial root home was not seeded for B-first boot')
        run('mount','-o','ro',dev['B'],str(roots['B']));mounts.add('B')
        for path in ['etc/hostname','etc/passwd','etc/group','etc/shadow','etc/machine-id','etc/ssh/ssh_host_ed25519_key','etc/NetworkManager/system-connections/personal.nmconnection']:
            check((root/path).read_bytes()==(roots['B']/path).read_bytes(),f'personalized A/B differ at {path}')
        check((roots['B']/'etc/hostname').read_text().strip()=='changed-after-failure','retry lost work made after first attempt')
        check(uuid['B'] in (roots['B']/'etc/fstab').read_text(),'B retained A root identity')
        run('umount',str(roots['B']));mounts.remove('B')
        try:(root/'etc/forbidden-after-sealing').write_text('must fail')
        except OSError:pass
        else:raise RuntimeError('sealed A accepted a system write')
        # Observe A as a read-only boot mount (no VM/kernel boot is claimed).
        run('mount','-o','remount,ro',str(root))
        active=dict(env,RECAB_ROOT=str(root),RECAB_BOOT_ROOT=str(root/'boot'),RECAB_STATE_DIR=str(root/'var/lib/recab'));active.pop('RECAB_OS_RELEASE_PATH')
        generation=complete['generation'];(proc/'cmdline').write_text(f'root=PARTUUID={uuid["A"]} ro recab.immutable recab.slot=A recab.generation={generation["id"]}\n');refresh()
        status=cli(active,'status');check(all(s['generation_ready'] and s['bootable'] and s['generation']['id']==generation['id'] for s in status['slots']),'both initial systems are not ready/bootable')
        check(status['slots'][0]['readiness']=='committed' and status['slots'][1]['readiness']=='ready' and status['last_trial'] is None,'initial B was falsely trial-committed')
        cli(active,'set-next','B');cli(active,'rollback')
        prepared=cli(active,'prepare');check(prepared['synchronization']['payload_bytes_written']==0,'first continuation rewrote the installed system')
        # A completed init must never reset a preparation or resurrect its entry.
        refused=setup_run();check(refused.returncode!=0 and not (boot/'loader/entries/lev-b.conf').exists(),'installer retry reset a live preparation')
        return {'result':'PASS','init':args.init,'personalized_equivalent_slots':True,'first_prepare_payload_bytes':prepared['synchronization']['payload_bytes_written'],'checks':['real GPT/ext4/vfat','real recsetup chroot personalization','passwords/users/hostname/network/services/keys preserved','shared home seeding','durable copy interruption/recovery','publication failure/recovery','idempotent installer retry','both initial generation-bound entries','initial B ready without fabricated trial','first continuation writes no payload','live ledger reset refusal']}
    finally:
        if child is not None and child.poll() is None:os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10)

base=Path(tempfile.mkdtemp(prefix='recab-p05-ext4-' if args.installation else 'recab-p04-ext4-'));loop=None;mounts=set();child=None;success=False
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
    for slot in (['A'] if args.installation else ['A','B']):
        root=roots[slot]
        for directory in ['etc','usr/lib/recab','usr/lib64','usr/bin','usr/share','usr/lib/systemd','sbin','var','root','run','tmp','dev','proc','sys','boot','home']:(root/directory).mkdir(parents=True,exist_ok=True)
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
    if args.installation:
        result=installation();success=True;print(json.dumps(result,indent=2));raise SystemExit(0)
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
