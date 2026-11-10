//! The slot state machine, without I/O: recab's persistent record, what the
//! boot loader currently selects, the transition each request makes, and
//! what a trial boot's outcome calls for.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::error::refuse;
use crate::generation::Generation;

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

/// A structured major version intentionally cannot be parsed by the old
/// numeric-schema reader, which otherwise ignores future safety restrictions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum StateSchema {
    Legacy(u32),
    Current { major: u32, minor: u32 },
}
impl Default for StateSchema {
    fn default() -> Self {
        Self::Current { major: 2, minor: 1 }
    }
}

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    Empty,
    Incomplete,
    Ready,
    Committed,
}

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlotState {
    pub slot: Slot,
    pub partuuid: String,
    pub generation: Option<Generation>,
    pub readiness: Readiness,
}
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreparationPhase {
    Intent,
    AwaitingSynchronization,
    Prepared,
    Staging,
    Ready,
    Interrupted,
}
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Preparation {
    pub id: String,
    pub slot: Slot,
    pub source_slot: Slot,
    pub source_generation: String,
    pub revision: u64,
    pub operation: String,
    pub phase: PreparationPhase,
    pub completed_steps: u64,
    pub failure: Option<String>,
    /// Once synchronized, reopening must never reseed pending system changes.
    #[serde(default)]
    pub synchronized: bool,
    #[serde(default)]
    pub synchronization: Option<crate::SyncReport>,
}

/// What recab keeps in its state file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PersistentState {
    pub schema: StateSchema,
    #[serde(default)]
    pub slots: Vec<SlotState>,
    #[serde(default)]
    pub preparation: Option<Preparation>,
    #[serde(default)]
    pub pending_trial_generation: Option<String>,
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
    pub fn check_schema(&self) -> Result<(), String> {
        match self.schema {
            StateSchema::Legacy(1)
            | StateSchema::Current {
                major: 2,
                minor: 0 | 1,
            } => Ok(()),
            _ => Err("unsupported recab state format; use a compatible recovery tool".into()),
        }
    }
    pub fn slot(&self, slot: Slot) -> Option<&SlotState> {
        self.slots.iter().find(|record| record.slot == slot)
    }
    pub fn slot_mut(&mut self, slot: Slot) -> Result<&mut SlotState> {
        self.slots
            .iter_mut()
            .find(|record| record.slot == slot)
            .ok_or_else(|| refuse("slot mapping is not initialized"))
    }
    pub fn check_mutable(&self) -> Result<()> {
        self.check_schema().map_err(refuse)?;
        if matches!(self.schema, StateSchema::Legacy(_)) {
            return Err(refuse(
                "legacy slot-only state is read-only; explicit adoption with compatible runtimes is required",
            ));
        }
        if self.slots.len() != 2 {
            return Err(refuse(
                "recab generation state is not initialized for both partitions",
            ));
        }
        let a = self
            .slot(Slot::A)
            .ok_or_else(|| refuse("slot A mapping is missing"))?;
        let b = self
            .slot(Slot::B)
            .ok_or_else(|| refuse("slot B mapping is missing"))?;
        if [a, b].iter().any(|record| {
            record.partuuid.is_empty()
                || !record
                    .partuuid
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        }) || a.partuuid.eq_ignore_ascii_case(&b.partuuid)
        {
            return Err(refuse("invalid or duplicate slot partition identity"));
        }
        for record in &self.slots {
            if record
                .generation
                .as_ref()
                .is_some_and(|generation| !generation.valid())
            {
                return Err(refuse("invalid system generation digest"));
            }
            if matches!(record.readiness, Readiness::Ready | Readiness::Committed)
                && record.generation.is_none()
            {
                return Err(refuse("a complete slot has no generation identity"));
            }
        }
        for slot in [self.known_good_slot, self.previous_good_slot]
            .into_iter()
            .flatten()
        {
            self.require_ready(slot)?;
        }
        match (
            self.pending_trial_slot,
            self.pending_trial_generation.as_ref(),
        ) {
            (Some(slot), Some(id)) if self.require_ready(slot)?.id == *id => {}
            (None, None) => {}
            _ => {
                return Err(refuse(
                    "pending trial does not identify a complete generation",
                ))
            }
        }
        if let Some(preparation) = &self.preparation {
            let source = self
                .slot(preparation.source_slot)
                .and_then(|slot| slot.generation.as_ref());
            if preparation.slot == preparation.source_slot
                || source.is_none_or(|source| source.id != preparation.source_generation)
                || preparation.id.len() != 64
                || !preparation.id.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(refuse("invalid preparation source or identity"));
            }
            self.require_ready(preparation.source_slot)?;
            let candidate = self.slot(preparation.slot).unwrap();
            if (preparation.phase == PreparationPhase::Ready
                && candidate.readiness != Readiness::Ready)
                || (preparation.phase != PreparationPhase::Ready
                    && candidate.readiness != Readiness::Incomplete)
            {
                return Err(refuse(
                    "preparation phase conflicts with candidate readiness",
                ));
            }
        }
        Ok(())
    }
    pub fn require_ready(&self, slot: Slot) -> Result<&Generation> {
        let record = self
            .slot(slot)
            .ok_or_else(|| refuse("slot has no generation mapping"))?;
        if !matches!(record.readiness, Readiness::Ready | Readiness::Committed) {
            return Err(refuse(format!(
                "slot {slot} is incomplete; kernel files do not establish a finalized candidate"
            )));
        }
        record
            .generation
            .as_ref()
            .ok_or_else(|| refuse("slot has no generation identity"))
    }
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            schema: StateSchema::default(),
            slots: Vec::new(),
            preparation: None,
            pending_trial_generation: None,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    /// The slot that booted.
    pub active: Slot,
    pub active_generation: Option<String>,
    /// The slot booted by default.
    pub default: Option<Slot>,
    /// The slot of a one-shot entry not taken yet.
    pub oneshot: Option<Slot>,
    /// systemd-boot boots another, bootable entry by default (one set from
    /// the boot menu, an old install's, another OS's): `default` is `None`.
    pub foreign_default: bool,
    /// Each slot's entry loads only files that are on the ESP (A, then B).
    pub bootable: [bool; 2],
}

