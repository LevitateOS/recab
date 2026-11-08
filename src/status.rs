//! `recab status`: the A/B view a person or front end reads, assembled from
//! the boot selection and recab's state.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::state::{committed, known_good, rollback_target, Slot, TrialRecord};
use crate::entry::{entry_name, layout_name, ENTRIES_DIR};
use crate::systemd_boot::has_trial_options;
use crate::System;

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
}

/// Current A/B view exported by `recab status --json`.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
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
    /// How the last judged trial boot ended.
    pub last_trial: Option<TrialRecord>,
    pub entry_prefix: String,
    /// Each slot's entry, kernel directory and partition.
    pub slots: Vec<SlotView>,
}

/// Query current A/B state from the boot configuration and recab's state, of
/// this system or of the system installed under `root` (its root partition
/// mounted there, the ESP and state partitions below it). Only reads, so it
/// works anywhere the files are readable.
pub fn status(root: Option<&Path>) -> Result<Status> {
    let sys = System::read(root)?;
    let mut slots = Vec::new();
    for slot in Slot::ALL {
        let entry = sys.backend.entry(&sys.boot.prefix, Some(slot))?;
        let partuuid = entry.as_ref().and_then(|e| e.root_partuuid());
        let device = match &partuuid {
            Some(partuuid) => target_root::device_with_tag("PARTUUID", partuuid)?,
            None => None,
        };
        slots.push(SlotView {
            slot,
            entry: format!("{ENTRIES_DIR}/{}", entry_name(&sys.boot.prefix, slot)),
            boot_dir: layout_name(&sys.boot.prefix, Some(slot)),
            root: entry.as_ref().and_then(|e| e.root_text()),
            device: device.map(|d| d.display().to_string()),
            bootable: sys.backend.bootable(&sys.boot, slot)?,
            trial: entry.as_ref().is_some_and(has_trial_options),
        });
    }
    let (sel, state) = (&sys.boot.selection, &sys.state);
    Ok(Status {
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
