//! A small generation-bound recovery ticket, written before the one-shot EFI
//! selection. Kept in firmware so losing /var or the ESP cannot erase which
//! generations were scheduled. This is transaction state, never an event log.
use crate::efivars::{EfiVars, Var};
use crate::state::{PersistentState, SlotState, TrialRecord};
use crate::system::System;
use crate::target::Target;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Ticket {
    version: u32,
    pub id: String,
    pub candidate: SlotState,
    pub fallback: SlotState,
    pub assessment: Option<TrialRecord>,
}
impl Ticket {
    pub fn scheduled(sys: &System, slot: crate::Slot) -> Result<Self> {
        crate::finalize::verify_slot(sys, sys.boot.selection.active)?;
        Ok(Self {
            version: 1,
            id: crate::prepare::new_id()?,
            candidate: sys.state.slot(slot).context("candidate missing")?.clone(),
            fallback: sys
                .state
                .slot(sys.boot.selection.active)
                .context("fallback missing")?
                .clone(),
            assessment: None,
        })
    }
    pub fn recovering(sys: &System, fallback: crate::Slot) -> Result<Self> {
        Ok(Self {
            version: 1,
            id: crate::prepare::new_id()?,
            candidate: sys
                .state
                .slot(sys.boot.selection.active)
                .context("candidate missing")?
                .clone(),
            fallback: sys
                .state
                .slot(fallback)
                .context("fallback missing")?
                .clone(),
            assessment: None,
        })
    }
    pub fn read(target: &Target) -> Result<Option<Self>> {
        EfiVars::new(target.firmware.clone())
            .get(Var::RecabTrial)?
            .map(|s| -> Result<Self> {
                anyhow::ensure!(s.len() <= 8192, "oversized trial recovery ticket");
                let ticket: Self = serde_json::from_str(&s)?;
                anyhow::ensure!(
                    ticket.version == 1
                        && ticket.candidate.slot != ticket.fallback.slot
                        && ticket.id.len() == 64
                        && ticket.id.bytes().all(|b| b.is_ascii_hexdigit()),
                    "unsupported trial recovery ticket"
                );
                let state = ticket.state();
                state.check_mutable()?;
                if let Some(record) = &ticket.assessment {
                    anyhow::ensure!(
                        record.attempt.as_deref() == Some(ticket.id.as_str())
                            && record.slot == ticket.candidate.slot
                            && record.generation.as_deref()
                                == ticket.candidate.generation.as_ref().map(|g| g.id.as_str())
                            && record.fallback_generation.as_deref()
                                == ticket.fallback.generation.as_ref().map(|g| g.id.as_str()),
                        "trial assessment does not match recovery generations"
                    );
                }
                Ok(ticket)
            })
            .transpose()
    }
    pub fn write(&self, target: &Target) -> Result<()> {
        self.state().check_mutable()?;
        let mut ticket = self.clone();
        if let Some(record) = &mut ticket.assessment {
            if record.detail.len() > 512 {
                let mut end = 512;
                while !record.detail.is_char_boundary(end) {
                    end -= 1;
                }
                record.detail.truncate(end);
                record
                    .detail
                    .push_str(" [diagnostic truncated in firmware]");
            }
        }
        let text = serde_json::to_string(&ticket)?;
        anyhow::ensure!(
            text.len() <= 8192,
            "trial recovery ticket exceeds firmware budget"
        );
        EfiVars::new(target.firmware.clone()).set(Var::RecabTrial, Some(&text))
    }
    pub fn clear(target: &Target) -> Result<()> {
        EfiVars::new(target.firmware.clone()).set(Var::RecabTrial, None)
    }
    pub fn state(&self) -> PersistentState {
        PersistentState {
            slots: vec![self.candidate.clone(), self.fallback.clone()],
            known_good_slot: Some(self.fallback.slot),
            pending_trial_slot: Some(self.candidate.slot),
            pending_trial_generation: self.candidate.generation.as_ref().map(|g| g.id.clone()),
            ..Default::default()
        }
    }
    pub fn matches(&self, state: &PersistentState) -> bool {
        [&self.candidate, &self.fallback].into_iter().all(|record| {
            state.slot(record.slot).is_some_and(|s| {
                s.partuuid == record.partuuid
                    && s.generation == record.generation
                    && s.seal == record.seal
            })
        })
    }
}