impl Selection {
    /// The slot the next boot starts.
    pub fn next_boot(&self) -> Option<Slot> {
        self.oneshot.or(self.default)
    }

    /// systemd-boot can boot `slot` (else it skips its entry).
    pub fn can_boot(&self, slot: Slot) -> bool {
        self.bootable[slot as usize]
    }
}

/// The slot to fall back to: the one committed as good, or else the default.
pub(crate) fn known_good(state: &PersistentState, sel: &Selection) -> Option<Slot> {
    state.known_good_slot.or(sel.default)
}

pub(crate) fn committed(state: &PersistentState, sel: &Selection) -> bool {
    known_good(state, sel) == Some(sel.active)
        && (matches!(state.schema, StateSchema::Legacy(_))
            || state
                .slot(sel.active)
                .and_then(|slot| slot.generation.as_ref())
                .is_some_and(|generation| {
                    sel.active_generation.as_deref() == Some(generation.id.as_str())
                }))
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

/// What a command asks of the state machine.
#[derive(Debug, Clone)]
pub(crate) enum Request {
    /// Schedule one trial boot of the inactive slot.
    SetNext(Slot),
    /// Keep the running slot.
    Commit,
    /// Go back to the known-good slot, or the one good before it.
    Rollback,
    /// Write a new system into the inactive slot.
    Stage {
        id: String,
    },
    Prepare {
        id: String,
        expected_id: Option<String>,
        expected_revision: Option<u64>,
    },
    Discard {
        id: String,
        revision: u64,
    },
}

/// A state transition; its names are used in the state file and audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    /// `recab init` set up the slots with this one holding the system.
    Init(Slot),
    Stage(Slot),
    Prepare(Slot),
    Discard(Slot),
    SetNext(Slot),
    Commit(Slot),
    Rollback(Slot),
    /// A trial boot that was not healthy.
    TrialFailed(Slot),
    /// A scheduled trial whose one-shot another boot used up.
    TrialMissed(Slot),
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Operation::Init(_) => "init",
            Operation::Stage(_) => "stage",
            Operation::Prepare(_) => "prepare",
            Operation::Discard(_) => "discard",
            Operation::SetNext(_) => "set-next",
            Operation::Commit(_) => "commit",
            Operation::Rollback(_) => "rollback",
            Operation::TrialFailed(_) => "trial-failed",
            Operation::TrialMissed(_) => "trial-missed",
        }
    }

    pub fn slot(self) -> Slot {
        match self {
            Operation::Init(slot)
            | Operation::Stage(slot)
            | Operation::Prepare(slot)
            | Operation::Discard(slot)
            | Operation::SetNext(slot)
            | Operation::Commit(slot)
            | Operation::Rollback(slot)
            | Operation::TrialFailed(slot)
            | Operation::TrialMissed(slot) => slot,
        }
    }

    /// `last_operation` in the state file: the name and the slot.
    pub fn label(self) -> String {
        format!("{}-{}", self.name(), self.slot())
    }
}

