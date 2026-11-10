//! Seal an inactive continuation and publish its entry last, under the recab lock.
use crate::error::{classified, refuse, ErrorClass};
use crate::state::{plan, Operation, PreparationPhase, Readiness, Request};
use crate::system::System;
use crate::{Generation, Slot};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootSeal {
    pub generation: String,
    pub kernel_release: String,
    pub architecture: String,
    pub protocol: u32,
}
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Finalization {
    pub generation: Generation,
    pub seal: BootSeal,
    pub kernel_source: String,
    pub initrd_source: String,
    pub boot_bytes_written: u64,
    pub boot_metadata_bytes_written: u64,
    pub reused_boot_files: u64,
    pub accounting_complete: bool,
}
#[derive(Debug, Clone, Default)]
pub struct FinalizeRequest {
    pub preparation: Option<String>,
    pub revision: Option<u64>,
    /// Omit both to reuse the recorded boot set; provide both for an update.
    pub kernel: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
    pub change_flavour: bool,
}

pub fn finalize(request: &FinalizeRequest) -> Result<Finalization> {
    if request.kernel.is_some() != request.initrd.is_some() {
        return Err(refuse(
            "finalization takes both --kernel and --initrd, or neither",
        ));
    }
    let sys = System::locked("finalize")?;
    crate::filesystem::private_mounts()?;
    crate::immutable::require_readonly(&sys.target)?;
    let active = sys.boot.selection.active;
    let source = sys.state.require_ready(active)?;
    source.verify_root(
        &sys.target.root,
        active,
        &sys.backend.slot_partuuid(&sys.boot, active)?,
    )?;
    sys.backend.verify_generation(&sys.boot, active, source)?;
    let transition = plan(
        Request::Finalize {
            expected_id: request.preparation.clone(),
            expected_revision: request.revision,
        },
        &sys.boot.selection,
        &sys.state,
    )?;
    let p = sys
        .state
        .preparation
        .as_ref()
        .context("preparation is missing")?;
    let slot = p.slot;
    let uuid = sys.backend.slot_partuuid(&sys.boot, slot)?;
    let peer = sys.backend.slot_partuuid(&sys.boot, active)?;
    check_record(&sys.target.root, active, &peer, &uuid, source)?;
    let (_, active_initrd) = sys.backend.inputs(&sys.boot, active)?;
    recinit::installed::check_immutable_initramfs(&active_initrd).map_err(|e| {
        refuse(format!(
            "retained fallback needs a compatible immutable boot runtime: {e}"
        ))
    })?;
    let state_uuid = crate::immutable::state_partition(&sys.target)?;
    let esp_uuid = sys.backend.partition()?;
    if [&peer, &uuid, &state_uuid].contains(&&esp_uuid) {
        return Err(refuse("ESP/root/state identities are not distinct"));
    }
    let mut mount = crate::slot_mount::SlotMount::open(&sys, slot, &p.id)?;
    let merged_inputs=crate::merge::boot_inputs(&sys)?;
    let (kernel, initrd) = match (&request.kernel, &request.initrd, &p.finalization) {
        (Some(k), Some(i), _) => (std::fs::canonicalize(k)?, std::fs::canonicalize(i)?),
        (None, None, Some(f)) => sys.backend.object_inputs(&f.generation).unwrap_or((
            PathBuf::from(&f.kernel_source),
            PathBuf::from(&f.initrd_source),
        )),
        _ if merged_inputs.is_some() => {let inputs=merged_inputs.as_ref().unwrap();(inputs.kernel(),inputs.initramfs())},
        _ => sys.backend.inputs(&sys.boot, active)?,
    };
    let checked = crate::candidate::check(
        &mount.point,
        &sys.target.root,
        slot,
        &uuid,
        &state_uuid,
        &esp_uuid,
        &kernel,
        &initrd,
        &source.kernel_digest,
        request.change_flavour || merged_inputs.as_ref().is_some_and(|v|crate::release::installed(&sys.target).is_ok_and(|(identity,_)|identity.flavour!=v.manifest().identity.flavour)),
    )
    .map_err(|error| crate::error::or_class(error, ErrorClass::StateOrDetection))?;
    let policy = crate::runtime::Policy::check(&mount.point, checked.init)?;
    sys.backend.preflight_objects(&checked)?;
    if let Some(f) = &p.finalization {
        if checked.kernel_digest != f.generation.kernel_digest
            || Some(&checked.initrd_digest) != f.generation.initrd_digest.as_ref()
            || crate::Inventory::capture(&mount.point)?.digest()? != f.generation.inventory_digest
        {
            return Err(refuse("candidate or boot inputs changed since finalization began; reopen the preparation before changing its revision"));
        }
    }
    if transition.is_none() {
        let f = p
            .finalization
            .clone()
            .context("finalized candidate has no completion record")?;
        f.generation.verify_root(&mount.point, slot, &uuid)?;
        let record = sys.state.slot(slot).unwrap();
        if record.generation.as_ref() != Some(&f.generation)
            || record.seal.as_ref() != Some(&f.seal)
        {
            return Err(refuse(
                "finalization completion differs from slot generation/seal",
            ));
        }
        check_record(&mount.point, slot, &uuid, &peer, &f.generation)?;
        mount.seal()?;
        sys.backend
            .publish(&sys.boot, slot, &f.generation, &f.seal, checked.init)?;
        return Ok(f);
    }
    let mut next = sys.apply(transition.as_ref().unwrap())?;
    let result = (|| -> Result<Finalization> {
        sys.backend.invalidate_entry(&sys.boot, slot)?;
        mount.writable(&sys)?;
        policy.apply(&mount.point)?;
        let generation = Generation::capture(
            &mount.point,
            &checked.kernel,
            Some(&checked.initrd),
            &sys.target.state_dir,
        )?;
        let seal = BootSeal {
            generation: generation.id.clone(),
            kernel_release: checked.release.clone(),
            architecture: checked.architecture.clone(),
            protocol: 3,
        };
        let mut complete = Finalization {
            generation: generation.clone(),
            seal: seal.clone(),
            kernel_source: checked
                .kernel
                .to_str()
                .context("kernel source path is not UTF-8")?
                .into(),
            initrd_source: checked
                .initrd
                .to_str()
                .context("initramfs source path is not UTF-8")?
                .into(),
            boot_bytes_written: 0,
            boot_metadata_bytes_written: 0,
            reused_boot_files: 0,
            accounting_complete: sys
                .state
                .preparation
                .as_ref()
                .is_some_and(|p| p.operation != "finalize"),
        };
        if let Some(previous) = &next.preparation.as_ref().unwrap().finalization {
            if previous.generation != generation {
                return Err(refuse(
                    "frozen candidate identity changed during finalization",
                ));
            }
        }
        next.preparation.as_mut().unwrap().finalization = Some(complete.clone());
        next.preparation.as_mut().unwrap().completed_steps = 3;
        sys.store.save(&next)?;
        generation.mark_boot(&mount.point, slot, &uuid, &peer)?;
        crate::filesystem::flush(&crate::filesystem::Root::open(&mount.point)?)?;
        generation.verify_root(&mount.point, slot, &uuid)?;
        check_record(&mount.point, slot, &uuid, &peer, &generation)?;
        let (bytes, reused, metadata) =
            sys.backend.install_objects(&checked, &generation, &seal)?;
        complete.boot_bytes_written = bytes;
        complete.reused_boot_files = reused;
        complete.boot_metadata_bytes_written = metadata;
        if crate::generation::hash_file(&checked.kernel)? != generation.kernel_digest
            || Some(crate::generation::hash_file(&checked.initrd)?) != generation.initrd_digest
        {
            return Err(refuse("boot inputs changed during finalization"));
        }
        mount.seal()?;
        let record = next.slot_mut(slot)?;
        record.generation = Some(generation.clone());
        record.seal = Some(seal.clone());
        record.readiness = Readiness::Ready;
        let p = next.preparation.as_mut().unwrap();
        p.finalization = Some(complete.clone());
        p.phase = PreparationPhase::Ready;
        p.operation = "finalize".into();
        p.synchronized = true;
        p.completed_steps = 5;
        p.revision += 1;
        // Ready data precedes entry publication: after a crash there is either
        // no entry, or a fully validated, flushed and protected generation.
        sys.store.save(&next)?;
        complete.boot_metadata_bytes_written +=
            sys.backend
                .publish(&sys.boot, slot, &generation, &seal, checked.init)?;
        let p = next.preparation.as_mut().unwrap();
        p.completed_steps = 6;
        p.finalization = Some(complete.clone());
        sys.store.save(&next)?;
        sys.store
            .audit(Operation::Finalize(slot), active, &sys.state, &next);
        if let Err(error) = sys.backend.garbage_collect(&next) {
            eprintln!("recab: warning: boot artifact collection deferred: {error:#}");
        }
        Ok(complete)
    })();
    match result {
        Ok(complete) => Ok(complete),
        Err(error) => {
            // Even a failed publication must not leave a selectable candidate.
            if let Err(invalidate) = sys.backend.invalidate_entry(&sys.boot, slot) {
                eprintln!("recab: warning: candidate entry removal failed: {invalidate:#}");
            }
            next.slot_mut(slot)?.readiness = Readiness::Incomplete;
            let p = next.preparation.as_mut().unwrap();
            p.phase = PreparationPhase::Interrupted;
            p.failure = Some(format!("{error:#}"));
            p.revision += 1;
            if let Err(save) = sys.store.save(&next) {
                eprintln!("recab: warning: finalization interruption could not be saved: {save:#}");
            }
            Err(classified(
                ErrorClass::PartialUpdate,
                format!("candidate finalization is incomplete: {error:#}"),
            ))
        }
    }
}

