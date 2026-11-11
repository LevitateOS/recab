"""Builder -> actual EROFS -> recab release integration inside finalization.py.
No OS build or VM boot: native ELF/initramfs/firmware fixtures remain structural.
"""
import copy,gzip,hashlib,json,os,shutil,subprocess,tomllib
from pathlib import Path

def exercise(h,active):
    check=h['check'];cli=h['cli'];base=h['base'];roots=h['roots'];state=h['state'];repo=Path(__file__).resolve().parents[3]
    def run(*args,**kwargs):
        try:return h['run'](*args,**kwargs)
        except subprocess.CalledProcessError as error:raise RuntimeError(error.stderr.decode()+error.stdout.decode()) from error
    builder=str(h['args'].release_builder.resolve(strict=True));bundle=base/'bundle';bundle.mkdir();source=bundle/'rootfs-source';source.mkdir();(bundle/'overlay').mkdir()
    for item in roots['A'].iterdir():
        if item.name in ['boot','var','home','root','proc','sys','dev','run','tmp','lost+found']:
            (source/item.name).mkdir()
        elif item.is_symlink():(source/item.name).symlink_to(os.readlink(item))
        elif item.is_dir():shutil.copytree(item,source/item.name,symlinks=True)
        else:shutil.copy2(item,source/item.name)
    (source/'tmp').chmod(0o1777)
    (source/'usr/lib/recab/generation.json').unlink(missing_ok=True);(source/'usr/lib/recab/boot-generation').unlink(missing_ok=True)
    # Genuine cached kernel/module binding; no fixture bypass in the verifier.
    modules=source/'usr/lib/modules'/h['release'];digest=hashlib.sha256()
    def walk(directory,relative=Path()):
        for item in sorted(directory.iterdir()):
            path=relative/item.name;name=os.fsencode(path);digest.update(len(name).to_bytes(8,'little'));digest.update(name)
            if item.is_symlink():
                value=os.fsencode(os.readlink(item));digest.update(b'l');digest.update(len(value).to_bytes(8,'little'));digest.update(value)
            elif item.is_dir():digest.update(b'd');walk(item,path)
            else:digest.update(b'f');digest.update(hashlib.sha256(item.read_bytes()).digest())
    walk(modules)
    (source/'usr/lib/recab/kernel.json').write_text(json.dumps({'version':1,'release':h['release'],'kernel_digest':h['kernel_hash'],'modules_digest':digest.hexdigest()}))
    owned=source/'usr/share/release-owned';owned.write_text('intended metadata is independent of staging grants\n');owned.chmod(0o600)
    linked=source/'usr/share/release-linked';os.link(owned,linked)
    manifest={'version':1,'entries':{}}
    for path in ['usr/share/release-linked','usr/share/release-owned']:
        manifest['entries'][path]={'kind':'file','uid':1234,'gid':5678,'mode':0o640,'sha256':hashlib.sha256(owned.read_bytes()).hexdigest(),'xattrs':{'user.release':b'authored'.hex()}}
    (source/'.recipe-target-metadata.json').write_text(json.dumps(manifest))
    (bundle/'.prepared-product.json').write_text(json.dumps({'product':'base-rootfs','distro_id':'levitate','rootfs_source_dir':'rootfs-source','overlay_dir':'overlay','rootfs_erofs':'filesystem.erofs','overlay_erofs':'overlay.erofs'}))
    (bundle/'system-sources.sha256').write_text(hashlib.sha256(b'real native P07 fixture input tree').hexdigest()+'\n')
    (bundle/'boot').mkdir();shutil.copyfile(h['KERNEL'],bundle/'boot/vmlinuz');shutil.copyfile(h['initrd'],bundle/'boot/initramfs-installed.img')
    driver=base/'release-input-check'
    compiler=os.environ.get('RECAB_TEST_RUSTC','rustc')
    try:run(compiler,'--edition','2024',str(repo/'tools/recab/tests/fixtures/release_input.rs'),'--extern','recab='+str(repo/'target/debug/librecab.rlib'),'--extern','distro_builder='+str(repo/'target/debug/libdistro_builder.rlib'),'--extern','levitate_recipe='+str(repo/'target/debug/liblevitate_recipe.rlib'),'-L','dependency='+str(repo/'target/debug/deps'),'-o',str(driver))
    except subprocess.CalledProcessError as error:raise RuntimeError(error.stderr.decode()) from error
    def build(repo_root=repo,key=None):
        run(str(driver),'--pack',str(source),str(bundle/'filesystem.erofs'))
        run(str(driver),'--pack',str(bundle/'overlay'),str(bundle/'overlay.erofs'))
        args=[builder,'--repo-root',str(repo_root),'release','manifest','levitate','base',str(source),str(bundle)]
        if key:args+=['--signing-key',str(key)]
        try:run(*args)
        except subprocess.CalledProcessError as error:
            raise RuntimeError(error.stderr.decode()+error.stdout.decode()) from error
    build()
    release=bundle/'release.json';original=release.read_bytes();doc=json.loads(original)
    inventory=json.loads((bundle/'system-inventory.json').read_text())
    check(inventory['entries']['usr/share/release-owned']['uid']==1234 and inventory['entries']['usr/share/release-owned']['mode']==0o640,'builder lost intended ownership/mode')
    check(inventory['entries']['usr/share/release-owned']['xattrs']['user.release']==b'authored'.hex(),'builder lost intended xattr')
    check(any(e['kind']=='hardlink' for p,e in inventory['entries'].items() if p.startswith('usr/share/release-')),'builder lost hardlink intent')
    edited=base/'pending-X';edited.write_text('pending X\n');cli(active,'edit','etc/config','--file',str(edited))
    before_state=(state/'state.json').read_bytes();before_a=cli(active,'inventory','--root',str(roots['A']))
    def b_inventory():
        run('mount','-o','ro',h['dev']['B'],str(roots['B']));h['mounts'].add('B')
        try:return cli(active,'inventory','--root',str(roots['B']))
        finally:run('umount',str(roots['B']));h['mounts'].remove('B')
    before_b=b_inventory();entries={p.name:p.read_bytes() for p in (roots['esp']/'loader/entries').iterdir()}
    cli(active,'update','check',code=21,contains='no update source configured')
    imported=cli(active,'release','import',str(release),'--trusted-local')
    check(imported['id']==hashlib.sha256(original).hexdigest(),'import did not bind exact manifest bytes')
    cache=Path(imported['cache']);check((cache/'release.json').read_bytes()==original,'cache manifest differs')
    check(imported['payload_bytes_copied']==sum(a['size'] for a in doc['artifacts'].values()),'import copied the full release more than once')
    cli(active,'release','import',str(release),code=21,contains='no distribution release key')
    # Source/key/protocol/identity failures happen without any candidate mutation.
    for field,value in [('distro','wrong'),('architecture','aarch64'),('init','openrc'),('libc','musl'),('flavour','desktop'),('channel','other')]:
        bad=copy.deepcopy(doc);bad['identity'][field]=value;release.write_text(json.dumps(bad));cli(active,'release','import',str(release),'--trusted-local',code=21)
    for field,value in [('boot_protocol',99),('state_major',99),('state_minor',99),('minimum_recab','99.0.0')]:
        bad=copy.deepcopy(doc);bad['compatibility'][field]=value;release.write_text(json.dumps(bad));cli(active,'release','import',str(release),'--trusted-local',code=21)
    bad=copy.deepcopy(doc);bad['artifacts']['kernel']['sha256']='0'*64;release.write_text(json.dumps(bad));cli(active,'release','import',str(release),'--trusted-local',code=21,contains='digest mismatch')
    bad=copy.deepcopy(doc);bad['artifacts']['rootfs']['path']='../escape';release.write_text(json.dumps(bad));cli(active,'release','import',str(release),'--trusted-local',code=21,contains='unsafe release member')
    bad=copy.deepcopy(doc);bad['artifacts']['rootfs']['path']='overlay.erofs';data=(bundle/'overlay.erofs').read_bytes();bad['artifacts']['rootfs'].update(size=len(data),sha256=hashlib.sha256(data).hexdigest());release.write_text(json.dumps(bad));cli(active,'release','import',str(release),'--trusted-local',code=21)
    release.write_bytes(original)
    # Correctly hash a genuinely mismatched installed initramfs.
    initrd=bundle/doc['artifacts']['initramfs']['path'];archive=initrd.read_bytes();raw=gzip.decompress(archive);needle=('# recab-kernel-release='+h['release']).encode();replacement=needle[:-1]+bytes([needle[-1]^1]);check(len(replacement)==len(needle),'fixture length')
    changed=gzip.compress(raw.replace(needle,replacement),mtime=0);initrd.write_bytes(changed);bad=copy.deepcopy(doc);bad['artifacts']['initramfs'].update(size=len(changed),sha256=hashlib.sha256(changed).hexdigest());release.write_text(json.dumps(bad));cli(active,'release','import',str(release),'--trusted-local',code=21,contains='initramfs')
    initrd.write_bytes(archive);release.write_bytes(original)
    # The lifetime check consumes the same real verified set after source names
    # are replaced and demonstrates the guard prevents even root writing it.
    kernel=bundle/doc['artifacts']['kernel']['path'];kernel_bytes=kernel.read_bytes();run(str(driver),str(release),env=active);kernel.write_bytes(kernel_bytes);release.write_bytes(original)
    cached_kernel=cache/doc['artifacts']['kernel']['path'];cached_kernel.chmod(0o600);cached_kernel.write_bytes(b'altered cached payload')
    cli(active,'release','import',str(release),'--trusted-local',code=21,contains='cached release artifact')
    cached_kernel.write_bytes(kernel_bytes);cached_kernel.chmod(0o400)
    # Real Ed25519 producer/consumer verification using an isolated variant,
    # with no test private key or endpoint written into the checkout.
    key=base/'release-key.der';run('openssl','genpkey','-algorithm','ED25519','-outform','DER','-out',str(key))
    public=run('openssl','pkey','-inform','DER','-in',str(key),'-pubout','-outform','DER').stdout
    check(public[:12]==bytes.fromhex('302a300506032b6570032100') and len(public)==44,'unexpected Ed25519 public key encoding');public=public[-32:].hex()
    fixture_repo=base/'signed-repo';fixture_repo.mkdir();(fixture_repo/'distro/variants/levitate').mkdir(parents=True)
    for item in repo.iterdir():
        if item.name!='distro':(fixture_repo/item.name).symlink_to(item,target_is_directory=item.is_dir())
    for item in (repo/'distro').iterdir():
        if item.name!='variants':(fixture_repo/'distro'/item.name).symlink_to(item,target_is_directory=item.is_dir())
    variant=repo/'distro/variants/levitate'
    for item in variant.iterdir():
        if item.name!='distro.toml':(fixture_repo/'distro/variants/levitate'/item.name).symlink_to(item,target_is_directory=item.is_dir())
    (fixture_repo/'distro/variants/levitate/distro.toml').write_text((variant/'distro.toml').read_text()+'\n[release.trust]\ned25519 = "'+public+'"\n')
    config=source/'usr/lib/recab/update-source.json';policy=json.loads(config.read_text());policy['trust']={'ed25519':public};config.write_text(json.dumps(policy))
    build(fixture_repo,key)
    installed=base/'signed-installed';(installed/'etc').mkdir(parents=True);(installed/'usr/lib/recab').mkdir(parents=True)
    shutil.copyfile(source/'etc/os-release',installed/'etc/os-release');shutil.copyfile(config,installed/'usr/lib/recab/update-source.json')
    signed=release.read_bytes();cli(active,'release','import',str(release),'--root',str(installed))
    release.write_bytes(signed+b'\n');cli(active,'release','import',str(release),'--root',str(installed),code=21,contains='signature verification');cli(active,'release','import',str(release),'--root',str(installed),'--trusted-local',code=21,contains='signature verification');release.write_bytes(signed)
    check((state/'state.json').read_bytes()==before_state and cli(active,'inventory','--root',str(roots['A']))==before_a and b_inventory()==before_b,'release import/refusal changed running/prepared system')
    check({p.name:p.read_bytes() for p in (roots['esp']/'loader/entries').iterdir()}==entries,'release import changed boot eligibility')
    check(all(run('blockdev','--getro',h['dev'][s]).stdout.strip()==b'1' for s in ['A','B']),'release import unprotected a root')
    return {'result':'PASS','p07':'builder/consumer','checks':['real native ELF, cached kernel/module and EROFS','intended UID/GID/mode/xattr/hardlinks','one media/online manifest','identity/protocol/digest/artifact-set refusals','full initramfs/module binding','source-path replacement cannot alter verified inputs','read-only verified input set','cache tampering refusal','real Ed25519 exact-byte signatures','configured trust cannot be bypassed by local flag','pending X/running system/ledger/boot entries unchanged'],'vm_boot':False,'full_build':False}
