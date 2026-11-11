"""Delegated HTTPS transport on finalization.py's real native disk/Recipe fixture.
All HTTP(S), TLS, signing, producer execution, cache assembly and merges are real.
Only firmware/boot identities are fixture observations; no VM or public service.
"""
import copy,email.utils,hashlib,http.server,json,os,shutil,signal,socket,ssl,subprocess,threading,time
from pathlib import Path

def raw(value):return json.dumps(value,separators=(',',':')).encode()
def digest(value):return hashlib.sha256(value).hexdigest()
def sign(owner,data,key):
    file=owner.base/'sign-input';out=owner.base/'signature';file.write_bytes(data)
    owner.run('openssl','pkeyutl','-sign','-rawin','-keyform','DER','-inkey',str(key),'-in',str(file),'-out',str(out))
    return out.read_bytes()
def key(owner,name):
    path=owner.base/(name+'.pk8');owner.run('openssl','genpkey','-algorithm','ED25519','-outform','DER','-out',str(path));path.chmod(0o600)
    public=owner.run('openssl','pkey','-inform','DER','-in',str(path),'-pubout','-outform','DER').stdout[-32:].hex()
    return path,public
def configure(o,h):
    o.server.shutdown();o.server.server_close()
    o.root_key,root_public=key(o,'root');o.index_key,index_public=key(o,'index');o.replacement,replacement_public=key(o,'replacement')
    now=int(time.time())
    o.delegation={'schema_version':1,'version':1,'root_version':1,'index_epoch':1,'distro':'levitateos','channel':'development',
        'issued_at':now-60,'expires_at':now+86400,'release_keys':[{'ed25519':o.public}],'index_keys':[{'ed25519':index_public}],'revoked':[]}
    document=raw(o.delegation);o.authority_raw=raw({'document':document.decode(),'signature':list(sign(o,document,o.root_key)),'transitions':[]})
    o.replacement_public=replacement_public
    ca=o.base/'ca.pem';ca_key=o.base/'ca.key';csr=o.base/'server.csr';cert=o.base/'server.pem';server_key=o.base/'server.key';ext=o.base/'server.ext'
    o.run('openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=recab native fixture CA','-keyout',str(ca_key),'-out',str(ca))
    o.run('openssl','req','-new','-newkey','rsa:2048','-nodes','-subj','/CN=127.0.0.1','-keyout',str(server_key),'-out',str(csr))
    ext.write_text('subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n')
    o.run('openssl','x509','-req','-in',str(csr),'-CA',str(ca),'-CAkey',str(ca_key),'-CAcreateserial','-days','1','-extfile',str(ext),'-out',str(cert))
    o.certificate=ca;o.fail=None;o.pause=None;o.throttle=None;o.bad_redirect=False;o.requests=[];o.servers=[]
    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self,*args,**kw):super().__init__(*args,directory=str(o.publish),**kw)
        def log_message(self,*args):pass
        def do_GET(self):
            o.requests.append((self.server.kind,self.path))
            if self.server.kind=='cdn' and o.throttle and self.path==o.throttle['path']:
                throttle=o.throttle;o.throttle=None
                self.send_response(429);self.send_header('Retry-After',throttle['after']);self.send_header('Content-Length','0');self.end_headers();return
            if self.server.kind=='cdn' and o.pause and self.path==o.pause['path']:
                pause=o.pause;o.pause=None;pause['entered'].set()
                pause['resume'].wait(60)
                return
            if self.server.kind=='asset':
                target='http://127.0.0.1:'+str(o.cdn.server_port)+'/forbidden' if o.bad_redirect else o.cdn_origin+self.path
                self.send_response(302);self.send_header('Location',target);self.send_header('Content-Length','0');self.end_headers();return
            if o.fail and self.path==o.fail:
                o.fail=None;data=Path(self.translate_path(self.path)).read_bytes()
                self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers()
                self.wfile.write(data[:max(1,len(data)//2)]);self.wfile.flush();self.connection.shutdown(socket.SHUT_RDWR);return
            super().do_GET()
    for kind in ['metadata','asset','cdn']:
        server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);server.kind=kind
        context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(cert,server_key);server.socket=context.wrap_socket(server.socket,server_side=True);o.tls_context=context
        o.servers.append(server);threading.Thread(target=server.serve_forever,daemon=True).start()
        setattr(o,kind,server)
    o.server=o.metadata;o.endpoint='127.0.0.1:'+str(o.server.server_port);meta='https://'+o.endpoint
    asset='https://127.0.0.1:'+str(o.asset.server_port);o.cdn_origin='https://127.0.0.1:'+str(o.cdn.server_port)
    o.endpoints=[o.endpoint,'127.0.0.1:'+str(o.asset.server_port),'127.0.0.1:'+str(o.cdn.server_port)]
    o.policy={'channel':'development','sequence':1,'source':{'index':meta+'/meta/index.json','recipe':'usr/share/recab/update-acquire.rhai'},'trust':None,
        'production':{'schema_version':2,'root':{'lineage':digest(bytes.fromhex(root_public)),'version':1,'key':{'ed25519':root_public}},
            'audience':'candidate','metadata_base':meta+'/meta/','asset_prefix':asset+'/objects/','redirect_origins':[o.cdn_origin]}}
    (o.publish/'meta').mkdir();(o.publish/'objects').mkdir();(o.publish/'meta/release-keys.json').write_bytes(o.authority_raw)

def variant_policy(o):
    p=o.policy['production'];r=p['root'];s=o.policy['source']
    return '\n[release.source]\nindex = '+json.dumps(s['index'])+'\nrecipe = "distro/builder/os-recipes/update-acquire.rhai"\n'+\
        '[release.production]\nschema_version = 2\naudience = "candidate"\nmetadata_base = '+json.dumps(p['metadata_base'])+'\nasset_prefix = '+json.dumps(p['asset_prefix'])+'\nredirect_origins = '+json.dumps(p['redirect_origins'])+'\n'+\
        '[release.production.root]\nversion = 1\nlineage = '+json.dumps(r['lineage'])+'\n[release.production.root.key]\ned25519 = '+json.dumps(r['key']['ed25519'])+'\n'

def exercise(o,h,active,bundle):
    cli,check,run=h['cli'],h['check'],h['run'];roots=h['roots'];running=h['first'];inactive=h['other']
    from production_publication import exercise as publication_checks
    publishing_checks=publication_checks(o,h,bundle,active)
    manifest_raw=(bundle/'release.json').read_bytes();manifest=json.loads(manifest_raw);release=digest(manifest_raw)
    path=o.publish/'meta/releases'/release;path.mkdir(parents=True)
    for name in ['release.json','release.sig']:shutil.copyfile(bundle/name,path/name)
    parts={}
    for artifact in manifest['artifacts'].values():
        body=(bundle/artifact['path']).read_bytes();step=(len(body)+2)//3 if artifact==manifest['artifacts']['rootfs'] else len(body)
        members=[]
        for offset in range(0,len(body),step):
            chunk=body[offset:offset+step];sha=digest(chunk);(o.publish/'objects'/(sha+'.part')).write_bytes(chunk)
            members.append({'offset':offset,'size':len(chunk),'sha256':sha,'url':o.policy['production']['asset_prefix']+sha+'.part'})
        parts[artifact['path']]=members
    signature=(path/'release.sig').read_bytes()
    entry={'release':{'identity':manifest['identity'],'manifest':{'path':'releases/'+release+'/release.json','sha256':release,'size':len(manifest_raw)},
        'signature':{'path':'releases/'+release+'/release.sig','sha256':digest(signature),'size':64}},'parts':parts}
    sequence=0
    def publish(changed=None,expires=None,signer=None):
        nonlocal sequence
        sequence+=1;now=int(time.time())
        index={'schema_version':2,'sequence':sequence,'index_epoch':o.delegation['index_epoch'],'channel':'development','audience':'candidate',
            'issued_at':now-60,'expires_at':expires or now+3600,'releases':[changed or entry]}
        body=raw(index);(o.publish/'meta/index.json').write_bytes(body);signatures=o.publish/'meta/index-signatures';signatures.mkdir(exist_ok=True)
        (signatures/(digest(body)+'.sig')).write_bytes(sign(o,body,signer or o.index_key))
        return index
    def authority():
        body=raw(o.delegation);(o.publish/'meta/release-keys.json').write_bytes(raw({'document':body.decode(),'signature':list(sign(o,body,o.root_key)),'transitions':[]}))
    def snapshot():
        return ((h['state']/'state.json').read_bytes(),cli(active,'inventory','--root',str(roots[running])),
            {p.name:p.read_bytes() for p in (h['esp']/'loader/entries').iterdir()})
    def protected(before):check(snapshot()==before,'production acquisition changed ledger, active root or boot entries')
    edited=o.base/'X';edited.write_text('prepared production X\n');cli(active,'edit','etc/config','--file',str(edited))
    before=snapshot();publish(signer=o.key);cli(active,'update','check',code=21,contains='Index role');protected(before)
    bad=copy.deepcopy(entry);next(iter(bad['parts'].values()))[0]['url']='https://unapproved.invalid/file'
    publish(bad);cli(active,'update','check',code=21,contains='publication prefix');protected(before)
    # Individually authentic parts still have to match the release-role-signed
    # whole artifact. An index signer cannot substitute a system image.
    bad=copy.deepcopy(entry);part=bad['parts'][manifest['artifacts']['rootfs']['path']][0]
    body=bytearray((o.publish/'objects'/(part['sha256']+'.part')).read_bytes());body[0]^=1
    part['sha256']=digest(body);part['url']=o.policy['production']['asset_prefix']+part['sha256']+'.part'
    (o.publish/'objects'/(part['sha256']+'.part')).write_bytes(body)
    publish(bad);cli(active,'update','check');cli(active,'update','download',code=21,contains='assembled artifact differs');protected(before)
    shutil.rmtree(h['state']/'updates/assembly'/release)
    publish();found=cli(active,'update','check');check(found['available']['id']==release,'delegated discovery selected another release');protected(before)
    # Predictable shared-state refusal without running a producer.
    pressure=roots['state']/'production-space-pressure';space=os.statvfs(roots['state'])
    try:
        with pressure.open('wb') as f:os.posix_fallocate(f.fileno(),0,max(1,space.f_bavail*space.f_frsize-8*1024*1024))
        cli(active,'update','download',code=21,contains='update workspace')
    finally:pressure.unlink(missing_ok=True)
    o.bad_redirect=True;cli(active,'update','download',code=21,contains='Recipe acquisition failed');o.bad_redirect=False
    check(not any(path=='/forbidden' for _,path in o.requests),'a forbidden HTTPS downgrade was followed');protected(before)
    root_parts=parts[manifest['artifacts']['rootfs']['path']];first='/objects/'+root_parts[0]['sha256']+'.part';second='/objects/'+root_parts[1]['sha256']+'.part'
    requests=lambda path:sum(kind=='cdn' and seen==path for kind,seen in o.requests)
    count=requests(first);o.throttle={'path':first,'after':email.utils.formatdate(time.time()+60,usegmt=True)}
    cli(active,'update','download',code=21,contains='non-numeric Retry-After');protected(before)
    check(requests(first)==count+1,'Retry-After date was ignored and the origin was retried early')
    count=requests(first);o.throttle={'path':first,'after':'1'}
    o.fail=second;cli(active,'update','download',code=21,contains='Recipe acquisition failed');protected(before)
    check(requests(first)==count+2,'bounded numeric Retry-After did not retry the real HTTPS origin')
    partials=[p for p in (h['state']/'updates/assembly'/release).iterdir() if p.suffix!='.json']
    partial=max(partials,key=lambda p:p.stat().st_size)
    with partial.open('r+b') as f:b=f.read(1);f.seek(0);f.write(bytes([b[0]^1]));f.flush();os.fsync(f.fileno())
    o.fail=second;cli(active,'update','download',code=21,contains='Recipe acquisition failed')
    check(not list((h['state']/'updates/recipe').iterdir()),'failed acquisition retained an unverified request tree')
    first_requests=sum(kind=='asset' and path==first for kind,path in o.requests)
    # Kill a real in-progress acquisition after an already durable first part.
    # Recipe's inherited request lease must die with the process group, and the
    # next command reclaims only the abandoned request (not verified assembly).
    pause={'path':second,'entered':threading.Event(),'resume':threading.Event()};o.pause=pause
    with (o.base/'cancelled-download.log').open('w+b') as log:
        process=subprocess.Popen([h['BIN'],'--json','update','download'],env=active,stdout=log,stderr=log,start_new_session=True)
        try:
            check(pause['entered'].wait(60),'real download did not reach its cancellation boundary')
            os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=10)
        finally:
            pause['resume'].set();o.pause=None
            if process.poll() is None:os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=10)
    check(list((h['state']/'updates/recipe').glob('request-*')),'cancelled request did not exercise stale scratch recovery')
    cli(active,'update','check')
    check(not list((h['state']/'updates/recipe').iterdir()),'retry did not reclaim abandoned acquisition scratch')
    # Host TMPDIR still supplies small verifier mountpoints. The actual Recipe
    # payload/runtime scratch must instead use recab's shared-state pool.
    host_tmp=o.base/'host-tmp';host_tmp.mkdir()
    downloaded=cli(dict(active,TMPDIR=str(host_tmp)),'update','download');protected(before)
    check(not list(host_tmp.glob('recipe-scratch-*')) and list((h['state']/'updates/recipe-tmp').glob('recipe-scratch-*')),
          'Recipe acquisition ignored shared-state scratch selection')
    check(downloaded['download']['downloaded'] and downloaded['download']['verified_files']==4,'production assembly did not publish complete release')
    check(downloaded['download']['reused_parts']>=1 and not downloaded['download']['accounting_complete'],'resume omitted verified-part reuse or invented accounting')
    check(sum(kind=='asset' and path==first for kind,path in o.requests)==first_requests,'valid first part was fetched again')
    check(downloaded['download']['assembly_write_bytes']>0,'assembly bytes were not reported')
    # A signed expired discovery index cannot fetch new content, but an already
    # downloaded release remains usable offline. Use actual elapsed UTC time.
    expires=int(time.time())+15;publish(expires=expires);cli(active,'update','check')
    while time.time()<=expires:time.sleep(.2)
    cli(active,'update','check',code=21,contains='expired')
    for server in o.servers:server.shutdown();server.server_close()
    cached=cli(active,'update','download',release);check(cached['download']['reused_files']==4,'offline cache was not reverified/reused');protected(before)
    applied=cli(active,'update','apply',release)
    for _ in range(32):
        status=cli(active,'update','status')
        if status['merge_complete']:break
        conflicts=status['merge']['conflicts'];pending=[c for c in conflicts if c['resolution'] is None]
        if pending:
            c=pending[0];check(c['kind']=='package_rebuild' and c['path']=='p09-native','unexpected production merge conflict: '+str(c))
            recipe=o.base/'native-release.rhai';cli(active,'update','reproduce','p09-native','--recipe',str(recipe),
                '--preparation',status['merge']['preparation'],'--revision',str(status['merge']['revision']))
        else:cli(active,'update','recover','--preparation',status['merge']['preparation'],'--revision',str(status['merge']['revision']))
    else:raise RuntimeError('production merge did not converge')
    run('mount','-o','ro',h['dev'][inactive],str(roots[inactive]));h['mounts'].add(inactive)
    try:
        check((roots[inactive]/'etc/config').read_text()=='prepared production X\n','production merge lost X')
        check((roots[inactive]/'usr/share/new-release').read_text()=='release 2\n','assembled release was not applied')
    finally:run('umount',str(roots[inactive]));h['mounts'].remove(inactive)
    final=cli(active,'finalize');check(final['generation']['id'],'production candidate did not seal')
    # A newly learned root revocation must block use of the completed cache.
    old=o.delegation['release_keys'][0]['ed25519'];o.delegation.update(version=2,release_keys=[{'ed25519':o.replacement_public}],revoked=[digest(bytes.fromhex(old))])
    authority()
    # Restart just metadata on the same approved port for the revocation fetch.
    o.server=http.server.ThreadingHTTPServer(('127.0.0.1',o.metadata.server_port),o.metadata.RequestHandlerClass);o.server.kind='metadata'
    o.server.socket=o.tls_context.wrap_socket(o.server.socket,server_side=True)
    threading.Thread(target=o.server.serve_forever,daemon=True).start()
    try:
        publish();cli(active,'update','check',code=21,contains='Release role')
        cli(active,'update','download',release,code=21,contains='revoked')
        cli(active,'update','apply',release,code=21,contains='revoked')
    finally:o.server.shutdown();o.server.server_close()
    return {'result':'PASS','running':running,'prepared':inactive,'production_enabled':False,'full_build':False,'vm_boot':False,
        'checks':['real Recipe/Ginux HTTPS with explicit test CA','root/index/release role separation','signed transport prefix checks','TLS redirect confinement before request',
            'shared-state preflight and disk-backed acquisition scratch','whole-artifact substitution refusal','real truncated-part retry','tampered partial rehash','valid-part reuse','whole-release verification and assembly accounting',
            'real acquisition group cancellation and abandoned-request reclamation',
            'bounded HTTP 429 delta-seconds retry and HTTP-date deferral without early retry',
            'expired index refusal and offline verified cache','prepared X and active/boot preservation','offline preserving apply/reproduction/finalization','known revocation blocks cached reuse'],
        'publishing_checks':publishing_checks,'download':downloaded['download'],'scope':'native actual storage/producers; no public production service or VM boot'}
