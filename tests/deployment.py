"""Deployment refusals through the real CLI on finalization.py's GPT disks.
Legacy ledgers, installation proofs and boot entries are exercised on disk;
firmware observations do not claim a physical or VM boot.
"""
import copy,json

def exercise(h,active):
    cli,check,run=h['cli'],h['check'],h['run']
    state,esp,roots=h['state'],h['esp'],h['roots']
    ledger=state/'state.json';installed=state/'installation.json'
    original=ledger.read_bytes();proof=installed.read_bytes()
    entries={p:p.read_bytes() for p in (esp/'loader/entries').iterdir()}
    evidence=[]
    def files(root):
        return {str(p.relative_to(root)):p.read_bytes() for p in root.rglob('*') if p.is_file()}
    for source in ['A','B']:
        target='B' if source=='A' else 'A'
        for s in ['A','B']:
            if s+'/var' in h['mounts']:run('umount',str(roots[s]/'var'));h['mounts'].remove(s+'/var')
            if s in h['mounts']:run('umount',str(roots[s]));h['mounts'].remove(s)
        run('mount','-o','ro',h['dev'][source],str(roots[source]));h['mounts'].add(source)
        run('mount','--bind',str(roots['state']),str(roots[source]/'var'));h['mounts'].add(source+'/var')
        value=json.loads(original);value['known_good_slot']=source;value['previous_good_slot']=target
        for slot in value['slots']:slot['readiness']='committed' if slot['slot']==source else 'ready'
        generation=next(s['generation'] for s in value['slots'] if s['slot']==source)
        prefix=h['fixture_prefix'];h['variable']('Default',f'{prefix}-{source.lower()}.conf');h['variable']('Selected',f'{prefix}-{source.lower()}.conf');h['variable']('OneShot',None)
        active=h['running'](source,generation)
        ledger.write_bytes(h['packed'](value));valid=ledger.read_bytes()
        healthy=cli(active,'status');check(healthy['committed'] and healthy['recovery_reason'] is None and all(s['generation_ready'] for s in healthy['slots']),'current sealed pair refused')
        before_root=cli(active,'inventory','--root',str(roots[source]))
        cases=[('numeric development state',dict(value,schema=1)),*[(f'state 2.{minor}',dict(value,schema={'major':2,'minor':minor})) for minor in range(5)],('missing ledger',None),('missing installation proof',value)]
        for slot in ['A','B']:
            missing=copy.deepcopy(value);next(s for s in missing['slots'] if s['slot']==slot).pop('seal')
            cases.append(('unsealed '+slot,missing))
            old=copy.deepcopy(value);next(s for s in old['slots'] if s['slot']==slot)['seal']['protocol']=2
            cases.append(('old boot protocol '+slot,old))
        for label,broken in cases:
            installed.write_bytes(proof)
            if label=='missing installation proof':installed.unlink()
            if broken is None:ledger.unlink(missing_ok=True)
            else:ledger.write_bytes(h['packed'](broken))
            old_state=ledger.read_bytes() if ledger.exists() else None
            before_esp=files(esp);before_firmware=files(h['efi'])
            view=cli(active,'status');check(view['recovery_reason'] and view['lifecycle']['state']=='unusable',label+' was presented as usable')
            check(all(o['verdict']=='refused' for o in view['operations']),label+' exposed a mutation')
            for argv in [('prepare',),('finalize',),('stage','--manifest',str(h['base']/'not-imported-release.json')),('update','apply','--manifest',str(h['base']/'not-imported-release.json')),('set-next',target),('set-next',target,'--generation',next(s['generation']['id'] for s in value['slots'] if s['slot']==target)),('commit',),('rollback',),('edit','etc/config','--file',str(h['proc']/'cmdline'))]:
                cli(active,*argv,code=21)
            check((ledger.read_bytes() if ledger.exists() else None)==old_state,label+' rewrote/upgraded state')
            check(files(esp)==before_esp and files(h['efi'])==before_firmware,label+' changed boot selection or artifacts')
            check(cli(active,'inventory','--root',str(roots[source]))==before_root,label+' changed the running system')
            evidence.append(source+'->'+target+': '+label)
        ledger.write_bytes(valid);installed.write_bytes(proof)
        # A matching ledger seal cannot make a legacy boot entry eligible.
        path=esp/f'loader/entries/{prefix}-{target.lower()}.conf';saved=path.read_bytes()
        path.write_text(f'title legacy\nlinux /legacy/vmlinuz\ninitrd /legacy/initrd.img\noptions root=PARTUUID={h["uuid"][target]} ro\n')
        old_firmware=files(h['efi']);cli(active,'set-next',target,code=21);cli(active,'rollback',code=21)
        check(files(h['efi'])==old_firmware,'legacy entry changed selection');path.write_bytes(saved)
        source_entry=esp/f'loader/entries/{prefix}-{source.lower()}.conf';source_saved=source_entry.read_bytes()
        source_entry.write_bytes(source_saved.replace(b'/recab/objects/'+generation['kernel_digest'].encode()+b'.linux',b'/legacy/vmlinuz'))
        old_ledger=ledger.read_bytes();cli(active,'prepare',code=21);cli(active,'commit',code=21)
        check(ledger.read_bytes()==old_ledger and files(h['efi'])==old_firmware,'legacy source entry bypassed sealed preparation');source_entry.write_bytes(source_saved)
        # Idempotent commit and raw selection still verify boot objects.
        linux=esp/'recab/objects'/(generation['kernel_digest']+'.linux');saved=linux.read_bytes();linux.write_bytes(saved[:-1]+bytes([saved[-1]^1]))
        cli(active,'commit',code=21);check(files(h['efi'])==old_firmware,'corrupted committed boot set changed selection');linux.write_bytes(saved)
        cli(active,'set-next',target);cli(active,'rollback')
        evidence.append(source+'->'+target+': raw selection, corrupted committed boot set and legacy entry refusal; current pair schedules safely')
    return {'result':'PASS','checks':evidence,'scope':'real GPT/ext4/vfat and CLI; no VM, hardware boot or full build'}
