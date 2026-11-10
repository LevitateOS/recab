"""Real signed feed + Recipe/Ginux HTTP + ext4/EROFS update integration.
The fixture uses native ELF programs and an existing kernel, never an OS build.
"""
import functools,hashlib,http.server,json,os,re,shutil,socket,stat,tarfile,threading
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
    def exercise(self,h,active):
        run=self.run;check=self.check;cli=h['cli'];roots=h['roots'];state=h['state'];base=self.base
        running=h['first'];inactive=h['other']
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
        # Build only a tiny native image from the existing fixture tree.
        bundle=base/'bundle';bundle.mkdir();source=bundle/'rootfs-source';source.mkdir();(bundle/'overlay').mkdir()
        for item in roots[running].iterdir():
            if item.name in ['boot','var','home','root','proc','sys','dev','run','tmp','lost+found']:(source/item.name).mkdir()
            elif item.is_symlink():(source/item.name).symlink_to(os.readlink(item))
            elif item.is_dir():shutil.copytree(item,source/item.name,symlinks=True)
            else:shutil.copy2(item,source/item.name)
        (source/'tmp').chmod(0o1777)
        for member in ['generation.json','boot-generation']:(source/'usr/lib/recab'/member).unlink(missing_ok=True)
        new_policy=dict(self.policy,sequence=2);(source/'usr/lib/recab/update-source.json').write_text(json.dumps(new_policy))
        (source/'usr/share/new-release').write_text('release 2\n')
        digest=hashlib.sha256()
        def walk(directory,relative=Path()):
            for item in sorted(directory.iterdir()):
                path=relative/item.name;name=os.fsencode(path);digest.update(len(name).to_bytes(8,'little'));digest.update(name)
                if item.is_symlink():
                    value=os.fsencode(os.readlink(item));digest.update(b'l');digest.update(len(value).to_bytes(8,'little'));digest.update(value)
                elif item.is_dir():digest.update(b'd');walk(item,path)
                else:digest.update(b'f');digest.update(hashlib.sha256(item.read_bytes()).digest())
        walk(source/'usr/lib/modules'/h['release'])
        (source/'usr/lib/recab/kernel.json').write_text(json.dumps({'version':1,'release':h['release'],'kernel_digest':h['kernel_hash'],'modules_digest':digest.hexdigest()}))
        (bundle/'.prepared-product.json').write_text(json.dumps({'product':'base-rootfs','distro_id':'levitate','rootfs_source_dir':'rootfs-source','overlay_dir':'overlay','rootfs_erofs':'filesystem.erofs','overlay_erofs':'overlay.erofs'}))
        (bundle/'system-sources.sha256').write_text(hashlib.sha256(b'genuine P08 native fixture').hexdigest()+'\n')
        (bundle/'boot').mkdir();shutil.copyfile(h['KERNEL'],bundle/'boot/vmlinuz');shutil.copyfile(h['initrd'],bundle/'boot/initramfs-installed.img')
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
        builder('release','manifest','levitate','base',str(source),str(bundle),'--signing-key',str(self.key))
        builder('release','index','levitate',str(bundle),str(self.publish),'1','--signing-key',str(self.key))
        for path in self.publish.rglob('*'):
            if path.name=='.index.lock':continue
            check(stat.S_IMODE(path.stat().st_mode)==(0o755 if path.is_dir() else 0o644),'published files are not readable by the HTTP service: '+str(path))
        old_index=(self.publish/'index.json').read_bytes()
        # Publisher refuses index rollback and ignores a stale convenience sig.
        (self.publish/'index.sig').write_bytes(b'x'*64)

        edited=base/'X';edited.write_text('prepared X\n');cli(active,'edit','etc/config','--file',str(edited))
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
        selected=cli(active,'update','apply',id)
        check(selected['phase']=='awaiting_merge' and not selected['merge_complete'],'apply falsely claimed completed P09 merge')
        check(candidate()==before_b and cli(active,'inventory','--root',str(roots[running]))==before_a,'offline selection lost X or changed A')
        p=cli(active,'status')['preparation'];cli(active,'finalize',code=21)
        cli(active,'update','apply',id,'--preparation',p['id'],'--revision',str(p['revision']))
        cli(active,'update','cancel','--preparation',p['id'],'--revision',str(p['revision']-1),code=21,contains='stale')
        cli(active,'update','cancel','--preparation',p['id'],'--revision',str(p['revision']))
        cli(active,'prepare');check(candidate()==before_b,'cancel/reopen lost X')
        authorities=list((state/'updates/recipe').rglob('authority.json'));check(authorities,'failed producer diagnostics were not retained')
        for path in authorities:
            authority=json.loads(path.read_text());check(set(authority['declaration']['needs']['inputs'])=={'request'},'producer received system trees')
            check(str(roots[running]) not in authority['config'] and str(roots[inactive]) not in authority['config'],'producer received a running/prepared mount')
        return {'result':'PASS','p08':'Recipe signed local HTTP updates','running':running,'prepared':inactive,'checks':['real signed builder feed and EROFS validation','real Ginux network acquisition','prepared X/running system/ledger/boot eligibility unchanged','disconnect/restart reuses completed artifacts','bad index signature and signed index rollback refused','offline verified cache reuse','offline apply pins inputs pending P09 merge','stale cancellation refusal and X-preserving cancel/reopen','producer sees only request file'],'full_build':False,'vm_boot':False}
