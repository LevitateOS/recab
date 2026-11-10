//! `recab status`: the A/B view a person or front end reads, assembled from
//! the boot selection and recab's state.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::entry::{entry_name, layout_name, ENTRIES_DIR};
use crate::state::{
    committed, known_good, plan, rollback_target, BootChange, PersistentState, Request, Selection,
    Slot, TrialRecord,
};
use crate::trial_options;
use crate::System;
use crate::{Generation, Preparation, Readiness};

/// One slot's layout on the ESP, as recab wrote it.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlotView {
    pub slot: Slot,
    /// The entry file, relative to the ESP (`loader/entries/<prefix>-a.conf`).
    pub entry: String,
    /// The directory of its kernel and initramfs, relative to the ESP.
    pub boot_dir: String,
    /// The entry's `root=` (`PARTUUID=...`), if it has one.
    pub root: Option<String>,
    /// The block device `root` names on this machine, if present.
    pub device: Option<String>,
    /// Every file the entry loads is on the ESP.
    pub bootable: bool,
    /// The entry carries a trial's options (it was staged or scheduled and
    /// not yet committed).
    pub trial: bool,
    #[serde(default)]
    pub generation: Option<Generation>,
    #[serde(default)]
    pub readiness: Option<Readiness>,
    #[serde(default)]
    pub generation_ready: bool,
    #[serde(default)]
    pub seal: Option<crate::BootSeal>,
    #[serde(default)]
    pub kernel: Option<String>,
    #[serde(default)]
    pub initrd: Option<String>,
}

/// The schema of [`Status`].
pub const STATUS_SCHEMA_VERSION: u32 = 5;

/// A change a person or front end can ask recab for.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// `recab set-next <inactive>`.
    SetNext,
    Commit,
    Rollback,
    /// `recab stage`.
    Stage,
    Prepare,
    Discard,
    Finalize,
}

/// What recab would make of an action now.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Ready,
    /// The system already is where it leads.
    NoChange,
    Refused,
}

/// The boot selection and slots an action leaves.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct After {
    pub default_slot: Option<Slot>,
    pub next_boot: Option<Slot>,
    pub trial_pending: bool,
    pub known_good_slot: Option<Slot>,
    pub rollback_slot: Option<Slot>,
}

/// One action as recab's state machine plans it from the current state.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionView {
    pub action: Action,
    /// The slot it acts on.
    pub slot: Slot,
    pub verdict: Verdict,
    /// Why it is refused (recab's message), when it is.
    pub reason: Option<String>,
    /// What it leaves, when it is ready.
    pub after: Option<After>,
}

/// Current A/B view exported by `recab status --json`.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub schema_version: u32,
    pub active_slot: Slot,
    pub inactive_slot: Slot,
    /// A one-shot trial boot is set in the firmware and not yet taken.
    pub trial_pending: bool,
    /// The slot the next boot starts: the trial slot, else the default.
    pub next_boot: Option<Slot>,
    pub default_slot: Option<Slot>,
    pub known_good_slot: Option<Slot>,
    /// The running slot is the known-good slot. After a trial boot this is
    /// false until it is committed; an uncommitted slot is left on next boot.
    pub committed: bool,
    /// The slot `recab rollback` would select, if any.
    pub rollback_slot: Option<Slot>,
    /// The slot `set-next` scheduled and no boot has judged yet.
    pub scheduled_trial: Option<Slot>,
    /// How the last judged trial boot ended.
    pub last_trial: Option<TrialRecord>,
    pub entry_prefix: String,
    /// Each slot's entry, kernel directory and partition.
    pub slots: Vec<SlotView>,
    /// What recab would make of each action now.
    pub actions: Vec<ActionView>,
    #[serde(default)]
    pub preparation: Option<Preparation>,
    #[serde(default)]
    pub recovery_reason: Option<String>,
}

/// What `request` would do from `sel` and `state`, as recab plans it.
fn action_view(
    action: Action,
    request: Request,
    slot: Slot,
    sel: &Selection,
    state: &PersistentState,
) -> ActionView {
    let (verdict, reason, after) = match plan(request, sel, state) {
        Err(err) => (Verdict::Refused, Some(format!("{err:#}")), None),
        Ok(None) => (Verdict::NoChange, None, None),
        Ok(Some(transition)) => {
            let mut next_sel = sel.clone();
            match transition.boot {
                Some(BootChange::OneShot(slot)) => next_sel.oneshot = Some(slot),
                Some(BootChange::Default(slot)) => {
                    next_sel.oneshot = None;
                    next_sel.default = Some(slot);
                    next_sel.foreign_default = false;
                }
                None => {}
            }
            let next = &transition.next;
            let after = After {
                default_slot: next_sel.default,
                next_boot: next_sel.next_boot(),
                trial_pending: next_sel.oneshot.is_some(),
                known_good_slot: known_good(next, &next_sel),
                rollback_slot: rollback_target(next, &next_sel),
            };
            (Verdict::Ready, None, Some(after))
        }
    };
    ActionView {
        action,
        slot,
        verdict,
        reason,
        after,
    }
}

