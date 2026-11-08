//! The slot state machine, without I/O: recab's persistent record, what the
//! boot loader currently selects, the transition each request makes, and
//! what a trial boot's outcome calls for.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::error::refuse;

/// System slot identifier (A/B). Its one spelling (`A`, `B`) is used by the
/// CLI, the JSON and the state file.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub const ALL: [Slot; 2] = [Slot::A, Slot::B];

    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
}

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Slot::A => "A",
            Slot::B => "B",
        })
    }
}

impl std::str::FromStr for Slot {
    type Err = String;

    /// `A` or `B`, in either case; nothing else (REQ-SEC-002).
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Slot::ALL
            .into_iter()
            .find(|slot| slot.to_string().eq_ignore_ascii_case(text))
            .ok_or_else(|| format!("'{text}' is not a slot (A or B)"))
    }
}

/// How the last judged trial boot ended (kept in the state file, and shown
/// as is by `status`).
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrialRecord {
    pub slot: Slot,
    /// The trial was healthy and committed; otherwise it was abandoned.
    pub committed: bool,
    /// How startup ended, and the failed units if any.
    pub detail: String,
    pub unix_time: u64,
}

/// The schema recab writes. Both slots' recab read the same file (it lives
/// on the shared /var), so newer schemas only add fields: an older recab
/// reads them, keeps the fields it does not know, and never lowers the number.
pub(crate) const STATE_SCHEMA_VERSION: u32 = 1;
/// The oldest schema recab has written.
const FIRST_STATE_SCHEMA: u32 = 1;

/// What recab keeps in its state file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersistentState {
    pub schema: u32,
    #[serde(default)]
    pub known_good_slot: Option<Slot>,
    /// The slot that was known-good before `known_good_slot`, while it still
    /// holds that system: what rollback from a committed slot returns to.
    #[serde(default)]
    pub previous_good_slot: Option<Slot>,
    /// The slot `set-next` scheduled; only its boot is judged as a trial.
    #[serde(default)]
    pub pending_trial_slot: Option<Slot>,
    /// How the last judged trial ended.
    #[serde(default)]
    pub last_trial: Option<TrialRecord>,
    #[serde(default)]
    pub last_operation: Option<String>,
    #[serde(default)]
    pub last_transition_unix: Option<u64>,
    /// Fields a newer recab wrote, kept as they are.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl PersistentState {
    /// A state file of a schema recab can read: the first schema or any
    /// newer one, since newer schemas only add fields.
    pub fn check_schema(&self) -> Result<(), String> {
        if self.schema < FIRST_STATE_SCHEMA {
            return Err(format!("schema {} is not a recab state schema", self.schema));
        }
        Ok(())
    }
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA_VERSION,
            known_good_slot: None,
            previous_good_slot: None,
            pending_trial_slot: None,
            last_trial: None,
            last_operation: None,
            last_transition_unix: None,
            unknown: serde_json::Map::new(),
        }
    }
}

/// What the boot loader selects, as recab's decisions need it. Entries whose
/// files are missing from the ESP are never selected: systemd-boot skips them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    /// The slot that booted.
    pub active: Slot,
    /// The slot booted by default.
    pub default: Option<Slot>,
    /// The slot of a one-shot entry not taken yet.
    pub oneshot: Option<Slot>,
    /// systemd-boot boots another, bootable entry by default (one set from
    /// the boot menu, an old install's, another OS's): `default` is `None`.
    pub foreign_default: bool,
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
    SetNext(Slot),
    Commit,
    Rollback,
}

