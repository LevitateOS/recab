//! Durable, reversible-direction preparation and in-place system synchronization.

use anyhow::Result;
use std::io::Read;

use crate::error::{ErrorClass, classified, refuse};
use crate::state::{Operation, PersistentState, Preparation, PreparationPhase, Request, plan};
use crate::system::System;

pub(crate) fn new_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(crate::generation::digest(&bytes))
}

pub fn prepare(expected_id: Option<&str>, expected_revision: Option<u64>) -> Result<Preparation> {
    let sys = System::locked("prepare")?;
    crate::filesystem::private_mounts()?;
    crate::immutable::require_readonly(&sys.target)?;
    let active = sys.boot.selection.active;
    let source = sys.state.require_ready(active)?;
    crate::finalize::verify_slot(&sys, active)?;
    sys.backend.verify_generation(&sys.boot, active, source)?;
    let transition = plan(
        Request::Prepare {
            id: new_id()?,
            expected_id: expected_id.map(str::to_string),
            expected_revision,
        },
        &sys.boot.selection,
        &sys.state,
    )?;
    let planned = transition.as_ref().map(|t| &t.next).unwrap_or(&sys.state);
    let preparation = planned
        .preparation
        .as_ref()
        .ok_or_else(|| refuse("preparation was not recorded"))?;
    let slot = preparation.slot;
    let id = preparation.id.clone();
    let uuid = sys.backend.slot_partuuid(&sys.boot, slot)?;
    let preserve = preparation.synchronized || preparation.phase == PreparationPhase::Prepared;
    sys.backend.can_change()?;
    // A fresh continuation gets read-only preflight before ending old rollback.
    // An interrupted continuation can require ext4 journal recovery first.
    let mut mounted = if preserve {
        None
    } else {
        Some(
            crate::slot_mount::SlotMount::open(&sys, slot, &id)
                .map_err(|error| crate::error::or_class(error, ErrorClass::StateOrDetection))?,
        )
    };
    if let Some(mount) = &mounted {
        if sys.state.preparation.is_none() {
            crate::sync::Synchronization::check(&sys.target.root, &mount.point, slot, &uuid, &id)
                .map_err(|error| crate::error::or_class(error, ErrorClass::StateOrDetection))?;
        }
    }
    let mut next = if let Some(transition) = transition {
        sys.apply(&transition)?
    } else {
        sys.state.clone()
    };
    let result = (|| -> Result<()> {
        sys.backend.invalidate_entry(&sys.boot, slot)?;
        if preserve {
            let p = next.preparation.as_mut().unwrap();
            if p.phase != PreparationPhase::Prepared || !p.synchronized {
                p.phase = PreparationPhase::Prepared;
                p.synchronized = true;
                p.failure = None;
                p.revision += 1;
                sys.store.save(&next)?;
            }
            return Ok(());
        }
        let p = next.preparation.as_mut().unwrap();
        p.phase = PreparationPhase::AwaitingSynchronization;
        p.completed_steps = 1;
        p.revision += 1;
        p.failure = None;
        sys.store.save(&next)?;
        let mut mount = mounted.take().unwrap();
        mount.writable(&sys)?;
        // Re-inventory after journal recovery; neither an old plan nor an old
        // completed-step counter is evidence that a range reached disk.
        let sync =
            crate::sync::Synchronization::check(&sys.target.root, &mount.point, slot, &uuid, &id)?;
        let (_, bytes, resumed) = sync.save_plan(&sys.target.state_dir, &id, &source.id)?;
        let report = crate::SyncReport {
            plan_bytes_written: bytes,
            resumed,
            accounting_complete: !resumed,
            ..Default::default()
        };
        let mut report = sync.apply(report)?;
        report.identity_bytes_written = mark_candidate(source, &mount.point, slot, &uuid)?;
        crate::filesystem::flush(&crate::filesystem::Root::open(&mount.point)?)?;
        sync.verify(&mount.point)?;
        drop(sync);
        mount.close()?;
        next.slot_mut(slot)?.generation = Some(source.clone());
        let p = next.preparation.as_mut().unwrap();
        p.phase = PreparationPhase::Prepared;
        p.synchronized = true;
        p.synchronization = Some(report);
        p.completed_steps = 2;
        p.revision += 1;
        sys.store.save(&next)?;
        Ok(())
    })();
    if let Err(error) = result {
        interrupted(&sys, &mut next, &error);
        return Err(classified(
            ErrorClass::PartialUpdate,
            format!("preparation is incomplete: {error:#}"),
        ));
    }
    cleanup_plan(&sys, &id);
    Ok(next.preparation.unwrap())
}