/// Query current A/B state from the boot configuration and recab's state, of
/// this system or of the system installed under `root` (its root partition
/// mounted there, the ESP and state partitions below it). Only reads, so it
/// works anywhere the files are readable.
pub fn status(root: Option<&Path>) -> Result<Status> {
    let sys = System::read(root)?;
    let mut slots = Vec::new();
    let mut generation_errors: [Option<String>; 2] = [None, None];
    for slot in Slot::ALL {
        let entry = sys.backend.entry(&sys.boot.prefix, Some(slot))?;
        let partuuid = sys.boot.slot_partuuids[slot as usize].clone();
        let device = match &partuuid {
            Some(partuuid) => target_root::device_with_tag("PARTUUID", partuuid)?,
            None => None,
        };
        let generation_ready = match sys.state.require_ready(slot).and_then(|generation| {
            if sys
                .state
                .slot(slot)
                .is_some_and(|r| r.readiness == Readiness::Ready && r.seal.is_none())
            {
                return Err(crate::error::refuse(
                    "candidate must be finalized before boot selection",
                ));
            }
            if let Some(seal) = sys.state.slot(slot).and_then(|r| r.seal.as_ref()) {
                crate::finalize::verify_entry(&sys, slot, generation, seal)?;
            }
            sys.backend.verify_generation(&sys.boot, slot, generation)
        }) {
            Ok(()) => true,
            Err(error) => {
                generation_errors[slot as usize] = Some(format!("{error:#}"));
                false
            }
        };
        slots.push(SlotView {
            slot,
            entry: format!("{ENTRIES_DIR}/{}", entry_name(&sys.boot.prefix, slot)),
            boot_dir: if sys.state.slot(slot).is_some_and(|r| r.seal.is_some()) {
                "recab/objects".into()
            } else {
                layout_name(&sys.boot.prefix, Some(slot))
            },
            root: partuuid.as_ref().map(|id| format!("PARTUUID={id}")),
            device: device.map(|d| d.display().to_string()),
            bootable: sys.backend.bootable(&sys.boot, slot)?,
            trial: entry.as_ref().is_some_and(trial_options::is_trial),
            generation: sys
                .state
                .slot(slot)
                .and_then(|record| record.generation.clone()),
            readiness: sys.state.slot(slot).map(|record| record.readiness.clone()),
            generation_ready,
            seal: sys.state.slot(slot).and_then(|r| r.seal.clone()),
            kernel: entry
                .as_ref()
                .and_then(|e| e.artifacts().ok())
                .map(|(k, _)| k),
            initrd: entry
                .as_ref()
                .and_then(|e| e.artifacts().ok())
                .and_then(|(_, i)| i),
        });
    }
    let (sel, state) = (&sys.boot.selection, &sys.state);
    let inactive = sel.active.other();
    let rollback_to = rollback_target(state, sel).unwrap_or(inactive);
    let mut actions = vec![
        action_view(
            Action::SetNext,
            Request::SetNext(inactive),
            inactive,
            sel,
            state,
        ),
        action_view(Action::Commit, Request::Commit, sel.active, sel, state),
        action_view(Action::Rollback, Request::Rollback, rollback_to, sel, state),
        action_view(
            Action::Stage,
            Request::Stage { id: "0".repeat(64) },
            inactive,
            sel,
            state,
        ),
    ];
    actions.push(action_view(
        Action::Prepare,
        Request::Prepare {
            id: "0".repeat(64),
            expected_id: None,
            expected_revision: None,
        },
        inactive,
        sel,
        state,
    ));
    if let Some(preparation) = &state.preparation {
        actions.push(action_view(
            Action::Finalize,
            Request::Finalize {
                expected_id: Some(preparation.id.clone()),
                expected_revision: Some(preparation.revision),
            },
            preparation.slot,
            sel,
            state,
        ));
        actions.push(action_view(
            Action::Discard,
            Request::Discard {
                id: preparation.id.clone(),
                revision: preparation.revision,
            },
            preparation.slot,
            sel,
            state,
        ));
    }
    for action in &mut actions {
        if matches!(
            action.action,
            Action::SetNext | Action::Commit | Action::Rollback
        ) && action.verdict != Verdict::Refused
        {
            if let Some(error) = &generation_errors[action.slot as usize] {
                action.verdict = Verdict::Refused;
                action.reason = Some(error.clone());
                action.after = None;
            }
        }
    }
    Ok(Status {
        schema_version: STATUS_SCHEMA_VERSION,
        preparation: state.preparation.clone(),
        recovery_reason: state
            .check_mutable()
            .err()
            .map(|error| format!("{error:#}")),
        scheduled_trial: state.pending_trial_slot,
        actions,
        active_slot: sel.active,
        inactive_slot: sel.active.other(),
        trial_pending: sel.oneshot.is_some(),
        next_boot: sel.next_boot(),
        default_slot: sel.default,
        known_good_slot: known_good(state, sel),
        committed: committed(state, sel),
        rollback_slot: rollback_target(state, sel),
        last_trial: state.last_trial.clone(),
        entry_prefix: sys.boot.prefix.clone(),
        slots,
    })
}