/// The boot selection a transition writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootChange {
    /// Boot this slot once, then the default again.
    OneShot(Slot),
    /// Boot this slot by default; any pending one-shot is cleared.
    Default(Slot),
}

#[derive(Debug, Clone)]
pub(crate) struct Transition {
    pub operation: Operation,
    pub next: PersistentState,
    /// The boot selection to write first, if it changes.
    pub boot: Option<BootChange>,
}

impl Transition {
    /// The slot the transition is about.
    pub fn operation_slot(&self) -> Slot {
        self.operation.slot()
    }
}

/// The transition `request` makes from `state` and `sel`, `None` when the
/// system already is where the request leads, or the reason it is refused.
/// recab never selects a slot that cannot boot.
pub(crate) fn plan(
    request: Request,
    sel: &Selection,
    state: &PersistentState,
) -> Result<Option<Transition>> {
    state.check_mutable()?;
    let active = sel.active;
    let inactive = active.other();
    let mut next = state.clone();
    let transition = match request {
        Request::SetNext(slot) if slot == active => {
            return Err(refuse(format!(
                "slot {slot} is the running slot; set-next schedules a trial of the inactive slot ({inactive}). To cancel a pending trial, run 'recab rollback'."
            )));
        }
        Request::SetNext(slot) => {
            let generation = state.require_ready(slot)?;
            if !committed(state, sel) {
                return Err(refuse(format!(
                    "slot {active} is running uncommitted and slot {slot} is the known-good slot; run 'recab commit' to keep slot {active}, or 'recab rollback' to return to slot {slot}"
                )));
            }
            // Already scheduled, in the firmware and in recab's state.
            if sel.oneshot == Some(slot)
                && state.pending_trial_slot == Some(slot)
                && state.pending_trial_generation.as_deref() == Some(generation.id.as_str())
            {
                return Ok(None);
            }
            if !sel.can_boot(slot) {
                return Err(refuse(format!(
                    "slot {slot} cannot boot (its entry names files missing from the ESP); write a system into it with 'recab stage' first"
                )));
            }
            if sel.default != Some(active) {
                return Err(refuse(format!(
                    "the default is not the known-good slot {active}, so a failed trial would not fall back to it; run 'recab commit' before 'recab set-next {slot}'"
                )));
            }
            next.pending_trial_slot = Some(slot);
            next.pending_trial_generation = Some(generation.id.clone());
            Transition {
                operation: Operation::SetNext(slot),
                next,
                boot: Some(BootChange::OneShot(slot)),
            }
        }
        Request::Commit => {
            let generation = state.require_ready(active)?;
            if sel.active_generation.as_deref() != Some(generation.id.as_str()) {
                return Err(refuse("the booted generation does not match the candidate"));
            }
            if committed(state, sel) && sel.default == Some(active) && sel.oneshot.is_none() {
                return Ok(None);
            }
            commit(state, sel)
        }
        Request::Rollback => {
            let target = rollback_target(state, sel).ok_or_else(|| {
                refuse(if committed(state, sel) {
                    format!("cannot rollback: slot {active} is committed and no other slot has been known-good since it was written")
                } else {
                    "cannot rollback: no known-good slot is recorded and no default slot is set".to_string()
                })
            })?;
            state.require_ready(target)?;
            if !sel.can_boot(target) {
                return Err(refuse(format!(
                    "cannot rollback: slot {target} cannot boot (its entry names files missing from the ESP)"
                )));
            }
            if target != active && committed(state, sel) {
                // Leaving a committed slot: it stays a slot to roll back to.
                next.previous_good_slot = Some(active);
            }
            next.known_good_slot = Some(target);
            next.pending_trial_generation = None;
            next.pending_trial_slot = None;
            Transition {
                operation: Operation::Rollback(target),
                next,
                boot: Some(BootChange::Default(target)),
            }
        }
        Request::Prepare {
            id,
            expected_id,
            expected_revision,
        } => {
            ensure_preparation_allowed(state, sel)?;
            if let Some(preparation) = &state.preparation {
                check_preparation(
                    preparation,
                    sel,
                    state,
                    expected_id.as_deref(),
                    expected_revision,
                )?;
                if preparation.phase == PreparationPhase::Ready {
                    let reopened = next.preparation.as_mut().unwrap();
                    reopened.phase = PreparationPhase::Prepared;
                    reopened.operation = "prepare".into();
                    reopened.synchronized = true;
                    reopened.revision += 1;
                    next.slot_mut(inactive)?.readiness = Readiness::Incomplete;
                    return Ok(Some(Transition {
                        operation: Operation::Prepare(inactive),
                        next,
                        boot: None,
                    }));
                }
                if preparation.operation != "prepare" {
                    return Err(refuse(
                        "the previous operation requires recovery before reopening",
                    ));
                }
                return Ok(None);
            }
            if expected_id.is_some() || expected_revision.is_some() {
                return Err(refuse(
                    "no existing preparation matches the expected revision",
                ));
            }
            let source = state.require_ready(active)?;
            next.slot_mut(inactive)?.readiness = Readiness::Incomplete;
            next.slot_mut(inactive)?.generation = None;
            next.previous_good_slot = None;
            next.preparation = Some(Preparation {
                id,
                slot: inactive,
                source_slot: active,
                source_generation: source.id.clone(),
                revision: 0,
                operation: "prepare".into(),
                phase: PreparationPhase::Intent,
                completed_steps: 0,
                failure: None,
                synchronized: false,
                synchronization: None,
            });
            Transition {
                operation: Operation::Prepare(inactive),
                next,
                boot: None,
            }
        }
        Request::Discard { id, revision } => {
            ensure_preparation_allowed(state, sel)?;
            let preparation = state
                .preparation
                .as_ref()
                .ok_or_else(|| refuse("there is no preparation to discard"))?;
            check_preparation(preparation, sel, state, Some(&id), Some(revision))?;
            if !committed(state, sel) || sel.oneshot.is_some() {
                return Err(refuse(
                    "cannot discard while a trial is pending or uncommitted",
                ));
            }
            next.preparation = None;
            next.slot_mut(inactive)?.readiness = Readiness::Incomplete;
            next.slot_mut(inactive)?.generation = None;
            Transition {
                operation: Operation::Discard(inactive),
                next,
                boot: None,
            }
        }
        Request::Stage { id } => {
            if state.preparation.is_some() {
                return Err(refuse(
                    "a preparation already exists; release staging cannot discard its pending work",
                ));
            }
            if !committed(state, sel) {
                return Err(refuse(format!(
                    "slot {active} is running uncommitted, so slot {inactive} is the fallback; staging over it would leave no known-good system.\n\
Run 'recab commit' to keep slot {active}, or reboot into slot {inactive}."
                )));
            }
            if sel.next_boot() == Some(inactive) {
                return Err(refuse(format!(
                    "the next boot is slot {inactive}; run 'recab rollback' (or 'recab commit') so it is slot {active} before staging slot {inactive}"
                )));
            }
            if sel.foreign_default {
                return Err(refuse(format!(
                    "systemd-boot boots another entry by default; run 'recab commit' so it is slot {active} before staging slot {inactive}"
                )));
            }
            // After a one-shot boot of the running slot, the freshly written
            // slot would boot as the default, untried.
            if sel.default == Some(inactive) {
                return Err(refuse(format!(
                    "the default is slot {inactive}; run 'recab commit' so it is slot {active} before staging slot {inactive}"
                )));
            }
            let source = state.require_ready(active)?;
            next.preparation = Some(Preparation {
                id,
                slot: inactive,
                source_slot: active,
                source_generation: source.id.clone(),
                revision: 0,
                operation: "stage".into(),
                phase: PreparationPhase::Staging,
                completed_steps: 0,
                failure: None,
                synchronized: false,
                synchronization: None,
            });
            next.slot_mut(inactive)?.readiness = Readiness::Incomplete;
            next.slot_mut(inactive)?.generation = None;
            next.pending_trial_generation = None;
            next.known_good_slot = Some(active);
            // A new system in the slot ends any trial scheduled before.
            next.pending_trial_slot = None;
            // Its system is about to be replaced: nothing to roll back to.
            if next.previous_good_slot == Some(inactive) {
                next.previous_good_slot = None;
            }
            Transition {
                operation: Operation::Stage(inactive),
                next,
                boot: None,
            }
        }
    };
    Ok(Some(transition))
}