fn check_record(root: &Path, slot: Slot, uuid: &str, peer: &str, g: &Generation) -> Result<()> {
    let root = crate::filesystem::Root::open(root)?;
    let text = root
        .optional_text(Path::new(recinit::installed::BOOT_RECORD))?
        .context("finalized boot record is missing")?;
    if text != format!("1 {slot} {uuid} {} {peer}\n", g.id) {
        return Err(refuse(
            "slot/root/generation boot record differs from sealed state",
        ));
    }
    Ok(())
}

/// Recheck before selection/commit, including a complete root scan. Readiness
/// alone is not evidence after a privileged edit or a storage failure.
pub(crate) fn verify_slot(sys: &System, slot: Slot) -> Result<()> {
    let g = sys.state.require_ready(slot)?;
    let record = sys.state.slot(slot).unwrap();
    if record.seal.is_none() && record.readiness != Readiness::Committed {
        return Err(refuse("candidate must be finalized before boot selection"));
    }
    let uuid = sys.backend.slot_partuuid(&sys.boot, slot)?;
    let peer = sys.backend.slot_partuuid(&sys.boot, slot.other())?;
    if slot == sys.boot.selection.active {
        crate::immutable::require_readonly(&sys.target)?;
        g.verify_root(&sys.target.root, slot, &uuid)?;
        if let Some(seal) = &record.seal {
            check_record(&sys.target.root, slot, &uuid, &peer, g)?;
            verify_entry(sys, slot, g, seal)?;
        }
    } else {
        let id = sys
            .state
            .preparation
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_else(|| g.id.clone());
        let mounted = crate::slot_mount::SlotMount::open(sys, slot, &id)?;
        g.verify_root(&mounted.point, slot, &uuid)?;
        if let Some(seal) = &record.seal {
            check_record(&mounted.point, slot, &uuid, &peer, g)?;
            verify_entry(sys, slot, g, seal)?;
        }
        mounted.seal()?;
    }
    Ok(())
}
pub(crate) fn verify_entry(
    sys: &System,
    slot: Slot,
    g: &Generation,
    seal: &BootSeal,
) -> Result<()> {
    sys.backend.verify_manifest(g, seal)?;
    let entry = sys
        .backend
        .entry(&sys.boot.prefix, Some(slot))?
        .context("finalized entry is missing")?;
    let (kernel, initrd) = entry.artifacts()?;
    let expected = crate::systemd_boot::object_paths(g)?;
    let options = entry.options();
    let root = format!(
        "root=PARTUUID={}",
        sys.backend.slot_partuuid(&sys.boot, slot)?
    );
    for (prefix, expected) in [
        ("root=", root),
        ("recab.generation=", format!("recab.generation={}", g.id)),
        ("recab.slot=", format!("recab.slot={slot}")),
    ] {
        let values = options
            .iter()
            .filter(|o| o.starts_with(prefix))
            .collect::<Vec<_>>();
        if values.len() != 1 || *values[0] != expected {
            return Err(refuse(
                "entry has an ambiguous or mismatched root/slot/generation",
            ));
        }
    }
    if (kernel, initrd) != (expected.0, Some(expected.1))
        || !entry.options().contains(&format!("recab.slot={slot}"))
        || !entry
            .options()
            .contains(&recinit::installed::IMMUTABLE_OPTION.to_string())
        || !entry.options().contains(&"ro".to_string())
        || entry.options().iter().any(|o| {
            o == "rw"
                || [
                    "init=",
                    "rdinit=",
                    "rootflags=",
                    "systemd.volatile",
                    "systemd.mount-extra",
                ]
                .iter()
                .any(|prefix| o.starts_with(prefix))
        })
    {
        return Err(refuse(
            "entry is not bound to the immutable finalized boot set",
        ));
    }
    Ok(())
}
