"""Real Recipe/Ginux publications within finalization.py's disposable GPT disk.
No simulated producer: pass --changes /absolute/path/to/recipe and GINUX_* env.
"""
import hashlib,json,os,re,shutil,signal,subprocess,tarfile,time
from pathlib import Path

class Changes:
    def __init__(self, h):
        self.h=h; self.base=h['base']; self.check=h['check']; self.cli=h['cli']; self.run=h['run']; self.roots=h['roots']; self.dev=h['dev']
        self.recipe=str(h['args'].changes.resolve(strict=True)); self.source=self.base/'service.rhai'; self.lib=self.base/'library.rhai'
        self.env=dict(h['env'],RECIPE_RECAB_BIN=h['BIN'],RECAB_RECIPE_BIN=self.recipe,TMPDIR=str(self.base/'tmp'))
        Path(self.env['TMPDIR']).mkdir()
        # A tiny genuine native runtime suffices for these filesystem-only
        # recipes. Use the real worker and loader/libraries, not a fake producer
        # or a compiler image containing unrelated toolchains.
        worker=self.base/'recipe-worker';shutil.copyfile(self.recipe,worker)
        self.run('strip',str(worker));worker.chmod(0o755)
        runtime=self.base/'runtime';(runtime/'etc').mkdir(parents=True)
        for name in re.findall(r'(/[^\s()]+)',self.run('ldd',str(worker)).stdout.decode()):
            source=Path(name)
            if source.is_file():
                dest=runtime/name.lstrip('/');dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(source,dest);dest.chmod(0o755)
        archive=self.base/'native-runtime.tar'
        with tarfile.open(archive,'w') as tar:
            for item in sorted(runtime.iterdir()):tar.add(item,arcname=item.name)
        self.env.update(GINUX_ROOTFS=str(archive),GINUX_ROOT='oci:recab-native-fixture@sha256:'+hashlib.sha256(archive.read_bytes()).hexdigest(),RECIPE_WORKER_BIN=str(worker))
        self.reports=[]
    def producer(self,path,name,version,destination,contents,dependency=None,fail=False,package=False):
        # The worker checks its real output before and after install.
        root='/out/package' if package else '/out/system'; output='package' if package else 'system'
        deps='let deps = ["library"];\n' if dependency else ''
        path.write_text('const RECIPE_API=2;\nconst NEEDS=#{outputs:#{'+json.dumps(output)+':"directory"}};\n'+deps+
            'let ctx=#{name:'+json.dumps(name)+',version:'+json.dumps(version)+',root:'+json.dumps(root)+'};\n'+
            'fn is_acquired(ctx){ready(ctx)}\nfn is_built(ctx){ready(ctx)}\n'+
            'fn is_installed(ctx){if !is_file(ctx.root+"/'+destination+'") {return needs_work(ctx,"missing payload");} ctx.outputs=#{'+json.dumps(output)+':ctx.root};ready(ctx)}\n'+
            'fn install(ctx){'+('throw "intentional producer failure";' if fail else '')+
            'mkdir(ctx.root+"/'+str(Path(destination).parent)+'");write_file(ctx.root+"/'+destination+'",'+json.dumps(contents)+');'+
            ('' if package else 'target_mode(ctx.root,"'+destination+'",0o644);target_finalize(ctx.root);')+'ctx.outputs=#{'+json.dumps(output)+':ctx.root};ctx}\n')
    def command(self,active,*argv,ok=True):
        merged=dict(active,**{k:v for k,v in self.env.items() if k.startswith(('GINUX_','RECIPE_','RECAB_RECIPE_')) or k=='TMPDIR'})
        child=subprocess.Popen([self.recipe,'--system','--json',*argv],env=merged,text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
        try:out,err=child.communicate(timeout=120)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid,signal.SIGKILL);child.communicate();raise
        q=subprocess.CompletedProcess(argv,child.returncode,out,err)
        # active inherits os.environ: explicitly restore the two tool paths.
        self.check((q.returncode==0)==ok,'Recipe '+str(argv)+': '+q.stdout+q.stderr)
        if not ok:return q
        return json.loads(q.stdout)
    def candidate(self,slot,callback):
        self.run('mount','-o','ro',self.dev[slot],str(self.roots[slot]));self.h['mounts'].add(slot)
        try:return callback(self.roots[slot])
        finally:self.run('umount',str(self.roots[slot]));self.h['mounts'].remove(slot)
    def first(self,active):
        active.update({k:v for k,v in self.env.items() if k in ('RECAB_RECIPE_BIN','RECIPE_RECAB_BIN','TMPDIR')})
        before=self.cli(active,'inventory','--root',str(self.roots['A']))
        service='usr/lib/systemd/system/p06.service';library='usr/lib/p06-library'
        self.producer(self.lib,'p06-library','1',library,'library v1\n')
        self.producer(self.source,'p06-service','1',service,'service v1\n',dependency=True)
        report=self.command(active,'install',str(self.source));self.reports.append(report)
        self.check(report['report']['payload_bytes_written']<32768,'small install rewrote system payload')
        view=self.command(active,'installed');self.check(not view['active']['packages'] and set(view['prepared']['packages'])=={'p06-service','p06-library'},'active/prepared selections not separated')
        self.check(self.command(active,'isinstalled','p06-service',ok=False).returncode!=0,'pending package claimed active')
        self.command(active,'isinstalled','p06-service','--prepared')
        self.command(active,'remove','p06-library',ok=False)
        self.candidate('B',lambda root:self.check((root/service).read_text()=='service v1\n','service missing in candidate'))
        self.producer(self.source,'p06-service','2',service,'service v2\n',dependency=True)
        report=self.command(active,'update','p06-service');self.reports.append(report)
        self.candidate('B',lambda root:self.check((root/service).read_text()=='service v2\n','update not applied'))
        # Conflicting base/admin content and failed builds publish no intent.
        state=(self.h['state']/'state.json').read_bytes()
        self.producer(self.source,'p06-service','3',service,'broken\n',dependency=True,fail=True)
        self.command(active,'update','p06-service',ok=False)
        self.check((self.h['state']/'state.json').read_bytes()==state,'failed producer changed preparation')
        conflict=self.base/'conflict.rhai';self.producer(conflict,'conflict','1','etc/config','overwrite admin\n')
        self.command(active,'install',str(conflict),ok=False)
        self.check((self.h['state']/'state.json').read_bytes()==state,'ownership refusal changed preparation')
        edited=self.base/'edit';edited.write_text('administrator service edit\n')
        self.cli(active,'edit',service,'--file',str(edited))
        self.command(active,'remove','p06-service',ok=False)
        edited.write_text('service v2\n');self.cli(active,'edit',service,'--file',str(edited))
        self.reports.append(self.command(active,'remove','p06-service'));self.reports.append(self.command(active,'remove','p06-library'))
        self.candidate('B',lambda root:self.check(not (root/service).exists() and not (root/library).exists(),'owned removal did not finish'))
        package=self.base/'profile.rhai';self.producer(package,'p06-profile','1','share/p06.txt','profile data\n',package=True)
        self.command(active,'install',str(package))
        self.candidate('B',lambda root:self.check((root/'usr/lib/recipe/profile/share/p06.txt').read_text()=='profile data\n','ordinary package did not target slot profile'))
        self.command(active,'remove','p06-profile')
        self.producer(self.source,'p06-service','4',service,'service v4\n',dependency=True)
        self.reports.append(self.command(active,'install',str(self.source)))
        edited.write_text('local X\n');self.cli(active,'edit','/etc/config','--file',str(edited))
        self.interrupt(active,edited)
        p=self.cli(active,'status')['preparation'];self.cli(active,'prepare','--preparation',p['id'],'--revision',str(p['revision']))
        self.cli(active,'edit','etc/config','--file',str(edited),'--preparation',p['id'],'--revision','0',code=21,contains='stale')
        self.check(self.cli(active,'inventory','--root',str(self.roots['A']))==before,'running A changed')
        self.candidate('B',lambda root:self.check((root/'etc/config').read_text()=='local X\n' and (root/service).read_text()=='service v4\n','reopen lost X'))
        # Check actual coordinator grants: the prepared root is never an input.
        authorities=list((self.h['state']/'recipe').rglob('authority.json'))
        self.check(bool(authorities),'no actual Ginux producer authority observed')
        for authority in authorities:
            data=json.loads(authority.read_text());self.check(not data['declaration']['needs']['inputs'],'unexpected system input selection')
            self.check(str(self.roots['A']) not in data['config'] and str(self.roots['B']) not in data['config'],'whole-system input granted to producer')
        retained=[p for p in (self.h['state']/'recipe').rglob('*') if p.is_file() and not p.is_symlink()]
        retained_bytes=sum(p.stat().st_size for p in retained)
        self.check(retained_bytes<4*1024*1024,'small changes retained whole-system exports/checkpoints')
        self.check(all(r['projection_bytes_written']<32768 for r in self.reports),'publication copied unrelated system data')
        self.check(all(r.get('invocation_io') is not None for r in self.reports),'whole invocation process I/O unmeasured')
        print(json.dumps({'p06':'forward PASS','reports':self.reports,'actual_ginux_invocations':len(authorities),'retained_bytes':retained_bytes}))
    def interrupt(self,active,edited):
        edited.write_bytes(b'R'*(1024*1024))
        child=subprocess.Popen([self.h['BIN'],'--json','edit','etc/config','--file',str(edited)],env=active,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
        pending=None;deadline=time.monotonic()+30
        try:
            while time.monotonic()<deadline:
                state=json.loads((self.h['state']/'state.json').read_text());p=state['preparation']
                if p.get('change'):
                    child.kill();child.communicate(timeout=10);pending=p;break
                if child.poll() is not None:raise RuntimeError('edit ended before interruption: '+str(child.communicate()))
                time.sleep(.001)
            self.check(pending is not None,'durable change intent not observed')
        finally:
            if child.poll() is None:os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=10)
        self.cli(active,'prepare',code=21,contains='recovery')
        self.cli(active,'finalize',code=21,contains='recovery')
        projection=self.h['state']/('change-payload-'+pending['change']['id'])/'etc/config'
        frozen=projection.read_bytes();projection.write_bytes(b'altered frozen payload')
        self.cli(active,'change','recover',code=22,contains='altered')
        projection.write_bytes(frozen)
        recovered=self.cli(active,'change','recover','--preparation',pending['id'],'--revision',str(pending['revision']))
        self.check(recovered['report']['resumed'] and not recovered['report']['accounting_complete'],'recovery invented complete write accounting')
        self.candidate('B',lambda root:self.check((root/'etc/config').read_bytes()==frozen,'interrupted edit did not recover'))
        self.check(self.run('blockdev','--getro',self.dev['B']).stdout.strip()==b'1','recovery left candidate writable')
        edited.write_text('local X\n');self.cli(active,'edit','etc/config','--file',str(edited))
    def reverse(self,active):
        active.update({k:v for k,v in self.env.items() if k in ('RECAB_RECIPE_BIN','RECIPE_RECAB_BIN','TMPDIR')})
        before=self.cli(active,'inventory','--root',str(self.roots['B']))
        self.producer(self.source,'p06-service','5','usr/lib/systemd/system/p06.service','service v5\n',dependency=True)
        self.command(active,'update','p06-service')
        self.candidate('A',lambda root:self.check((root/'etc/config').read_text()=='local X\n' and (root/'usr/lib/systemd/system/p06.service').read_text()=='service v5\n','reverse change lost X or update'))
        self.check(self.cli(active,'inventory','--root',str(self.roots['B']))==before,'running B changed')
        print(json.dumps({'p06':'reverse PASS','running_system_unchanged':True}))