fn ensure_preparation_allowed(state: &PersistentState, sel: &Selection) -> Result<()> {
    if !committed(state, sel) {
        return Err(refuse(
            "the running slot is uncommitted; its fallback cannot be prepared",
        ));
    }
    if sel.oneshot.is_some() || state.pending_trial_slot.is_some() {
        return Err(refuse("cancel the pending trial before preparing a system"));
    }
    if sel.default != Some(sel.active) || sel.foreign_default {
        return Err(refuse(
            "the committed active generation must be the default before preparation",
        ));
    }
    Ok(())
}
fn check_preparation(
    preparation: &Preparation,
    sel: &Selection,
    state: &PersistentState,
    id: Option<&str>,
    revision: Option<u64>,
) -> Result<()> {
    if preparation.source_slot != sel.active
        || preparation.slot != sel.active.other()
        || state
            .slot(sel.active)
            .and_then(|slot| slot.generation.as_ref())
            .is_none_or(|source| source.id != preparation.source_generation)
    {
        return Err(refuse(
            "the preparation belongs to a different active generation; it cannot be silently rebased",
        ));
    }
    if id.is_some_and(|id| id != preparation.id)
        || revision.is_some_and(|revision| revision != preparation.revision)
    {
        return Err(refuse("stale preparation id or revision"));
    }
    Ok(())
}

