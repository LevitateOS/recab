#!/usr/bin/env python3
"""Actual offline release-engineering CLI integration using an existing bundle.
Consumes its real produced manifest/payloads, generates disposable external keys,
authorizes roles and verifies staged feeds. No builds, network or VM observations.
"""
import argparse,copy,hashlib,json,os,re,shutil,subprocess,tempfile,time
from pathlib import Path

def exercise(builder,repo,manifest_path):
    def run(*args,ok=True,contains=None):
        q=subprocess.run(args,capture_output=True,text=True,timeout=60)
        if (q.returncode==0)!=ok or (contains and contains not in q.stderr):
            raise RuntimeError(str(args)+': '+q.stdout+q.stderr)
        return q
    def sha(raw):return hashlib.sha256(raw).hexdigest()
    def raw(value):return json.dumps(value,separators=(',',':')).encode()
    manifest_bytes=manifest_path.read_bytes();manifest=json.loads(manifest_bytes);identity=manifest['identity']
    with tempfile.TemporaryDirectory(prefix='recab-p14-metadata-') as directory:
        base=Path(directory);fixture=base/'repo';fixture.mkdir();variant=fixture/'distro/variants/levitate';variant.mkdir(parents=True)
        for item in repo.iterdir():
            if item.name!='distro':(fixture/item.name).symlink_to(item,target_is_directory=item.is_dir())
        for item in (repo/'distro').iterdir():
            if item.name!='variants':(fixture/'distro'/item.name).symlink_to(item,target_is_directory=item.is_dir())
        for item in (repo/'distro/variants/levitate').iterdir():
            if item.name!='distro.toml':(variant/item.name).symlink_to(item,target_is_directory=item.is_dir())
        keys={};public={}
        for role in ['root','release','index','replacement','successor']:
            key=base/(role+'.pk8');keys[role]=key
            run('openssl','genpkey','-algorithm','ED25519','-outform','DER','-out',str(key));key.chmod(0o600)
            public[role]=subprocess.run(['openssl','pkey','-inform','DER','-in',str(key),'-pubout','-outform','DER'],check=True,capture_output=True).stdout[-32:].hex()
        lineage=sha(bytes.fromhex(public['root']))
        prefix='https://github.com/LevitateOS/LevitateOS/releases/download/'
        text=(repo/'distro/variants/levitate/distro.toml').read_text()
        text=re.sub(r'(?m)^channel = .*$', 'channel = '+json.dumps(identity['channel']),text)
        text=re.sub(r'(?m)^init = .*$', 'init = '+json.dumps(identity['init']),text)
        text=re.sub(r'(?m)^libc = .*$', 'libc = '+json.dumps(identity['libc']),text)
        text+='''
[release.source]
index = "https://levitateos.github.io/LevitateOS/updates/levitate/candidate/x86_64/index.json"
recipe = "distro/builder/os-recipes/update-acquire.rhai"
[release.production]
schema_version = 2
audience = "candidate"
metadata_base = "https://levitateos.github.io/LevitateOS/updates/"
asset_prefix = "'''+prefix+'''"
redirect_origins = ["https://release-assets.githubusercontent.com"]
[release.production.root]
lineage = "'''+lineage+'''"
version = 1
[release.production.root.key]
ed25519 = "'''+public['root']+'''"
'''
        (variant/'distro.toml').write_text(text)
        feed=base/'feed';feed.mkdir();document=base/'keys.json';proof=feed/'release-keys.json'
        now=int(time.time())
        delegation={'schema_version':1,'version':1,'root_version':1,'index_epoch':1,'distro':identity['distro'],'channel':identity['channel'],
            'issued_at':now-60,'expires_at':now+86400,'release_keys':[{'ed25519':public['release']}],
            'index_keys':[{'ed25519':public['index']}],'revoked':[]}
        document.write_bytes(raw(delegation))
        def cli(*args,**kw):return run(str(builder),'--repo-root',str(fixture),'release',*map(str,args),**kw)
        cli('authorize','levitate',document,proof,'--root-key',keys['root'])
        original_proof=proof.read_bytes()
        release_id=sha(manifest_bytes);relative='releases/'+release_id
        release=feed/relative;release.mkdir(parents=True);(release/'release.json').write_bytes(manifest_bytes)
        def sign_bytes(value,role):
            input=base/'sign-input';output=base/'signature';input.write_bytes(value)
            run('openssl','pkeyutl','-sign','-rawin','-keyform','DER','-inkey',str(keys[role]),'-in',str(input),'-out',str(output))
            return output.read_bytes()
        signature=sign_bytes(manifest_bytes,'release');(release/'release.sig').write_bytes(signature)
        parts={}
        for artifact in manifest['artifacts'].values():
            source=manifest_path.parent/artifact['path'];offset=0;items=[]
            with source.open('rb') as f:
                while chunk:=f.read(1024*1024*1024):
                    digest=sha(chunk);items.append({'offset':offset,'size':len(chunk),'sha256':digest,'url':prefix+release_id+'/'+digest+'.part'});offset+=len(chunk)
            if offset!=artifact['size']:raise RuntimeError('existing payload size mismatch')
            parts[artifact['path']]=items
        entry={'release':{'identity':identity,'manifest':{'path':relative+'/release.json','sha256':release_id,'size':len(manifest_bytes)},
            'signature':{'path':relative+'/release.sig','sha256':sha(signature),'size':64}},'parts':parts}
        index={'schema_version':2,'sequence':1,'index_epoch':1,'channel':identity['channel'],'audience':'candidate','issued_at':now-60,'expires_at':now+3600,'releases':[entry]}
        def publish(value=index,role='index'):
            body=raw(value);(feed/'index.json').write_bytes(body);signatures=feed/'index-signatures';signatures.mkdir(exist_ok=True)
            (signatures/(sha(body)+'.sig')).write_bytes(sign_bytes(body,role))
        def verify(**kw):return cli('verify-feed','levitate',feed,**kw)
        publish();result=json.loads(verify().stdout)
        if result['release_ids']!=[release_id] or result['payloads_verified'] or result['production_enabled']:raise RuntimeError('metadata CLI overstated validation')
        publish(role='release');verify(ok=False,contains='Index role');publish()
        wrong=sign_bytes(manifest_bytes,'index');(release/'release.sig').write_bytes(wrong)
        changed=copy.deepcopy(index);changed['releases'][0]['release']['signature']['sha256']=sha(wrong)
        publish(changed);verify(ok=False,contains='Release role');(release/'release.sig').write_bytes(signature);publish()
        for field,value,message in [('audience','stable','audience mismatch'),('expires_at',now-1,'expired'),('issued_at',now+7200,'validity window')]:
            changed=copy.deepcopy(index);changed[field]=value
            if field=='expires_at':changed['issued_at']=now-3600
            publish(changed);verify(ok=False,contains=message)
        changed=copy.deepcopy(index);changed['issued_at']=now+600;changed['expires_at']=now+3600;publish(changed);verify(ok=False,contains='future')
        changed=copy.deepcopy(index);next(iter(changed['releases'][0]['parts'].values()))[0]['offset']=1
        publish(changed);verify(ok=False,contains='non-contiguous')
        changed=copy.deepcopy(index);next(iter(changed['releases'][0]['parts'].values()))[0]['url']='https://github.com/unapproved/repo/releases/download/x/file'
        publish(changed);verify(ok=False,contains='publication prefix');publish()
        bad=copy.deepcopy(delegation);bad['version']=2;bad['release_keys']=[{'ed25519':public['replacement']}];bad['revoked']=[sha(bytes.fromhex(public['release']))]
        document.write_bytes(raw(bad));cli('authorize','levitate',document,proof,'--root-key',keys['root']);verify(ok=False,contains='Release role')
        proof.write_bytes(original_proof)
        cli('authorize','levitate',document,proof,'--root-key',keys['index'],ok=False,contains='signing key differs')
        if proof.read_bytes()!=original_proof:raise RuntimeError('wrong root key replaced public authorization')
        keys['root'].chmod(0o644)
        cli('authorize','levitate',document,proof,'--root-key',keys['root'],ok=False,contains='group or other')
        keys['root'].chmod(0o600)
        expired=copy.deepcopy(delegation);expired.update(version=2,issued_at=now-3600,expires_at=now-1);document.write_bytes(raw(expired))
        cli('authorize','levitate',document,proof,'--root-key',keys['root'],ok=False,contains='expired')
        # A root can renew expired signed authorization without erasing history.
        expired_body=raw(expired);archived={'document':expired_body.decode(),'signature':list(sign_bytes(expired_body,'root')),'transitions':[]}
        proof.write_bytes(raw(archived));renewed=copy.deepcopy(delegation);renewed['version']=3;document.write_bytes(raw(renewed))
        cli('authorize','levitate',document,proof,'--root-key',keys['root']);verify()
        proof.write_bytes(original_proof)
        changed=copy.deepcopy(index);changed['index_epoch']=2;publish(changed);verify(ok=False,contains='delegation channel differ');publish()
        epoch=copy.deepcopy(delegation);epoch.update(version=2,index_epoch=2);document.write_bytes(raw(epoch))
        cli('authorize','levitate',document,proof,'--root-key',keys['root'])
        changed=copy.deepcopy(index);changed['index_epoch']=2;publish(changed);verify()
        prior=proof.read_bytes();epoch.update(version=3,index_epoch=1);document.write_bytes(raw(epoch))
        cli('authorize','levitate',document,proof,'--root-key',keys['root'],ok=False,contains='epoch rollback refused')
        if proof.read_bytes()!=prior:raise RuntimeError('epoch rollback replaced public authorization')
        proof.write_bytes(original_proof);publish()
        # Real dual signatures prove root succession, not a replacement public key.
        new=copy.deepcopy(delegation);new.update(version=2,root_version=2);body=raw(new)
        message=b'recab-root-transition-v1\n'+raw([lineage,2,{'ed25519':public['root']},{'ed25519':public['successor']}])
        transition={'version':2,'previous':{'ed25519':public['root']},'successor':{'ed25519':public['successor']},
            'previous_signature':list(sign_bytes(message,'root')),'successor_signature':list(sign_bytes(message,'successor'))}
        chain={'document':body.decode(),'signature':list(sign_bytes(body,'successor')),'transitions':[transition]}
        proof.write_bytes(raw(chain));verify()
        chain['transitions'][0]['successor_signature'][0]^=1;proof.write_bytes(raw(chain));verify(ok=False,contains='signature verification failed')
        proof.write_bytes(original_proof);verify()
        return {'result':'PASS','scope':'real release-engineering CLI and cryptographic metadata; no payload validation/network/VM/production enablement',
            'release':release_id,'checks':['root authorization','signing-role separation','revocation','expiry and clock errors','audience binding','transport range and URL confinement','wrong-root refusal without replacement','dual-signed root rotation','expired authority renewal and private-key permissions','root-authorized index epoch binding']}

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--builder',required=True,type=Path);parser.add_argument('--repo',required=True,type=Path);parser.add_argument('--manifest',required=True,type=Path);args=parser.parse_args()
    print(json.dumps(exercise(args.builder.resolve(strict=True),args.repo.resolve(strict=True),args.manifest.resolve(strict=True)),indent=2))