fn mark_candidate(
    generation: &crate::Generation,
    root: &std::path::Path,
    slot: crate::Slot,
    uuid: &str,
) -> Result<u64> {
    let marker =
        serde_json::json!({"version":1,"generation":generation.id,"slot":slot,"partuuid":uuid});
    let bytes = serde_json::to_vec(&marker)?;
    let opened = crate::filesystem::Root::open(root)?;
    let entry = opened.entry(std::path::Path::new(crate::generation::MARKER), false)?;
    if let Some(meta) = entry.metadata()? {
        if !meta.is_file() {
            return Err(refuse("generation marker must be a regular file"));
        }
        if meta.len() <= 4096 {
            let mut current = Vec::new();
            entry.open()?.take(4097).read_to_end(&mut current)?;
            if serde_json::from_slice::<serde_json::Value>(&current)
                .ok()
                .as_ref()
                == Some(&marker)
            {
                return Ok(0);
            }
        }
        // Never write a generation marker through an alias into shared data.
        std::fs::remove_file(entry.path())?;
    }
    entry.write(&bytes, 0o600)?;
    Ok(bytes.len() as u64)
}

pub fn discard(id: &str, revision: u64) -> Result<()> {
    let sys = System::locked("discard")?;
    let transition = plan(
        Request::Discard {
            id: id.into(),
            revision,
        },
        &sys.boot.selection,
        &sys.state,
    )?
    .ok_or_else(|| refuse("no matching preparation"))?;
    sys.backend.can_change()?;
    let mut intent = sys.state.clone();
    let pending = intent
        .preparation
        .as_mut()
        .ok_or_else(|| refuse("no preparation"))?;
    pending.operation = "discard".into();
    pending.phase = PreparationPhase::Intent;
    pending.revision += 1;
    intent.slot_mut(transition.operation_slot())?.readiness = crate::Readiness::Incomplete;
    sys.store.save(&intent)?;
    if let Err(error) = sys
        .backend
        .invalidate_entry(&sys.boot, transition.operation_slot())
    {
        interrupted(&sys, &mut intent, &error);
        return Err(classified(
            ErrorClass::PartialUpdate,
            format!("discard could not invalidate the candidate: {error:#}"),
        ));
    }
    sys.apply(&transition)?;
    crate::change::discard(&sys, id)?;
    crate::update::discard(&sys, id)?;
    crate::merge::discard(&sys, id)?;
    cleanup_plan(&sys, id);
    Ok(())
}

fn interrupted(sys: &System, next: &mut PersistentState, error: &anyhow::Error) {
    if let Some(preparation) = &mut next.preparation {
        preparation.phase = PreparationPhase::Interrupted;
        preparation.failure = Some(format!("{error:#}"));
        preparation.revision += 1;
    }
    if let Err(save) = sys.store.save(next) {
        eprintln!("recab: warning: interruption could not be recorded: {save:#}");
    }
    sys.store.audit(
        if next
            .preparation
            .as_ref()
            .is_some_and(|p| p.operation == "discard")
        {
            Operation::Discard(sys.boot.selection.active.other())
        } else {
            Operation::Prepare(sys.boot.selection.active.other())
        },
        sys.boot.selection.active,
        &sys.state,
        next,
    );
}

fn cleanup_plan(sys: &System, id: &str) {
    let path = sys.target.state_dir.join(format!("synchronize-{id}.json"));
    let result = match std::fs::remove_file(path) {
        Ok(()) => target_root::sync_dir(&sys.target.state_dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("recab: warning: completed synchronization plan could not be removed: {error}");
    }
}
