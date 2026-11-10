//! Preparation identity and boot invalidation. P03 supplies synchronization;
//! recording intent must never claim that an unchanged inactive root is cloned.

use anyhow::Result;
use std::io::Read;

use crate::error::{classified, refuse, ErrorClass};
use crate::state::{plan, Operation, PersistentState, Preparation, PreparationPhase, Request};
use crate::system::System;

pub(crate) fn new_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(crate::generation::digest(&bytes))
}

pub fn prepare(expected_id: Option<&str>, expected_revision: Option<u64>) -> Result<Preparation> {
    let sys = System::locked("prepare")?;
    crate::immutable::require_readonly(&sys.target)?;
    let active = sys.boot.selection.active;
    sys.state.require_ready(active)?.verify_root(
        &sys.target.root,
        active,
        &sys.backend.slot_partuuid(&sys.boot, active)?,
    )?;
    sys.backend
        .verify_generation(&sys.boot, active, sys.state.require_ready(active)?)?;
    let transition = plan(
        Request::Prepare {
            id: new_id()?,
            expected_id: expected_id.map(str::to_string),
            expected_revision,
        },
        &sys.boot.selection,
        &sys.state,
    )?;
    let mut next = match transition {
        Some(transition) => {
            sys.backend.can_change()?;
            sys.apply(&transition)?
        }
        None => sys.state.clone(),
    };
    let preparation = next
        .preparation
        .as_ref()
        .ok_or_else(|| refuse("preparation was not recorded"))?;
    let slot = preparation.slot;
    // Repeating prepare also finishes an interrupted entry invalidation; never
    // reseed or touch candidate payloads here.
    if let Err(error) = sys.backend.invalidate_entry(&sys.boot, slot) {
        interrupted(&sys, &mut next, &error);
        return Err(classified(
            ErrorClass::PartialUpdate,
            format!("preparation is incomplete; boot entry invalidation failed: {error:#}"),
        ));
    }
    let preparation = next.preparation.as_mut().unwrap();
    if preparation.phase != PreparationPhase::Prepared
        && preparation.phase != PreparationPhase::AwaitingSynchronization
    {
        preparation.phase = PreparationPhase::AwaitingSynchronization;
        preparation.completed_steps = 1;
        preparation.revision += 1;
        preparation.failure = None;
    }
    if next != sys.state {
        sys.store.save(&next).map_err(|error| classified(ErrorClass::PartialUpdate,
            format!("boot entry was invalidated but preparation progress could not be saved: {error:#}")))?;
    }
    Ok(next.preparation.unwrap())
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
    sys.apply(&transition).map(|_| ())
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