/// A state transition; its names are used in the state file and audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    /// `recab init` set up the slots with this one holding the system.
    Init(Slot),
    Stage(Slot),
    SetNext(Slot),
    Commit(Slot),
    Rollback(Slot),
    /// A trial boot that was not healthy.
    TrialFailed(Slot),
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Operation::Init(_) => "init",
            Operation::Stage(_) => "stage",
            Operation::SetNext(_) => "set-next",
            Operation::Commit(_) => "commit",
            Operation::Rollback(_) => "rollback",
            Operation::TrialFailed(_) => "trial-failed",
        }
    }

    /// `last_operation` in the state file.
    pub fn label(self) -> String {
        match self {
            Operation::Init(slot) => format!("init-{slot}"),
            Operation::Stage(slot) => format!("stage-{slot}"),
            Operation::SetNext(slot) => format!("set-next-{slot}"),
            Operation::Commit(slot) => format!("commit-{slot}"),
            Operation::Rollback(slot) => format!("rollback-to-{slot}"),
            Operation::TrialFailed(slot) => format!("trial-failed-{slot}"),
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

/// The transition `stage` makes: it writes the inactive slot, or the reason
/// it is refused.
pub(crate) fn plan_stage(sel: &Selection, state: &PersistentState) -> Result<Transition> {
    let active = sel.active;
    let mut next = state.clone();
    let slot = active.other();
    if !committed(state, sel) {
        return Err(refuse(format!(
            "slot {active} is running uncommitted, so slot {slot} is the fallback; staging over it would leave no known-good system.\n\
Run 'recab commit' to keep slot {active}, or reboot into slot {slot}."
        )));
    }
    if sel.next_boot() == Some(slot) {
        return Err(refuse(format!(
            "the next boot is slot {slot}; run 'recab rollback' (or 'recab commit') so it is slot {active} before staging slot {slot}"
        )));
    }
    if sel.foreign_default {
        return Err(refuse(format!(
            "systemd-boot boots another entry by default; run 'recab commit' so it is slot {active} before staging slot {slot}"
        )));
    }
    // After a one-shot boot of the running slot, the freshly written
    // slot would boot as the default, untried.
    if sel.default == Some(slot) {
        return Err(refuse(format!(
            "the default is slot {slot}; run 'recab commit' so it is slot {active} before staging slot {slot}"
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
    Ok(Transition { operation: Operation::Stage(slot), next, boot: BootChange::None })
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

/// How system startup ended, as the init adapter reports it.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Startup {
    /// Startup ran to its end with nothing failed.
    Running,
    /// Startup ran to its end with something failed.
    Failed,
    /// Startup ended in the init's emergency state.
    Emergency,
    /// Still starting when the deadline passed.
    TimedOut,
    /// No verdict (shutting down, the init unreachable).
    Inconclusive,
}

/// What a trial boot's outcome calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrialVerdict {
    /// Not a trial recab scheduled, or no verdict: do nothing.
    Ignore,
    Commit,
    /// Do not commit; return to the known-good slot.
    Fail,
    /// The slots could not be read, but the firmware says this failed boot
    /// is a one-shot trial: reboot into the firmware's default.
    RebootUnread,
    /// The slots could not be read, and nothing says this boot is a trial.
    Unreadable,
}

/// What recab could read of the slots when it judges a trial.
pub(crate) struct TrialView<'a> {
    pub sel: &'a Selection,
    pub state: &'a PersistentState,
    /// recab's state file exists (it lives on /var, which may be what broke).
    pub state_exists: bool,
}

/// Judge a boot from how startup ended, what recab read of the slots (`None`
/// when the ESP or /var could not be read) and, asked only when the slots
/// do not settle it, whether the firmware booted a one-shot entry. Only the
/// boot of the slot `set-next` scheduled is a trial; a slot picked from the
/// boot menu, or a trial an operator already rolled back, is left alone.
pub(crate) fn judge_trial(
    startup: &Startup,
    view: Option<TrialView>,
    firmware_trial: impl FnOnce() -> Result<Option<bool>>,
) -> Result<TrialVerdict> {
    if *startup == Startup::Inconclusive {
        return Ok(TrialVerdict::Ignore);
    }
    let failed = *startup != Startup::Running;
    let Some(view) = view else {
        return Ok(if failed && firmware_trial()? == Some(true) {
            TrialVerdict::RebootUnread
        } else {
            TrialVerdict::Unreadable
        });
    };
    let is_trial = view.state.pending_trial_slot == Some(view.sel.active) && !committed(view.state, view.sel);
    Ok(match startup {
        // No state on /var but a one-shot boot that failed: /var itself may
        // be what failed to mount.
        _ if !is_trial => {
            if failed && !view.state_exists && firmware_trial()? == Some(true) {
                TrialVerdict::Fail
            } else {
                TrialVerdict::Ignore
            }
        }
        Startup::Running => TrialVerdict::Commit,
        _ => TrialVerdict::Fail,
    })
}
