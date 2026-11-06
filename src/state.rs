//! The slot state machine, without I/O: recab's persistent record, what the
//! boot loader currently selects, and the transition each request makes.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::error::{tagged_err, ErrorClass};
use crate::Slot;

pub(crate) const STATE_SCHEMA_VERSION: u32 = 1;

/// What recab keeps in its state file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PersistentState {
    pub schema: u32,
    pub known_good_slot: Option<Slot>,
    /// The slot that was known-good before `known_good_slot`, while it still
    /// holds that system: what rollback from a committed slot returns to.
    #[serde(default)]
    pub previous_good_slot: Option<Slot>,
    pub pending_trial_slot: Option<Slot>,
    pub last_operation: Option<String>,
    pub last_transition_unix: Option<u64>,
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA_VERSION,
            known_good_slot: None,
            previous_good_slot: None,
            pending_trial_slot: None,
            last_operation: None,
            last_transition_unix: None,
        }
    }
}

/// What the boot loader selects, as recab's decisions need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    /// The slot that booted.
    pub active: Slot,
    /// The slot booted by default.
    pub default: Option<Slot>,
    /// The slot of a one-shot entry not taken yet.
    pub oneshot: Option<Slot>,
}

impl Selection {
    /// The slot the next boot starts.
    pub fn next_boot(&self) -> Option<Slot> {
        self.oneshot.or(self.default)
    }
}

/// The slot to fall back to: the one committed as good, or else the default.
pub(crate) fn known_good(state: &PersistentState, sel: &Selection) -> Option<Slot> {
    state.known_good_slot.or(sel.default)
}

pub(crate) fn committed(state: &PersistentState, sel: &Selection) -> bool {
    known_good(state, sel) == Some(sel.active)
}

/// What rollback selects: from an uncommitted slot, or to cancel a pending
/// trial, the known-good slot; from a committed slot, the slot that was good
/// before it.
pub(crate) fn rollback_target(state: &PersistentState, sel: &Selection) -> Option<Slot> {
    if !committed(state, sel) || sel.oneshot.is_some() {
        known_good(state, sel)
    } else {
        state.previous_good_slot
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Request {
    /// Write the inactive slot.
    Stage,
    SetNext(Slot),
    Commit,
    Rollback,
}

/// A state transition; its names are used in the state file and audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Stage(Slot),
    SetNext(Slot),
    Commit(Slot),
    Rollback(Slot),
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Operation::Stage(_) => "stage",
            Operation::SetNext(_) => "set-next",
            Operation::Commit(_) => "commit",
            Operation::Rollback(_) => "rollback",
        }
    }

    /// `last_operation` in the state file.
    pub fn label(self) -> String {
        match self {
            Operation::Stage(slot) => format!("stage-{slot}"),
            Operation::SetNext(slot) => format!("set-next-{slot}"),
            Operation::Commit(slot) => format!("commit-{slot}"),
            Operation::Rollback(slot) => format!("rollback-to-{slot}"),
        }
    }
}

/// The boot selection a transition writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootChange {
    None,
    /// Boot this slot once, then the default again.
    OneShot(Slot),
    /// Boot this slot by default; any pending one-shot is cleared.
    Default(Slot),
}

#[derive(Debug, Clone)]
pub(crate) struct Transition {
    pub operation: Operation,
    pub next: PersistentState,
    pub boot: BootChange,
}

fn refuse(message: String) -> anyhow::Error {
    tagged_err(ErrorClass::StateOrDetection, message)
}

/// The transition `request` makes from `state` and `sel`, `None` when the
/// system already is where the request leads, or the reason it is refused.
pub(crate) fn plan(
    request: Request,
    sel: &Selection,
    state: &PersistentState,
) -> Result<Option<Transition>> {
    let active = sel.active;
    let mut next = state.clone();
    let transition = match request {
        Request::Stage => {
            let slot = active.other();
            if !committed(state, sel) {
                return Err(refuse(format!(
                    "slot {active} is running uncommitted, so slot {slot} is the fallback; staging over it would leave no known-good system.\n\
Run 'recab commit' to keep slot {active}, or reboot into slot {slot}."
                )));
            }
            if sel.oneshot == Some(slot) {
                return Err(refuse(format!(
                    "a trial boot of slot {slot} is pending; run 'recab rollback' to cancel it before staging slot {slot} again"
                )));
            }
            next.known_good_slot = Some(active);
            if next.pending_trial_slot == Some(slot) {
                next.pending_trial_slot = None;
            }
            // Its system is about to be replaced: nothing to roll back to.
            if next.previous_good_slot == Some(slot) {
                next.previous_good_slot = None;
            }
            Transition { operation: Operation::Stage(slot), next, boot: BootChange::None }
        }
        Request::SetNext(slot) if slot == active => {
            return Err(refuse(format!(
                "slot {slot} is the running slot; set-next stages the inactive slot ({}). To cancel a pending trial, run 'recab rollback'.",
                slot.other()
            )));
        }
        Request::SetNext(slot) => {
            if sel.oneshot == Some(slot) {
                return Ok(None);
            }
            let fallback = known_good(state, sel).unwrap_or(active);
            if sel.default != Some(fallback) {
                return Err(refuse(format!(
                    "cannot stage trial boot: default slot ({}) is not the known-good slot ({fallback}), so a failed trial would not fall back to it.\n\
Run 'recab commit' to set a stable fallback default before 'recab set-next {slot}'.",
                    sel.default.map_or("none".to_string(), |s| s.to_string())
                )));
            }
            next.known_good_slot = Some(fallback);
            next.pending_trial_slot = Some(slot);
            Transition { operation: Operation::SetNext(slot), next, boot: BootChange::OneShot(slot) }
        }
        Request::Commit => {
            if committed(state, sel) && sel.default == Some(active) && sel.oneshot.is_none() {
                return Ok(None);
            }
            let good = known_good(state, sel);
            if good != Some(active) {
                // The slot that was good until now is what rollback returns to.
                next.previous_good_slot = good;
            }
            next.known_good_slot = Some(active);
            next.pending_trial_slot = None;
            Transition { operation: Operation::Commit(active), next, boot: BootChange::Default(active) }
        }
        Request::Rollback => {
            let target = rollback_target(state, sel).ok_or_else(|| {
                refuse(if committed(state, sel) {
                    format!("cannot rollback: slot {active} is committed and no other slot has been known-good since it was written")
                } else {
                    "cannot rollback: no known-good slot is recorded and no default slot is set".to_string()
                })
            })?;
            if target != active && committed(state, sel) {
                // Leaving a committed slot: it stays a slot to roll back to.
                next.previous_good_slot = Some(active);
            }
            next.known_good_slot = Some(target);
            next.pending_trial_slot = None;
            Transition { operation: Operation::Rollback(target), next, boot: BootChange::Default(target) }
        }
    };
    Ok(Some(transition))
}
