"""Real signed feed + Recipe/Ginux HTTP + ext4/EROFS update integration.
The fixture uses native ELF programs and an existing kernel, never an OS build.
"""
import functools,hashlib,http.server,json,os,re,shutil,signal,socket,stat,subprocess,tarfile,threading,time
from pathlib import Path

class Updates:
    def __init__(self,h):
        self.base=h['base'];self.run=h['run'];self.check=h['check'];self.repo=Path(__file__).resolve().parents[3]
        self.check(h['args'].release_builder is not None,'--updates requires --release-builder')
        self.recipe=str(h['args'].updates.resolve(strict=True));self.builder=str(h['args'].release_builder.resolve(strict=True))
        self.key=self.base/'key.der';self.run('openssl','genpkey','-algorithm','ED25519','-outform','DER','-out',str(self.key))
        self.public=self.run('openssl','pkey','-inform','DER','-in',str(self.key),'-pubout','-outform','DER').stdout[-32:].hex()
        self.publish=self.base/'published';self.publish.mkdir();self.requests=[];self.fail=None
        owner=self
        class Handler(http.server.SimpleHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_GET(self):
                owner.requests.append(self.path)
                if owner.fail and self.path.endswith(owner.fail):
                    owner.fail=None;path=self.translate_path(self.path);data=Path(path).read_bytes()
                    self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers()
                    self.wfile.write(data[:max(1,len(data)//2)]);self.wfile.flush();self.connection.shutdown(socket.SHUT_RDWR);return
                super().do_GET()
        self.server=http.server.ThreadingHTTPServer(('127.0.0.1',0),functools.partial(Handler,directory=str(self.publish)))
        threading.Thread(target=self.server.serve_forever,daemon=True).start()
        self.endpoint=f'127.0.0.1:{self.server.server_port}'
        self.policy={'channel':'development','sequence':1,'source':{'index':f'http://{self.endpoint}/index.json','recipe':'usr/share/recab/update-acquire.rhai'},'trust':{'ed25519':self.public}}
        self.producer=(self.repo/'distro/builder/os-recipes/update-acquire.rhai').read_text().replace('network: [] /* recab configured endpoint */','network: '+json.dumps([self.endpoint]))
    def configure(self,root):
        (root/'usr/lib/recab/update-source.json').write_text(json.dumps(self.policy))
        dest=root/'usr/share/recab/update-acquire.rhai';dest.parent.mkdir(parents=True,exist_ok=True);dest.write_text(self.producer)
    def seed(self,root,h):
        digest=hashlib.sha256()
        def walk(directory,relative=Path()):
            for item in sorted(directory.iterdir()):
                path=relative/item.name;name=os.fsencode(path);digest.update(len(name).to_bytes(8,'little'));digest.update(name)
                if item.is_dir():digest.update(b'd');walk(item,path)
                else:digest.update(b'f');digest.update(hashlib.sha256(item.read_bytes()).digest())
        walk(root/'usr/lib/modules'/h['release'])
        (root/'usr/lib/recab/kernel.json').write_text(json.dumps({'version':1,'release':h['release'],'kernel_digest':h['kernel_hash'],'modules_digest':digest.hexdigest()}))
        for file,body in [('local-delete','keep deleted locally\n'),('release-delete','removed by release\n'),('collision','original\n'),('mode-conflict','same contents\n')]:
            (root/'etc'/file).write_text(body)
        (root/'etc/type-tree').mkdir();(root/'etc/type-tree/child').write_text('original child\n')
        (root/'usr/share/ranges').write_bytes(b'O'*(32*1024*1024))
        linked=root/'usr/share/hard-a';linked.write_text('old hardlinked body\n');os.link(linked,root/'usr/share/hard-b')
        for file,row in [('passwd','oldsvc:x:100:100:Old:/nonexistent:/bin/sh\n'),('shadow','oldsvc:!:20000:0:99999:7:::\n'),('group','oldsvc:x:100:\n'),('gshadow','oldsvc:*::\n')]:
            path=root/'etc'/file
            if path.exists():path.write_text(path.read_text()+row)
        (root/'etc/group').write_text((root/'etc/group').read_text().replace('root:x:0:','root:x:0:root,oldsvc'))
        if (root/'etc/gshadow').exists():(root/'etc/gshadow').write_text((root/'etc/gshadow').read_text().replace('root:*::','root:*::root,oldsvc'))
    def exercise(self,h,active):
        run=self.run;check=self.check;cli=h['cli'];roots=h['roots'];state=h['state'];base=self.base
        running=h['first'];inactive=h['other']
        filesystem_uuid=run('blkid','-c','/dev/null','-s','UUID','-o','value',h['dev'][inactive]).stdout
        # Tiny real native Recipe worker/runtime, matching the actual launcher.
        worker=base/'recipe-worker';shutil.copyfile(self.recipe,worker);run('strip',str(worker));worker.chmod(0o755)
        runtime=base/'runtime';(runtime/'etc').mkdir(parents=True)
        for name in re.findall(r'(/[^\s()]+)',run('ldd',str(worker)).stdout.decode()):
            src=Path(name)
            if src.is_file():
                dst=runtime/name.lstrip('/');dst.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(src,dst);dst.chmod(0o755)
        archive=base/'runtime.tar'
        with tarfile.open(archive,'w') as tar:
            for item in sorted(runtime.iterdir()):tar.add(item,arcname=item.name)
        active.update(RECAB_RECIPE_BIN=self.recipe,GINUX_ROOTFS=str(archive),GINUX_ROOT='oci:recab-native-fixture@sha256:'+hashlib.sha256(archive.read_bytes()).hexdigest(),RECIPE_WORKER_BIN=str(worker),TMPDIR=str(base/'tmp'))
        Path(active['TMPDIR']).mkdir()
        def package_recipe(path,release=False,ack=False):
            inputs='inputs:#{release:"file"},' if release else ''
            value='RECAB_RELEASE_BASE' if ack else '""'
            path.write_text('const RECIPE_API=2;\nconst NEEDS=#{'+inputs+'outputs:#{system:"directory"}};\n'+
                'let ctx=#{name:"p09-native",version:"1",root:RUNTIME.paths.output+"/system",recab_release:'+value+'};\n'+
                'fn is_acquired(ctx){ready(ctx)} fn is_built(ctx){ready(ctx)}\n'+
                'fn is_installed(ctx){if !is_file(ctx.root+"/usr/lib/p09-local.so"){return needs_work(ctx,"missing payload");}ctx.outputs=#{system:ctx.root};ready(ctx)}\n'+
                'fn install(ctx){'+('verify_sha256(join_path(RUNTIME.paths.inputs,"release"),ctx.recab_release);' if ack else '')+
                'mkdir(ctx.root+"/usr/lib");copy_file("/lib64/libm.so.6",ctx.root+"/usr/lib/p09-local.so");target_mode(ctx.root,"usr/lib/p09-local.so",0o755);target_finalize(ctx.root);ctx.outputs=#{system:ctx.root};ctx}\n')
        initial_recipe=base/'native-initial.rhai';package_recipe(initial_recipe);cli(active,'change','install',str(initial_recipe))
        bad_recipe=base/'native-unbound.rhai';package_recipe(bad_recipe,True)
        new_recipe=base/'native-release.rhai';package_recipe(new_recipe,True,True)
        # Build only a tiny native image from the existing fixture tree.
        bundle=base/'bundle';bundle.mkdir();source=bundle/'rootfs-source';source.mkdir();(bundle/'overlay').mkdir()
        for item in roots[running].iterdir():
            if item.name in ['boot','var','home','root','proc','sys','dev','run','tmp','lost+found']:(source/item.name).mkdir()
            elif item.is_symlink():(source/item.name).symlink_to(os.readlink(item))
            elif item.is_dir():shutil.copytree(item,source/item.name,symlinks=True)
            else:shutil.copy2(item,source/item.name)
        (source/'tmp').chmod(0o1777)
        incoming_kernel=h['KERNEL'];incoming_initrd=h['initrd'];incoming_release=h['release'];incoming_hash=h['kernel_hash']
        if h['args'].update_kernel:
            check(h['args'].update_module is not None,'--update-kernel needs --update-module')
            incoming_kernel=h['args'].update_kernel.resolve(strict=True)
            with incoming_kernel.open('rb') as f:
                header=f.read(4096);f.seek(int.from_bytes(header[0x20e:0x210],'little')+512);incoming_release=f.read(256).split()[0].decode()
            incoming_hash=hashlib.sha256(incoming_kernel.read_bytes()).hexdigest()
            shutil.rmtree(source/'usr/lib/modules');modules=source/'usr/lib/modules'/incoming_release;(modules/'kernel').mkdir(parents=True)
            module=h['args'].update_module.read_bytes();(modules/'kernel/test.ko').write_bytes(module);(modules/'modules.dep').write_bytes(h['dep']);(modules/'modules.builtin').write_bytes(h['builtin'])
            incoming_initrd=base/'new-installed.img'
            incoming_initrd.write_bytes(h['cpio']([('init',0o100755,h['template'].replace(h['release'],incoming_release).encode()),('usr/bin/busybox',0o100755,Path('/usr/bin/true').read_bytes()),(f'usr/lib/modules/{incoming_release}/kernel/test.ko',0o100644,module),(f'usr/lib/modules/{incoming_release}/modules.dep',0o100644,h['dep']),(f'usr/lib/modules/{incoming_release}/modules.builtin',0o100644,h['builtin'])]))
        for member in ['generation.json','boot-generation','release-baseline.json','baseline']:(source/'usr/lib/recab'/member).unlink(missing_ok=True)
        new_policy=dict(self.policy,sequence=2);(source/'usr/lib/recab/update-source.json').write_text(json.dumps(new_policy))
        (source/'usr/share/new-release').write_text('release 2\n')
        (source/'etc/fstab').write_text((source/'etc/fstab').read_text()+'# new release mount policy comment\n')
        (source/'etc/00-new-release').write_text('release writes began\n')
        (source/'etc/release-delete').unlink();(source/'etc/collision').write_text('incoming edit\n');(source/'etc/mode-conflict').chmod(0o640)
        shutil.rmtree(source/'etc/type-tree');(source/'etc/type-tree').write_text('release changed directory into a file\n')
        with (source/'usr/share/ranges').open('r+b') as f:f.seek(16*1024*1024);f.write(b'N'*(1024*1024))
        (source/'usr/share/hard-b').unlink();os.link(source/'usr/share/hard-a',source/'usr/share/hard-b');(source/'usr/share/hard-a').write_text('new hardlinked body\n')
        shutil.copyfile('/lib64/libm.so.6',source/'usr/lib/libp09-release.so.1')
        for file,row in [('passwd',f'newsvc:x:{1000 if h["args"].merge_id_collision else 200}:200:New:/nonexistent:/bin/sh\n'),('shadow','newsvc:!:20000:0:99999:7:::\n'),('group','newsvc:x:200:\n'),('gshadow','newsvc:*::\n')]:
            path=source/'etc'/file
            if path.exists():path.write_text(path.read_text()+row)
        (source/'etc/group').write_text((source/'etc/group').read_text().replace('root,oldsvc','root,oldsvc,newsvc'))
        if (source/'etc/gshadow').exists():(source/'etc/gshadow').write_text((source/'etc/gshadow').read_text().replace('root,oldsvc','root,oldsvc,newsvc'))
        shadow=source/'etc/shadow';shadow.write_text(shadow.read_text().replace('root:!:20000:0:99999:7','root:!:22000:0:99999:14'))
        digest=hashlib.sha256()
        def walk(directory,relative=Path()):
            for item in sorted(directory.iterdir()):
                path=relative/item.name;name=os.fsencode(path);digest.update(len(name).to_bytes(8,'little'));digest.update(name)
                if item.is_symlink():
                    value=os.fsencode(os.readlink(item));digest.update(b'l');digest.update(len(value).to_bytes(8,'little'));digest.update(value)
                elif item.is_dir():digest.update(b'd');walk(item,path)
                else:digest.update(b'f');digest.update(hashlib.sha256(item.read_bytes()).digest())
        walk(source/'usr/lib/modules'/incoming_release)
        (source/'usr/lib/recab/kernel.json').write_text(json.dumps({'version':1,'release':incoming_release,'kernel_digest':incoming_hash,'modules_digest':digest.hexdigest()}))
        (bundle/'.prepared-product.json').write_text(json.dumps({'product':'base-rootfs','distro_id':'levitate','rootfs_source_dir':'rootfs-source','overlay_dir':'overlay','rootfs_erofs':'filesystem.erofs','overlay_erofs':'overlay.erofs'}))
        (bundle/'system-sources.sha256').write_text(hashlib.sha256(b'genuine P08 native fixture').hexdigest()+'\n')
        (bundle/'boot').mkdir();shutil.copyfile(incoming_kernel,bundle/'boot/vmlinuz');shutil.copyfile(incoming_initrd,bundle/'boot/initramfs-installed.img')
        driver=base/'release-input-check'
        run(os.environ.get('RECAB_TEST_RUSTC','rustc'),'--edition','2024',str(self.repo/'tools/recab/tests/release_input.rs'),'--extern','recab='+str(self.repo/'target/debug/librecab.rlib'),'--extern','distro_builder='+str(self.repo/'target/debug/libdistro_builder.rlib'),'--extern','levitate_recipe='+str(self.repo/'target/debug/liblevitate_recipe.rlib'),'-L','dependency='+str(self.repo/'target/debug/deps'),'-o',str(driver))
        run(str(driver),'--pack',str(source),str(bundle/'filesystem.erofs'));run(str(driver),'--pack',str(bundle/'overlay'),str(bundle/'overlay.erofs'))
        fixture=base/'repo';(fixture/'distro/variants/levitate').mkdir(parents=True)
        for item in self.repo.iterdir():
            if item.name!='distro':(fixture/item.name).symlink_to(item,target_is_directory=item.is_dir())
        for item in (self.repo/'distro').iterdir():
            if item.name!='variants':(fixture/'distro'/item.name).symlink_to(item,target_is_directory=item.is_dir())
        variant=self.repo/'distro/variants/levitate'
        for item in variant.iterdir():
            if item.name!='distro.toml':(fixture/'distro/variants/levitate'/item.name).symlink_to(item,target_is_directory=item.is_dir())
        text=(variant/'distro.toml').read_text().replace('sequence = 1','sequence = 2')
        text+='\n[release.trust]\ned25519 = "'+self.public+'"\n[release.source]\nindex = "'+self.policy['source']['index']+'"\nrecipe = "distro/builder/os-recipes/update-acquire.rhai"\n'
        (fixture/'distro/variants/levitate/distro.toml').write_text(text)
        def builder(*args):
            try:return run(self.builder,'--repo-root',str(fixture),*args)
            except Exception as error:
                if hasattr(error,'stderr'):raise RuntimeError(error.stderr.decode()+error.stdout.decode()) from error
                raise
        original_bundle=None
        if h['args'].adopt_baseline:
            original_bundle=base/'original-release';original_bundle.mkdir();original_source=original_bundle/'rootfs-source';original_source.mkdir();(original_bundle/'overlay').mkdir();(original_bundle/'boot').mkdir()
            for item in roots[running].iterdir():
                if item.name in ['boot','var','home','root','proc','sys','dev','run','tmp','lost+found']:(original_source/item.name).mkdir()
                elif item.is_symlink():(original_source/item.name).symlink_to(os.readlink(item))
                elif item.is_dir():shutil.copytree(item,original_source/item.name,symlinks=True)
                else:shutil.copy2(item,original_source/item.name)
            for member in ['generation.json','boot-generation','baseline','release-baseline.json']:(original_source/'usr/lib/recab'/member).unlink(missing_ok=True)
            (original_source/'usr/share/hard-b').unlink();os.link(original_source/'usr/share/hard-a',original_source/'usr/share/hard-b')
            shutil.copyfile(bundle/'.prepared-product.json',original_bundle/'.prepared-product.json');shutil.copyfile(bundle/'system-sources.sha256',original_bundle/'system-sources.sha256')
            shutil.copyfile(h['KERNEL'],original_bundle/'boot/vmlinuz');shutil.copyfile(h['initrd'],original_bundle/'boot/initramfs-installed.img')
            run(str(driver),'--pack',str(original_source),str(original_bundle/'filesystem.erofs'));run(str(driver),'--pack',str(original_bundle/'overlay'),str(original_bundle/'overlay.erofs'))
            (fixture/'distro/variants/levitate/distro.toml').write_text(text.replace('sequence = 2','sequence = 1'))
            builder('release','manifest','levitate','base',str(original_source),str(original_bundle),'--signing-key',str(self.key))
            (fixture/'distro/variants/levitate/distro.toml').write_text(text)
        builder('release','manifest','levitate','base',str(source),str(bundle),'--signing-key',str(self.key))
        builder('release','index','levitate',str(bundle),str(self.publish),'1','--signing-key',str(self.key))
        for path in self.publish.rglob('*'):
            if path.name=='.index.lock':continue
            check(stat.S_IMODE(path.stat().st_mode)==(0o755 if path.is_dir() else 0o644),'published files are not readable by the HTTP service: '+str(path))
        old_index=(self.publish/'index.json').read_bytes()
        # Publisher refuses index rollback and ignores a stale convenience sig.
        (self.publish/'index.sig').write_bytes(b'x'*64)

        edited=base/'X';edited.write_text('prepared X\n');cli(active,'edit','etc/config','--file',str(edited))
        def modify(callback):
            run('blockdev','--setrw',h['dev'][inactive]);run('mount','-o','rw',h['dev'][inactive],str(roots[inactive]));h['mounts'].add(inactive)
            try:return callback(roots[inactive])
            finally:run('umount',str(roots[inactive]));h['mounts'].remove(inactive);run('blockdev','--setro',h['dev'][inactive])
        def local_changes(root):
            (root/'etc/local-delete').unlink();(root/'etc/collision').write_text('local collision X\n');(root/'etc/mode-conflict').chmod(0o600);(root/'etc/type-tree/child').write_text('local child X\n')
            for file,row in [('passwd','alice:x:1000:1000:Alice:/home/alice:/bin/sh\n'),('shadow','alice:!:20000:0:99999:7:::\n'),('group','alice:x:1000:\n'),('gshadow','alice:*::\n')]:
                path=root/'etc'/file
                if path.exists():path.write_text(''.join(line+'\n' for line in path.read_text().splitlines() if not line.startswith('oldsvc:')).replace('root,oldsvc','root')+row)
            shadow=root/'etc/shadow';shadow.write_text(shadow.read_text().replace('root:!:20000','root:local-password-hash-secret:21000'))
        modify(local_changes)
        if original_bundle:modify(lambda root:(root/'usr/lib/recab/release-baseline.json').unlink())
        before_state=(state/'state.json').read_bytes();before_a=cli(active,'inventory','--root',str(roots[running]))
        def candidate():
            run('mount','-o','ro',h['dev'][inactive],str(roots[inactive]));h['mounts'].add(inactive)
            try:return cli(active,'inventory','--root',str(roots[inactive]))
            finally:run('umount',str(roots[inactive]));h['mounts'].remove(inactive)
        before_b=candidate();entries={p.name:p.read_bytes() for p in (roots['esp']/'loader/entries').iterdir()}
        def unchanged():
            check((state/'state.json').read_bytes()==before_state,'download/check changed slot ledger')
            check(cli(active,'inventory','--root',str(roots[running]))==before_a and candidate()==before_b,'download/check changed A or prepared X')
            check({p.name:p.read_bytes() for p in (roots['esp']/'loader/entries').iterdir()}==entries,'update changed boot eligibility')
            check(all(run('blockdev','--getro',h['dev'][s]).stdout.strip()==b'1' for s in ['A','B']),'update left writable root')
        signature_path=self.publish/'index-signatures'/(hashlib.sha256(old_index).hexdigest()+'.sig')
        good_signature=signature_path.read_bytes();signature_path.write_bytes(b'x'*64)
        cli(active,'update','check',code=21,contains='signature verification failed');unchanged()
        signature_path.write_bytes(good_signature)
        found=cli(active,'update','check');check(found['phase']=='available' and found['available']['sequence']==2,'did not discover release 2');id=found['available']['id'];unchanged()
        self.fail='boot/vmlinuz'
        cli(active,'update','download',id,code=21,contains='Recipe acquisition failed')
        interrupted=cli(active,'update','status');check(interrupted['phase']=='interrupted' and interrupted['download']['verified_files']==2,'failed download lost completed objects');unchanged()
        root_requests=sum(p.endswith('/filesystem.erofs') for p in self.requests)
        downloaded=cli(active,'update','download',id)
        check(downloaded['download']['downloaded'] and downloaded['download']['reused_files']==2,'retry did not reuse verified complete artifacts')
        check(not downloaded['download']['accounting_complete'],'interrupted producer accounting claimed complete')
        check(sum(p.endswith('/filesystem.erofs') for p in self.requests)==root_requests,'retry downloaded the rootfs again');unchanged()
        # New index is accepted; old signed index is then rejected persistently.
        builder('release','index','levitate',str(bundle),str(self.publish),'2','--signing-key',str(self.key))
        cli(active,'update','check');(self.publish/'index.json').write_bytes(old_index)
        cli(active,'update','check',code=21,contains='stale/equivocating');unchanged()
        self.server.shutdown();self.server.server_close()
        # Offline cache reuse does not attempt any acquisition.
        reused=cli(active,'update','download',id);check(reused['download']['reused_files']==4,'offline cache not reused');unchanged()
        cli(active,'stage','--manifest',str(bundle/'release.json'),code=21,contains='conflicts')
        selected=cli(active,'update','status')
        if original_bundle:
            check(selected['phase']=='blocked' and selected['merge']['conflicts'][0]['kind']=='missing_or_unsafe_baseline','missing original baseline was invented or not reported')
            p=cli(active,'status')['preparation']
            cli(active,'update','adopt-baseline','--manifest',str(original_bundle/'release.json'),'--preparation',p['id'],'--revision',str(p['revision']))
            before_b=candidate();cli(active,'stage','--manifest',str(bundle/'release.json'),code=21,contains='conflicts');selected=cli(active,'update','status')
        check(selected['phase']=='conflicted' and not selected['merge_complete'],'collisions did not stop the merge')
        check(candidate()==before_b,'conflicted merge modified captured X')
        check({c['kind'] for c in selected['merge']['conflicts']} >= {'directory_tree','account_fields','content_or_metadata'},'conflict classes incomplete')
        cli(active,'finalize',code=21);cli(active,'prepare',code=21)
        p=cli(active,'status')['preparation'];cli(active,'update','cancel','--preparation',p['id'],'--revision',str(p['revision']))
        cli(active,'prepare');check(candidate()==before_b,'cancel/reopen lost X')
        selected=cli(active,'update','apply',id);merge=selected['merge']
        ledger=(state/'state.json').read_bytes()
        cli(active,'update','reproduce','p09-native','--recipe',str(bad_recipe),'--preparation',merge['preparation'],'--revision',str(merge['revision']),code=21,contains='did not acknowledge')
        check((state/'state.json').read_bytes()==ledger and candidate()==before_b,'failed reproduction changed X or advanced preparation')
        merge=cli(active,'update','reproduce','p09-native','--recipe',str(new_recipe),'--preparation',merge['preparation'],'--revision',str(merge['revision']))
        check(all(c['kind']!='package_rebuild' for c in merge['conflicts']),'successful Recipe reproduction did not resolve dependency work')
        # Resolve a metadata collision with an explicit edited file. Other
        # resolutions choose C; a subtree choice covers its child conflicts.
        edited=base/'resolved-mode';edited.write_text('same contents\n');edited.chmod(0o600)
        identity_checked=not h['args'].merge_id_collision
        while True:
            pending=[c for c in merge['conflicts'] if c['resolution'] is None]
            check(pending,'merge lost its conflict before application')
            pending.sort(key=lambda c:(c['kind']=='directory_tree',c['path']))
            cf=pending[0];command=['update','resolve',cf['id'],'--choice','edited' if cf['path']=='etc/mode-conflict' else 'local','--preparation',merge['preparation'],'--revision',str(merge['revision'])]
            if cf['path']=='etc/mode-conflict':command+=['--file',str(edited)]
            cli(active,*command[:command.index('--revision')+1],str(merge['revision']-1),*command[command.index('--revision')+2:],code=21,contains='stale')
            if len(pending)>1:
                merge=cli(active,*command);check(merge['phase']=='conflicted','resolution applied before all conflicts were decided');continue
            if not identity_checked:
                merge=cli(active,*command)
                check(merge['phase']=='conflicted' and merge['failure'] and 'account identity' in merge['failure'],'numeric ID collision did not stop before release writes')
                def check_not_applied(root):
                    check(not (root/'etc/00-new-release').exists() and json.loads((root/'usr/lib/recab/release-baseline.json').read_text())['identity']['sequence']==1,'failed identity resolution applied or advanced release')
                modify(check_not_applied)
                edited_passwd=base/'resolved-passwd';edited_passwd.write_text('root:x:0:0:root:/root:/bin/sh\nalice:x:1000:1000:Alice:/home/alice:/bin/sh\nnewsvc:x:200:200:New:/nonexistent:/bin/sh\n');edited_passwd.chmod(0o644)
                passwd_conflict=next(c for c in merge['conflicts'] if c['path']=='etc/passwd' and c['kind']=='account_fields')
                merge=cli(active,'update','resolve',passwd_conflict['id'],'--choice','edited','--file',str(edited_passwd),'--preparation',merge['preparation'],'--revision',str(merge['revision']))
                identity_checked=True;continue
            # Kill only after observing an actual N file, proving that recovery
            # resumes a mixed tree rather than recomputing original local intent.
            child=subprocess.Popen([h['BIN'],'--json',*command],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
            deadline=time.monotonic()+60;killed=False
            try:
                observed=Path('/proc')/str(child.pid)/'root'/str(base).lstrip('/')/'staging'/('prepare-'+merge['preparation'])/'etc/00-new-release'
                while time.monotonic()<deadline:
                    if observed.exists():os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10);killed=True;break
                    if child.poll() is not None:raise RuntimeError('merge ended before interruption: '+str(child.communicate()))
                    time.sleep(.001)
                check(killed,'partial merge not observed')
            finally:
                if child.poll() is None:os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10)
            break
        cli(active,'finalize',code=21)
        # Detect corruption of a frozen private account input before more writes.
        def corrupt(root):
            path=root/'usr/lib/recab/update-work/accounts/etc/shadow';original=path.read_bytes();path.write_bytes(b'altered frozen input');return original
        frozen=modify(corrupt);cli(active,'update','recover',code=22,contains='frozen')
        modify(lambda root:(root/'usr/lib/recab/update-work/accounts/etc/shadow').write_bytes(frozen))
        # A real FIFO audit sink holds the process after its durable validated
        # checkpoint. Kill there and finish private-input cleanup to exercise
        # recovery without any of the consumed bodies still present.
        audit=state/'audit.log';saved_audit=state/'audit.saved';audit.rename(saved_audit);os.mkfifo(audit)
        child=subprocess.Popen([h['BIN'],'--json','update','recover'],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
        try:
            deadline=time.monotonic()+60;validated=False
            journal=state/('merge-'+merge['preparation']+'.json')
            while time.monotonic()<deadline:
                if journal.exists() and json.loads(journal.read_text())['phase']=='validated':validated=True;break
                if child.poll() is not None:raise RuntimeError('recovery ended before validated checkpoint: '+str(child.communicate()))
                time.sleep(.001)
            check(validated,'validated completion checkpoint not reached');os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10)
        finally:
            if child.poll() is None:os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10)
            audit.unlink();saved_audit.rename(audit)
        modify(lambda root:shutil.rmtree(root/'usr/lib/recab/update-work'))
        recovered=cli(active,'update','recover');check(recovered['phase']=='complete'  and recovered['report']['resumed'] and not recovered['report']['accounting_complete'],'partial merge recovery invented success/accounting')
        run('mount','-o','ro',h['dev'][inactive],str(roots[inactive]));h['mounts'].add(inactive)
        try:
            root=roots[inactive]
            check((root/'etc/config').read_text()=='prepared X\n' and (root/'etc/collision').read_text()=='local collision X\n','merge lost X')
            check(not (root/'etc/local-delete').exists() and not (root/'etc/release-delete').exists(),'a deletion was resurrected')
            check((root/'etc/type-tree/child').read_text()=='local child X\n' and stat.S_IMODE((root/'etc/mode-conflict').stat().st_mode)==0o600,'type/metadata resolution lost X')
            check((root/'usr/share/new-release').read_text()=='release 2\n','new release file missing')
            fstab=(root/'etc/fstab').read_text();root_entry=next(line.split() for line in fstab.splitlines() if line.split() and not line.lstrip().startswith('#') and line.split()[1]=='/')
            check('# new release mount policy comment' in fstab and root_entry[:4]==[f'PARTUUID={h["uuid"][inactive]}','/','ext4','ro'],'release fstab change was lost or copied the other root identity')
            check((root/'usr/share/hard-a').stat().st_ino==(root/'usr/share/hard-b').stat().st_ino and (root/'usr/share/hard-b').read_text()=='new hardlinked body\n','release hardlink group damaged')
            passwd=(root/'etc/passwd').read_text();shadow=(root/'etc/shadow').read_text();group=(root/'etc/group').read_text()
            check('alice:' in passwd and 'newsvc:' in passwd and 'oldsvc:' not in passwd,'structured accounts lost local/removal/release intent')
            check('root:local-password-hash-secret:21000:0:99999:14' in shadow,'account field merge lost password/aging resolution or unrelated release field')
            check('root:x:0:newsvc,root' in group and 'oldsvc' not in group,'member removal resurrected or release member lost')
            check('p09-native' in json.loads((root/'usr/lib/recipe/system.json').read_text())['packages'] and (root/'usr/lib/p09-local.so').read_bytes()[:4]==b'\x7fELF','desired package selection lost after reproduction')
            check(json.loads((root/'usr/lib/recab/release-baseline.json').read_text())['identity']['sequence']==2,'baseline not advanced')
            check(not (root/'usr/lib/recab/update-work').exists(),'private merge work leaked after completion')
        finally:run('umount',str(roots[inactive]));h['mounts'].remove(inactive)
        check(all(b'local-password-hash-secret' not in path.read_bytes() for path in state.rglob('*.json') if path.is_file()),'account secret leaked into shared state')
        check(cli(active,'inventory','--root',str(roots[running]))==before_a,'merge changed running system')
        finalized=cli(active,'finalize');check(finalized['seal']['kernel_release']==incoming_release and finalized['generation']['kernel_digest']==incoming_hash,'finalizer used running kernel instead of merged boot set');cli(active,'set-next',inactive);cli(active,'rollback')
        cli(active,'stage','--manifest',str(bundle/'release.json'));check(run('blkid','-c','/dev/null','-s','UUID','-o','value',h['dev'][inactive]).stdout==filesystem_uuid,'stage reformatted the inactive system')
        ledger=(state/'state.json').read_bytes();cli(active,'update','apply',id);check((state/'state.json').read_bytes()==ledger,'reapplying completed release changed preparation')
        changed=base/'Y';changed.write_text('later Y\n');cli(active,'edit','etc/config','--file',str(changed));cli(active,'finalize')
        return {'result':'PASS','p09':'preserving merge/conflicts/recovery','running':running,'prepared':inactive,'checks':['C plus N retained','local/release deletions','metadata and directory/type conflicts','edited/C/subtree resolutions and stale CAS','structured account fields/member removals','no account bodies in shared state','actual partial-write kill and frozen-input tamper refusal','captured-C and post-cleanup checkpoint recovery','hardlinks','Recipe reproduction bound to new release and selected receipt retained','new baseline only after complete validation','bound finalization/trial inputs (including an existing newer kernel when supplied)','idempotent apply, manifest stage without mkfs, and later Y'],'adopted_original':bool(original_bundle),'numeric_id_collision_checked':h['args'].merge_id_collision,'full_build':False,'vm_boot':False}