/// Keep the running slot: known-good and the default; the slot that was
/// good until now is what rollback returns to.
fn commit(state: &PersistentState, sel: &Selection) -> Transition {
    let active = sel.active;
    let mut next = state.clone();
    let good = known_good(state, sel);
    if good != Some(active) {
        next.previous_good_slot = good;
    }
    next.known_good_slot = Some(active);
    next.pending_trial_slot = None;
    next.pending_trial_generation = None;
    if next
        .preparation
        .as_ref()
        .is_some_and(|preparation| preparation.slot == active)
    {
        next.preparation = None;
    }
    if let Some(record) = next.slots.iter_mut().find(|record| record.slot == active) {
        record.readiness = Readiness::Committed;
    }
    Transition {
        operation: Operation::Commit(active),
        next,
        boot: Some(BootChange::Default(active)),
    }
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

/// What the trial service knows about this boot once startup ended.
pub(crate) struct TrialInput {
    pub startup: Startup,
    /// The firmware booted a slot other than its default (a one-shot), from
    /// the firmware alone; `None` when its variables name no two slots.
    pub firmware_trial: Option<bool>,
    /// How startup ended, in words, for the record.
    pub detail: String,
    pub unix_time: u64,
}

/// What recab could read of the slots when it judges a boot.
pub(crate) struct TrialView<'a> {
    pub sel: &'a Selection,
    pub state: &'a PersistentState,
    /// recab had written state (it lives on /var, which may be what broke).
    pub state_found: bool,
}

