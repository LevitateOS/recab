"""Generation-bound trial CLI checks on the finalizer's real GPT filesystems.
Init command probes supply observed statuses and real stuck subprocesses; this
is process/filesystem/EFI-protocol evidence, not a PID 1 or hardware boot test.
"""
import json,os,shutil,signal,subprocess,time
from pathlib import Path
TICKET='RecabTrial-a69e7a1c-44ab-42f5-93ed-836a4c103bad'
def exercise(h,active,candidate,generation):
    cli,run,check=h['cli'],h['run'],h['check'];base=h['base'];roots=h['roots'];dev=h['dev'];mounts=h['mounts'];esp=h['esp'];state=h['state'];efi=h['efi'];proc=h['proc'];init=h['args'].init
    prefix='levitateos' if h['args'].release_builder else 'lev'
    mode=base/'assessment-mode';rebooted=base/'reboot-requested';pid=base/'health-child';openrc=base/'openrc';openrc.mkdir();levels=base/'runlevels';levels.mkdir()
    for name in ['starting','started','inactive','failed']:(openrc/name).mkdir()
    for name in ['sysinit','boot','default']:(levels/name).mkdir()
    (levels/'default/oneshot').touch();(openrc/'inactive/oneshot').touch();(openrc/'softlevel').write_text('default')
    helper=base/'health-helper';helper.write_text('''#!/usr/bin/python3
import os,sys,time,subprocess
from pathlib import Path
base=Path(__file__).parent
mode=(base/'assessment-mode').read_text()
if 'reboot' in sys.argv or sys.argv[0].endswith('reboot-helper'):
    (base/'reboot-requested').write_text('requested');sys.exit(0)
probe='--crashed' in sys.argv
first='is-system-running' in sys.argv
if mode=='spawn-error':sys.exit(9)
if mode=='hang-main' or (mode in ['hang-list','shutdown-signal'] and not first):
    child=subprocess.Popen(['/usr/bin/sleep','100'])
    (base/'health-child').write_text(str(child.pid))
    time.sleep(100)
if mode=='pipe-owner' and first:
    child=subprocess.Popen(['/usr/bin/sleep','100'])
    (base/'health-child').write_text(str(child.pid));sys.exit(0)
if probe:
    if mode=='assessment-error':print('cannot collect services',file=sys.stderr);sys.exit(7)
    sys.exit(1)
if first:
    print('stopping' if mode=='shutdown' else 'degraded' if mode=='degraded' else 'offline' if mode=='offline' else 'running')
    sys.exit(1 if mode in ['degraded','offline'] else 0)
if mode=='assessment-error':print('list failed',file=sys.stderr);sys.exit(7)
''');helper.chmod(0o755)
    reboot_helper=base/'reboot-helper';reboot_helper.symlink_to(helper)
    def environment(env):return dict(env,RECAB_SYSTEMCTL_BIN=str(helper),RECAB_RC_STATUS_BIN=str(helper),RECAB_REBOOT_BIN=str(reboot_helper),RECAB_OPENRC_DIR=str(openrc),RECAB_RUNLEVELS_DIR=str(levels))
    active=environment(active)
    def switch(slot,g):
        for s in ['A','B']:
            if s+'/var' in mounts:run('umount',str(roots[s]/'var'));mounts.remove(s+'/var')
            if s in mounts:run('umount',str(roots[s]));mounts.remove(s)
        run('mount','-o','ro',dev[slot],str(roots[slot]));mounts.add(slot)
        run('mount','--bind',str(roots['state']),str(roots[slot]/'var'));mounts.add(slot+'/var')
        h['variable']('Selected',f'{prefix}-{slot.lower()}.conf');h['variable']('OneShot',None)
        return environment(h['running'](slot,g))
    def stopped_child():
        if not pid.exists():return
        child=int(pid.read_text());path=Path('/proc')/str(child)/'stat'
        check(not path.exists() or path.read_text().split()[2]=='Z','health assessment leaked a live helper descendant');pid.unlink()
    evidence=[]
    for direction in range(2):
        source='A' if active['RECAB_ROOT']==str(roots['A']) else 'B';destination='B' if source=='A' else 'A'
        check(destination==candidate,'wrong candidate in trial integration')
        source_generation=cli(active,'status')['slots'][0 if source=='A' else 1]['generation']
        original_state=(state/'state.json').read_bytes();original_entries={p.name:p.read_bytes() for p in (esp/'loader/entries').iterdir()}
        original_vars={p.name:p.read_bytes() for p in (efi/'efivars').iterdir()}
        # Existing finalized content from a previous writer must not bypass
        # the new reader requirement when a preparation is reopened.
        runtime=roots[source]/'usr/lib/recab/runtime.json';runtime_bytes=runtime.read_bytes();runtime_stat=runtime.stat()
        def runtime_record(contents):
            run('blockdev','--setrw',dev[source]);run('mount','-o','remount,rw',str(roots[source]));runtime.write_bytes(contents);os.utime(runtime,ns=(runtime_stat.st_atime_ns,runtime_stat.st_mtime_ns));run('mount','-o','remount,ro',str(roots[source]));run('blockdev','--setro',dev[source])
        older=json.loads(runtime_bytes);older['state_minor_max']=3
        runtime_record(json.dumps(older).encode());cli(active,'prepare',code=21,contains='runtime');check((state/'state.json').read_bytes()==original_state,'incompatible reader changed durable state');runtime_record(runtime_bytes)
        evidence.append(f'{source}->{destination}:older-reader-refused-before-write')
        cases=['degraded','assessment-error','missing-helper','hang-main','hang-list','shutdown','shutdown-signal','missing-state','missing-var','missing-esp','efi-failure','efi-readonly-default-safe','partial-default','partial-schedule','commit-state-failure','unbootable-fallback','corrupt-fallback','reused-slot','healthy']
        if init=='systemd':cases+=['pipe-owner','offline']
        # Healthy must run last, to leave a committed generation for reverse.
        cases.remove('healthy');cases.append('healthy')
        selected=os.environ.get('RECAB_TRIAL_CASES')
        if selected:
            names=selected.split(',');check(all(name in cases for name in names),'unknown focused trial case');cases=[c for c in cases if c in names]
        for case in cases:
            print(f'trial check: {init} {source}->{destination} {case}',flush=True)
            active=switch(source,source_generation)
            (state/'state.json').write_bytes(original_state)
            for path in (esp/'loader/entries').iterdir():path.unlink()
            for name,content in original_entries.items():(esp/'loader/entries'/name).write_bytes(content)
            for path in (efi/'efivars').iterdir():
                if path.is_file():path.unlink()
            for name,content in original_vars.items():(efi/'efivars'/name).write_bytes(content)
            rebooted.unlink(missing_ok=True);mode.write_text(case);(openrc/'softlevel').write_text('default')
            for p in (openrc/'failed').iterdir():p.unlink()
            cli(active,'set-next',destination)
            scheduled=(state/'state.json').read_bytes();ticket_path=efi/'efivars'/TICKET;check(ticket_path.exists(),'trial recovery ticket missing')
            trial=switch(destination,generation)
            if case=='degraded':(openrc/'failed/broken').touch()
            if case=='shutdown':(openrc/'softlevel').write_text('shutdown')
            if case=='missing-state':(state/'state.json').unlink()
            if case=='missing-var':
                run('umount',str(roots[destination]/'var'));mounts.remove(destination+'/var');trial=environment(h['running'](destination,generation));mode.write_text('assessment-error')
            if case=='missing-esp':
                run('umount',str(esp));mounts.remove('esp');trial=environment(h['running'](destination,generation));mode.write_text('assessment-error')
            if case=='missing-helper':
                trial.update(RECAB_SYSTEMCTL_BIN=str(base/'absent-systemctl'),RECAB_RC_STATUS_BIN=str(base/'absent-rc-status'))
            if case in ['efi-failure','efi-readonly-default-safe']:
                if case=='efi-failure':h['variable']('Default',f'{prefix}-{destination.lower()}.conf')
                run('mount','--bind',str(efi/'efivars'),str(efi/'efivars'));run('mount','-o','remount,bind,ro',str(efi/'efivars'));mode.write_text('degraded')
            if case=='partial-default':h['variable']('Default',f'{prefix}-{destination.lower()}.conf');mode.write_text('healthy')
            if case=='partial-schedule':(state/'state.json').write_bytes(original_state);mode.write_text('healthy')
            if case=='corrupt-fallback':
                run('blockdev','--setrw',dev[source]);run('mount','-o','rw',dev[source],str(roots[source]));backup=(roots[source]/'etc/config').read_bytes();backup_stat=(roots[source]/'etc/config').stat();(roots[source]/'etc/config').write_text('corrupt fallback\n');run('umount',str(roots[source]));run('blockdev','--setro',dev[source]);mode.write_text('degraded')
            if case=='commit-state-failure':
                # Mounted readonly state rejects the commit after EFI selection.
                run('mount','--bind',str(state),str(state));run('mount','-o','remount,bind,ro',str(state));mode.write_text('healthy')
            if case=='unbootable-fallback':
                broken=esp/'loader/entries'/f'{prefix}-{source.lower()}.conf';backup=broken.read_bytes();broken.unlink();mode.write_text('degraded')
            if case=='reused-slot':
                wrong=json.loads(scheduled);replacement=wrong['slots'][0 if source=='A' else 1];record=wrong['slots'][0 if destination=='A' else 1];record['generation']=replacement['generation'];record['seal']=replacement['seal'];wrong['pending_trial_generation']=source_generation['id']
                (state/'state.json').write_text(json.dumps(wrong));mode.write_text('healthy')
            if case in ['efi-failure','efi-readonly-default-safe','unbootable-fallback','corrupt-fallback']:(openrc/'failed/broken').touch()
            if case=='shutdown-signal':
                child=subprocess.Popen([h['BIN'],'--json','trial','--timeout','5'],env=trial,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
                limit=time.monotonic()+4
                while not pid.exists() and time.monotonic()<limit:time.sleep(.01)
                check(pid.exists(),'cancellation helper did not start');child.send_signal(signal.SIGTERM);out,err=child.communicate(timeout=3);check(child.returncode==0,f'cancelled trial failed: {err}')
            else:
                start=time.monotonic();q=subprocess.run([h['BIN'],'--json','trial','--timeout','1'],env=trial,text=True,capture_output=True,timeout=15)
                if case in ['healthy','partial-default','partial-schedule','shutdown']:check(q.returncode==0,f'{source}->{destination} {case}: {q.stderr}')
                elif case in ['reused-slot']:check(q.returncode!=0 and not rebooted.exists(),f'reused generation accepted: {q.stderr}')
                else:check(q.returncode==23,f'{source}->{destination} {case}: {q.returncode}: {q.stderr}')
                if case.startswith('hang') or case=='pipe-owner':check(time.monotonic()-start<4,'assessment deadline did not bound the whole helper tree')
                if case in ['efi-failure','unbootable-fallback','corrupt-fallback'] or (case=='missing-helper' and init=='systemd'):check(not rebooted.exists() and 'not rebooting' in q.stderr,f'unsafe fallback reboot: {q.stderr}')
                elif case not in ['healthy','partial-default','partial-schedule','shutdown','reused-slot']:check(rebooted.exists(),f'failed trial did not request fallback: {q.stderr}')
            stopped_child()
            if case=='missing-esp':run('mount',dev['esp'],str(esp));mounts.add('esp')
            if case=='efi-readonly-default-safe':run('umount',str(efi/'efivars'))
            if case=='efi-failure':
                run('umount',str(efi/'efivars'));mode.write_text('healthy');(openrc/'failed/broken').unlink(missing_ok=True);cli(trial,'trial','--timeout','2',code=23,contains='Rebooting into slot '+source);check(not json.loads((state/'state.json').read_text())['last_trial']['committed'],'EFI write failure lost its durable verdict')
            if case=='commit-state-failure':run('umount',str(state))
            if case=='unbootable-fallback':broken.write_bytes(backup)
            if case=='corrupt-fallback':
                run('blockdev','--setrw',dev[source]);run('mount','-o','rw',dev[source],str(roots[source]));(roots[source]/'etc/config').write_bytes(backup);os.utime(roots[source]/'etc/config',ns=(backup_stat.st_atime_ns,backup_stat.st_mtime_ns));run('umount',str(roots[source]));run('blockdev','--setro',dev[source])
            if case=='assessment-error':
                mode.write_text('healthy');rebooted.unlink(missing_ok=True);cli(trial,'trial','--timeout','2',code=23,contains='Rebooting into slot '+source);check(rebooted.exists(),'latched failure did not retain fallback');check(not json.loads((state/'state.json').read_text())['last_trial']['committed'],'retry committed a latched failed trial')
            if case in ['missing-state','missing-var']:
                check(not (roots[destination]/'var/lib/recab/state.json').exists() if case=='missing-var' else not (state/'state.json').exists(),'trial invented substitute state')
                (state/'state.json').write_bytes(scheduled)
            if case in ['shutdown','shutdown-signal']:
                check((state/'state.json').read_bytes()==scheduled and not rebooted.exists(),'shutdown got a verdict')
            elif case not in ['healthy','partial-default','partial-schedule','unbootable-fallback','corrupt-fallback','reused-slot']:
                # Observe fallback, then reconcile the durable ticket, including
                # when the failed boot could not save any state.
                mode.write_text('healthy')
                if case=='commit-state-failure':
                    saved_ticket=ticket_path.read_bytes();cli(trial,'commit');ticket_path.write_bytes(saved_ticket)
                active=switch(source,source_generation);cli(active,'trial','--timeout','2')
                check(json.loads((state/'state.json').read_text())['known_good_slot']==source,'fallback reconciliation retained the wrong committed slot')
                default=(efi/f'efivars/LoaderEntryDefault-{h["GUID"]}').read_bytes()[4:].decode('utf-16-le').rstrip('\0');check(default==f'{prefix}-{source.lower()}.conf','fallback reconciliation retained the wrong EFI default')
                record=json.loads((state/'state.json').read_text())['last_trial']
                check(record and not record['committed'] and record['generation']==generation['id'] and record['fallback_generation']==source_generation['id'],'failure was not bound to both generations')
                check(not ticket_path.exists(),'recovered fallback retained stale ticket')
                status=cli(active,'status')
                if status['schema_version']>=10:
                    check(status['lifecycle']['state']=='trial_failed' and status['lifecycle']['trial_current'],'failed generation not exposed in lifecycle')
                    if not status['preparation']:
                        check(any(o['id']=='replace_failed' and o['destructive'] for o in status['operations']) and next(o for o in status['operations'] if o['id']=='prepare')['verdict']=='refused','failed changes can be silently reseeded')
                if case=='assessment-error':
                    old_attempt=record['attempt'];cli(active,'set-next',destination);retry=switch(destination,generation);retried=subprocess.run([h['BIN'],'trial','--timeout','2'],env=retry,text=True,capture_output=True,timeout=15);check(retried.returncode==0,'explicit retry failed: '+retried.stderr);new_record=json.loads((state/'state.json').read_text())['last_trial'];check(new_record['committed'] and new_record['attempt']!=old_attempt,'explicit retry inherited an earlier failed verdict')
            if case in ['healthy','partial-default','partial-schedule']:
                record=json.loads((state/'state.json').read_text())['last_trial'];check(record['committed'] and record['generation']==generation['id'],'healthy trial did not commit scheduled generation');check(not ticket_path.exists(),'committed trial retained ticket')
            evidence.append(f'{source}->{destination}:{case}')
        active=trial
        if direction==0:
            cli(active,'prepare');change=base/'trial-Y';change.write_text('reverse generation Y\n');cli(active,'edit','etc/config','--file',str(change));next_candidate=cli(active,'finalize');candidate=source;generation=next_candidate['generation']
    return {'result':'PASS','init':init,'checks':evidence,'scope':'real filesystems and CLI; observed boot/init records, no VM or hardware boot'}
