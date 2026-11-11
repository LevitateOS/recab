"""Real CLI/filesystem/TLS publication checks inside the native update fixture."""
import hashlib,json,os,shutil,subprocess,time
from pathlib import Path

def exercise(o,h,bundle,active):
    from production_updates import raw,sign,digest
    run,check=o.run,o.check
    def cli(*args,ok=True,contains=None,env=None):
        q=subprocess.run([o.builder,'--repo-root',str(o.fixture),'release',*map(str,args)],env=env,capture_output=True,text=True,timeout=120)
        check((q.returncode==0)==ok,'production publisher CLI: '+str(args)+': '+q.stderr)
        if contains:check(contains in q.stderr,'missing publisher error '+contains+': '+q.stderr)
        return json.loads(q.stdout) if q.returncode==0 else None
    prepared=o.base/'publication';source=bundle/'rootfs-source';commit=run('git','-C',str(o.repo),'rev-parse','HEAD').stdout.decode().strip()
    record=cli('transport','levitate',bundle,source,prepared,commit,8*1024*1024)
    check(record['id']==digest((bundle/'release.json').read_bytes()),'transport re-signed or changed system identity')
    check(all(len(parts)<=64 and all(p['size']<=8*1024*1024 for p in parts) for parts in record['entry']['parts'].values()),'publisher part bounds missing')
    original=(prepared/'publication.json').read_bytes()
    cli('transport','levitate',bundle,source,prepared,commit,8*1024*1024)
    check((prepared/'publication.json').read_bytes()==original,'resume changed publication')
    part=next((prepared/'assets').iterdir());body=part.read_bytes();part.write_bytes(b'X'+body[1:])
    cli('transport','levitate',bundle,source,prepared,commit,8*1024*1024,ok=False,contains='part was altered');part.write_bytes(body)
    cli('transport','levitate',bundle,source,prepared,commit,1,ok=False,contains='64 transport parts')
    feed=o.publish/'meta/levitate/candidate/x86_64'
    first=cli('publish','levitate',prepared,feed,'candidate','--index-key',o.index_key)
    check(first['index_sequence']==1,'first publication sequence is not 1')
    current=json.loads((feed/'index.json').read_bytes());selection=current['releases']
    cli('publish','levitate','refresh',feed,'candidate','--index-key',o.key,ok=False,contains='Index role')
    check(json.loads((feed/'index.json').read_bytes())==current,'wrong-role publication changed index')
    # Separate real processes contend on the actual feed lock and allocate
    # distinct consecutive sequences; there is no mocked concurrency receipt.
    command=[o.builder,'--repo-root',str(o.fixture),'release','publish','levitate','refresh',str(feed),'candidate','--index-key',str(o.index_key)]
    jobs=[subprocess.Popen(command,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True) for _ in range(2)]
    sequences=[]
    for job in jobs:
        stdout,stderr=job.communicate(timeout=120);check(job.returncode==0,'concurrent refresh failed: '+stderr);sequences.append(json.loads(stdout)['index_sequence'])
    check(sorted(sequences)==[2,3],'concurrent publishers reused an index sequence')
    # Re-sign genuinely expired discovery bytes, then renew without dropping or
    # selecting any system. The root proof remains current.
    expired=json.loads((feed/'index.json').read_bytes());expired.update(issued_at=int(time.time())-10,expires_at=int(time.time())-1)
    body=raw(expired);(feed/'index.json').write_bytes(body)
    (feed/'index-signatures'/(digest(body)+'.sig')).write_bytes(sign(o,body,o.index_key))
    renewed=cli('publish','levitate','refresh',feed,'candidate','--index-key',o.index_key)
    check(renewed['index_sequence']==4 and json.loads((feed/'index.json').read_bytes())['releases']==selection,'refresh changed selections or sequence')
    # Expose the genuine generated parts through the fixture's TLS asset/CDN
    # origins and use the builder's real Recipe acquisition, not host HTTP.
    shutil.copytree(prepared/'assets',o.publish/'objects'/record['tag'])
    env={k:v for k,v in os.environ.items() if k not in ['GH_TOKEN','GITHUB_TOKEN','RECAB_LEVITATE_INDEX_KEY_BASE64','RECAB_ACORN_INDEX_KEY_BASE64']}
    compiler=Path(os.environ['RECAB_TEST_RUSTC'])
    env.update(GINUX_ROOT=active['GINUX_ROOT'],GINUX_ROOTFS=active['GINUX_ROOTFS'],RECIPE_BIN=o.recipe,
               PATH=str(compiler.parent)+':'+env['PATH'],RUSTC=str(compiler))
    verified=cli('verify-remote','levitate',prepared,env=env)
    check(verified['all_payloads_verified'] and verified['release_id']==record['id'],'public Recipe verification omitted whole payloads')
    online=cli('verify-online','levitate',feed,'candidate','--representative',env=env)
    check(online['deployed_metadata_verified'] and online['representative_assets_verified'],'Pages/asset verification incomplete')
    check(not (o.fixture/'.artifacts/ginux-export').exists(),'public verification accumulated payload exports in the shared repository')
    fake=dict(env,GH_TOKEN='fixture-token-never-forwarded')
    cli('verify-remote','levitate',prepared,env=fake,ok=False,contains='without GitHub/signing credentials')
    # Exercise the real orchestration CLI on a disposable default-branch Git
    # repository and bare remote. No GitHub API is simulated or mutated here.
    script=o.repo/'.github/scripts/recab_updates.py';metadata_repo=o.base/'metadata-repo';origin=o.base/'metadata-origin.git'
    run('git','init','--initial-branch=main',str(metadata_repo));run('git','init','--bare','--initial-branch=main',str(origin))
    def git(directory,*args):return run('git','-C',str(directory),*args)
    for name,value in [('user.name','recab native fixture'),('user.email','recab-fixture@localhost')]:git(metadata_repo,'config',name,value)
    (metadata_repo/'README').write_text('public update metadata fixture\n');(metadata_repo/'.gitignore').write_text((o.repo/'.gitignore').read_text())
    git(metadata_repo,'add','.');git(metadata_repo,'commit','-m','Initialize explicit publication fixture');git(metadata_repo,'remote','add','origin',str(origin));git(metadata_repo,'push','origin','main')
    staged=metadata_repo/'release/updates/site';staged_feed=staged/'updates/levitate/candidate/x86_64'
    shutil.copytree(feed,staged_feed)
    archive=staged/'.index-history';archive.mkdir();shutil.copyfile(feed/'index.json',archive/'old.json')
    script_env=dict(env,RECAB_DEFAULT_BRANCH='main',GITHUB_REPOSITORY='LevitateOS/LevitateOS',GITHUB_REF='refs/heads/main')
    def orchestrate(*args,ok=True,contains=None):
        q=subprocess.run(['python3',str(script),'--repo-root',str(metadata_repo),*map(str,args)],env=script_env,capture_output=True,text=True,timeout=120)
        check((q.returncode==0)==ok,'metadata orchestration: '+str(args)+': '+q.stderr)
        if contains:check(contains in q.stderr,'missing orchestration refusal '+contains+': '+q.stderr)
        return q
    exported=o.base/'pages-export';orchestrate('export-site',exported)
    check((exported/'.nojekyll').is_file() and not (exported/'.index-history').exists() and not list(exported.rglob('.index.lock')),'Pages included private writer state/history')
    (staged/'payload.part').write_bytes(body[:16]);orchestrate('export-site',o.base/'bad-pages',ok=False,contains='only public metadata');(staged/'payload.part').unlink()
    (staged/'alias.json').symlink_to(feed/'index.json');orchestrate('export-site',o.base/'bad-pages',ok=False,contains='symlinks');(staged/'alias.json').unlink()
    with (staged/'oversized.json').open('wb') as f:f.truncate(256*1024*1024+1)
    orchestrate('export-site',o.base/'bad-pages',ok=False,contains='256 MiB');(staged/'oversized.json').unlink()
    orchestrate('commit-metadata')
    check(not git(metadata_repo,'ls-files','**/.index.lock').stdout.strip(),'local writer lock was committed')
    concurrent=o.base/'concurrent-metadata';run('git','clone','--single-branch','--branch','main',str(origin),str(concurrent))
    for name,value in [('user.name','recab native fixture'),('user.email','recab-fixture@localhost')]:git(concurrent,'config',name,value)
    (concurrent/'README').write_text('another actual default-branch writer advanced\n');git(concurrent,'add','README');git(concurrent,'commit','-m','Advance the metadata base');git(concurrent,'push','origin','main')
    shutil.copyfile(feed/'index.json',staged/'index.json');before=git(origin,'rev-parse','HEAD').stdout
    orchestrate('commit-metadata',ok=False,contains='default branch advanced')
    check(git(origin,'rev-parse','HEAD').stdout==before and (staged/'index.json').is_file(),'stale-base refusal overwrote remote or lost local work')
    orchestrate('promote','levitate',record['tag'],ok=False,contains='disabled pending installed')
    # A root-authorized epoch increment recovers an exhausted/poisoned index
    # counter without changing system references. Reverting it is refused.
    proof=o.fixture/'distro/variants/levitate/release-keys.json';original_proof=proof.read_bytes()
    delegation=dict(o.delegation,version=2,index_epoch=2);body=raw(delegation)
    proof.write_bytes(raw({'document':body.decode(),'signature':list(sign(o,body,o.root_key)),'transitions':[]}))
    epoch=cli('publish','levitate','refresh',feed,'candidate','--index-key',o.index_key)
    check(epoch['index_sequence']==1 and epoch['index_epoch']==2 and json.loads((feed/'index.json').read_bytes())['releases']==selection,'root epoch recovery reset system selection')
    proof.write_bytes(original_proof)
    cli('publish','levitate','refresh',feed,'candidate','--index-key',o.index_key,ok=False,contains='rollback refused')
    return ['genuine producer transport/resume/tamper refusal','real concurrent refresh sequence allocation','expired metadata renewal without release selection',
            'generated public parts and metadata through real Recipe/Ginux HTTPS','credential-free payload verification','root epoch recovery and rollback refusal',
            'actual default-branch Git commit/push and stale-base refusal','Pages export bounds/link/payload/history exclusions','stable promotion remains disabled']