/// Where a failed trial reboots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reboot {
    /// Into the known-good slot (made the default first if it is not).
    Into(Slot),
    /// The slots could not be read: into the firmware's default.
    FirmwareDefault,
    /// Not at all, and why.
    Not(String),
}

/// What a boot's outcome calls for.
pub(crate) enum Judgment {
    /// Not a trial recab scheduled, or no verdict: nothing to do.
    Ignore,
    /// A transition with nothing more to it: a healthy trial committed, or
    /// a scheduled trial forgotten because another boot used its one-shot.
    Change(Transition),
    /// The trial failed: record it (`None` when there is no state to record
    /// in), and reboot.
    Failed {
        transition: Option<Transition>,
        reboot: Reboot,
    },
}

/// Judge a boot. It is a trial when recab scheduled this slot's boot
/// (`set-next`) and has not committed it; a slot picked from the boot menu,
/// or a trial an operator already rolled back, is left alone. `view` is
/// `None` when the ESP or /var could not be read: then the firmware alone
/// tells a one-shot boot, and a failed one reboots into its default.
pub(crate) fn judge(input: &TrialInput, view: Option<TrialView>) -> Judgment {
    if input.startup == Startup::Inconclusive {
        return Judgment::Ignore;
    }
    let failed = input.startup != Startup::Running;
    let one_shot = input.firmware_trial == Some(true);
    let Some(TrialView {
        sel,
        state,
        state_found,
    }) = view
    else {
        return if failed && one_shot {
            Judgment::Failed {
                transition: None,
                reboot: Reboot::FirmwareDefault,
            }
        } else {
            Judgment::Ignore
        };
    };
    if state.check_mutable().is_err() {
        return if failed && one_shot {
            Judgment::Failed {
                transition: None,
                reboot: Reboot::FirmwareDefault,
            }
        } else {
            Judgment::Ignore
        };
    }
    let active = sel.active;
    let is_trial = state.pending_trial_slot == Some(active)
        && !committed(state, sel)
        && state.pending_trial_generation.as_deref() == sel.active_generation.as_deref()
        && state.pending_trial_generation.is_some();
    if !is_trial {
        // No state on /var but a one-shot boot that failed: /var itself may
        // be what failed to mount; there is nothing to record it in.
        if failed && !state_found && one_shot {
            return Judgment::Failed {
                transition: None,
                reboot: Reboot::FirmwareDefault,
            };
        }
        // Another boot used up the one-shot of the trial recab scheduled: a
        // later boot of that slot from the menu is no trial.
        if let Some(scheduled) = state.pending_trial_slot.filter(|slot| *slot != active) {
            if sel.oneshot.is_none() {
                let mut next = state.clone();
                next.pending_trial_slot = None;
                next.pending_trial_generation = None;
                return Judgment::Change(Transition {
                    operation: Operation::TrialMissed(scheduled),
                    next,
                    boot: None,
                });
            }
        }
        return Judgment::Ignore;
    }
    let record = TrialRecord {
        slot: active,
        committed: !failed,
        detail: input.detail.clone(),
        unix_time: input.unix_time,
    };
    if !failed {
        let mut transition = commit(state, sel);
        transition.next.last_trial = Some(record);
        return Judgment::Change(transition);
    }
    let mut next = state.clone();
    next.pending_trial_slot = None;
    next.last_trial = Some(record);
    next.pending_trial_generation = None;
    let reboot = match known_good(state, sel).filter(|slot| *slot != active) {
        None => Reboot::Not("there is no known-good slot to return to".to_string()),
        Some(slot) if !sel.can_boot(slot) => {
            Reboot::Not(format!("the known-good slot {slot} cannot boot"))
        }
        Some(slot) => Reboot::Into(slot),
    };
    let boot = match reboot {
        Reboot::Into(slot) if sel.next_boot() != Some(slot) => Some(BootChange::Default(slot)),
        _ => None,
    };
    Judgment::Failed {
        transition: Some(Transition {
            operation: Operation::TrialFailed(active),
            next,
            boot,
        }),
        reboot,
    }
}
