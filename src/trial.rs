//! Assess the scheduled generation, and recover only to its checked fallback.
use crate::error::{ErrorClass, classified};
use crate::init_system;
use crate::state::{
    Judgment, Operation, Reboot, Slot, Startup, TrialInput, TrialRecord, TrialView, judge,
};
use crate::store::{Store, now};
use crate::system::{System, lock};
use crate::systemd_boot::SystemdBoot;
use crate::target::Target;
use crate::trial_record::Ticket;
use anyhow::{Context, Result};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialOutcome {
    NotATrial,
    Committed(Slot),
}

pub fn trial(timeout: Duration) -> Result<TrialOutcome> {
    let target = Target::running()?;
    // Only an explicit shutdown has no verdict. Collection errors must reach
    // recovery once generation/firmware facts confirm this trial.
    let (startup, detail) = match init_system::health(&target, timeout) {
        Ok(health) => (health.startup, health.describe(timeout)),
        Err(err) => (
            Startup::Failed,
            format!("health assessment failed: {err:#}"),
        ),
    };
    if startup == Startup::Inconclusive {
        return Ok(TrialOutcome::NotATrial);
    }
    let backend = SystemdBoot::new(&target);
    let ticket = Ticket::read(&target)?;
    if ticket.is_none() && !matches!(backend.is_ab(), Ok(true)) {
        return Ok(TrialOutcome::NotATrial);
    }
    let mut held = Some(lock(&target, "trial")?);
    // Never read stale state through an unmounted /var directory, or recreate
    // state there. Keep the run lock even when the durable ledger is absent.
    let mounted = Store::new(&target).require_mounted();
    let sys = mounted.and_then(|()| {
        System::read_locked(target.clone(), held.take().unwrap()).and_then(|sys| {
            anyhow::ensure!(sys.state_found, "durable recab state is missing");
            sys.state.check_mutable()?;
            Ok(sys)
        })
    });
    if held.is_none() && sys.is_err() {
        held = Some(lock(&target, "trial recovery")?);
    }
    let _held = held;
    let ticket = Ticket::read(&target)?;
    let input = TrialInput {
        startup,
        detail,
        unix_time: now()?,
    };
    if let Some(mut ticket) = ticket {
        let root = backend.running_root()?;
        let current = if root == ticket.candidate.partuuid {
            &ticket.candidate
        } else if root == ticket.fallback.partuuid {
            &ticket.fallback
        } else {
            anyhow::bail!(
                "trial recovery ticket names neither running partition; refusing generation substitution"
            )
        };
        confirm_running(&target, current)?;
        // The ledger may have recorded failure while EFI writes failed. Bind
        // that verdict to this attempt, so an explicit later retry of the same
        // generation does not inherit a historical failure.
        if let Ok(sys) = &sys {
            if let Some(record) = sys.state.last_trial.as_ref().filter(|r| {
                !r.committed
                    && r.attempt.as_deref() == Some(ticket.id.as_str())
                    && r.generation == ticket.candidate.generation.as_ref().map(|g| g.id.clone())
                    && r.fallback_generation
                        == ticket.fallback.generation.as_ref().map(|g| g.id.clone())
            }) {
                ticket.assessment = Some(record.clone());
            }
        }
        if current.slot == ticket.fallback.slot {
            // A consumed/missed one-shot or a recovered failure: reconcile only
            // this ticket's generations; do not overwrite an unrelated ledger.
            if let Ok(sys) = &sys {
                anyhow::ensure!(
                    ticket.matches(&sys.state),
                    "trial recovery generations differ from durable state"
                );
                if sys.boot.selection.oneshot == Some(ticket.candidate.slot) {
                    return Ok(TrialOutcome::NotATrial);
                }
                crate::filesystem::private_mounts()?;
                crate::finalize::verify_slot(sys, ticket.fallback.slot)?;
                let mut next = sys.state.clone();
                next.pending_trial_slot = None;
                next.pending_trial_generation = None;
                next.known_good_slot = Some(ticket.fallback.slot);
                let operation =
                    if let Some(record) = ticket.assessment.clone().filter(|r| !r.committed) {
                        next.last_trial = Some(record);
                        next.previous_good_slot = None;
                        Operation::TrialFailed(ticket.candidate.slot)
                    } else {
                        Operation::TrialMissed(ticket.candidate.slot)
                    };
                let boot = if sys.boot.selection.next_boot() == Some(ticket.fallback.slot) {
                    None
                } else {
                    Some(crate::state::BootChange::Default(ticket.fallback.slot))
                };
                sys.apply(&crate::state::Transition {
                    operation,
                    next,
                    boot,
                })?;
                Ticket::clear(&target)?;
                return Ok(TrialOutcome::NotATrial);
            }
            return Err(classified(
                ErrorClass::TrialFailed,
                "fallback generation is running, but state is unavailable; recovery ticket retained",
            ));
        }
        let selected = backend.firmware_selected()?;
        anyhow::ensure!(
            selected == Some(ticket.candidate.slot),
            "firmware selection does not confirm the scheduled trial"
        );
        if let Ok(sys) = &sys {
            anyhow::ensure!(
                ticket.matches(&sys.state),
                "trial recovery generations differ from durable state; refusing to commit or select a reused slot"
            );
        }
        // A failure is latched until the checked fallback runs. Retrying the
        // command cannot turn that same failed trial into a success.
        let failed = ticket.assessment.as_ref().is_some_and(|r| !r.committed);
        if input.startup == Startup::Running && !failed {
            if let Ok(sys) = &sys {
                let mut state = sys.state.clone();
                state.pending_trial_slot = Some(ticket.candidate.slot);
                state.pending_trial_generation =
                    ticket.candidate.generation.as_ref().map(|g| g.id.clone());
                let view = TrialView {
                    sel: &sys.boot.selection,
                    state: &state,
                };
                if let Judgment::Change(mut transition) = judge(&input, view) {
                    if let Some(record) = &mut transition.next.last_trial {
                        record.attempt = Some(ticket.id.clone());
                    }
                    // The durable ticket survives any partial EFI/state write.
                    ticket.assessment = transition.next.last_trial.clone();
                    match ticket.write(&target).and_then(|_| sys.apply(&transition)) {
                        Ok(_) => return Ok(TrialOutcome::Committed(ticket.candidate.slot)),
                        Err(err) => {
                            let record = assessment(
                                &ticket,
                                false,
                                format!("healthy assessment could not be committed: {err:#}"),
                                input.unix_time,
                            );
                            ticket.assessment = Some(record);
                        }
                    }
                }
            }
        }
        if ticket.assessment.as_ref().is_none_or(|r| r.committed) {
            ticket.assessment = Some(assessment(
                &ticket,
                false,
                if let Err(err) = &sys {
                    format!("{}; trial metadata unavailable: {err:#}", input.detail)
                } else {
                    input.detail.clone()
                },
                input.unix_time,
            ));
        }
        // Failure to persist the verdict is reported, but a verified fallback
        // selection still permits recovery. The one-shot has already been used.
        if let Err(err) = ticket.write(&target) {
            eprintln!("recab: warning: recovery verdict was not saved in firmware: {err:#}");
        }
        return recover(&target, &backend, &ticket, sys.as_ref().ok());
    }
    // Compatibility with generation-aware ledgers scheduled before tickets.
    // Never reboot from a slot-only firmware guess.
    let sys = sys?;
    let view = TrialView {
        sel: &sys.boot.selection,
        state: &sys.state,
    };
    match judge(&input, view) {
        Judgment::Ignore => Ok(TrialOutcome::NotATrial),
        Judgment::Change(transition) => {
            if matches!(transition.operation, Operation::Commit(_)) {
                let fallback = crate::state::known_good(&sys.state, &sys.boot.selection)
                    .filter(|slot| *slot != sys.boot.selection.active)
                    .context("trial has no protected fallback")?;
                crate::filesystem::private_mounts()?;
                crate::finalize::verify_slot(&sys, fallback)?;
            }
            sys.apply(&transition)?;
            Ok(match transition.operation {
                Operation::Commit(slot) => TrialOutcome::Committed(slot),
                _ => TrialOutcome::NotATrial,
            })
        }
        Judgment::Failed { transition, reboot } => {
            let slot = match reboot {
                Reboot::Into(slot) => slot,
                Reboot::Not(why) => return Err(classified(ErrorClass::TrialFailed, why)),
            };
            let mut ticket = Ticket::recovering(&sys, slot)?;
            ticket.fallback = sys.state.slot(slot).context("fallback missing")?.clone();
            ticket.assessment = transition.as_ref().and_then(|t| t.next.last_trial.clone());
            if let Some(record) = &mut ticket.assessment {
                record.attempt = Some(ticket.id.clone());
            }
            ticket.write(&target)?;
            recover(&target, &backend, &ticket, Some(&sys))
        }
    }
}
fn assessment(ticket: &Ticket, committed: bool, detail: String, unix_time: u64) -> TrialRecord {
    TrialRecord {
        slot: ticket.candidate.slot,
        attempt: Some(ticket.id.clone()),
        generation: ticket.candidate.generation.as_ref().map(|g| g.id.clone()),
        fallback_generation: ticket.fallback.generation.as_ref().map(|g| g.id.clone()),
        committed,
        detail,
        unix_time,
    }
}
fn confirm_running(target: &Target, record: &crate::SlotState) -> Result<()> {
    let text = std::fs::read_to_string(target.cmdline_path())?;
    let generation = record
        .generation
        .as_ref()
        .context("ticket generation missing")?;
    for (prefix, expected) in [
        ("root=", format!("root=PARTUUID={}", record.partuuid)),
        ("recab.slot=", format!("recab.slot={}", record.slot)),
        (
            "recab.generation=",
            format!("recab.generation={}", generation.id),
        ),
    ] {
        let values = text
            .split_whitespace()
            .filter(|s| s.starts_with(prefix))
            .collect::<Vec<_>>();
        anyhow::ensure!(
            values == [expected.as_str()],
            "running root/slot/generation does not match scheduled trial"
        );
    }
    // A damaged marker/inventory must prevent commit, but cannot prevent
    // recognizing the already-booted trial for recovery. Bind the trusted
    // command line to the actual mounted partition instead.
    use std::os::unix::fs::MetadataExt;
    let mounts = target.mount_table()?;
    let mounted = mounts
        .iter()
        .rev()
        .find(|m| m.mount_point == target.root)
        .context("running system root is not mounted")?;
    anyhow::ensure!(
        std::fs::metadata(&target.root)?.dev() == libc::makedev(mounted.dev.0, mounted.dev.1)
            && crate::target::partuuid(std::path::Path::new(&mounted.source))? == record.partuuid,
        "actual running partition differs from trial recovery ticket"
    );
    Ok(())
}
fn recover(
    target: &Target,
    backend: &SystemdBoot,
    ticket: &Ticket,
    sys: Option<&System>,
) -> Result<TrialOutcome> {
    // A verdict can be durable even when EFI selection fails. Keep the trial
    // pending until selection succeeds; never lose its failed assessment merely
    // because the firmware is temporarily read-only.
    if let Some(sys) = sys {
        let mut checkpoint = sys.state.clone();
        checkpoint.last_trial = ticket.assessment.clone();
        checkpoint.last_operation = Some(Operation::TrialFailed(ticket.candidate.slot).label());
        checkpoint.last_transition_unix = Some(now()?);
        if let Err(err) = sys.store.save(&checkpoint) {
            eprintln!("recab: warning: failed assessment checkpoint was not saved: {err:#}");
        }
    }
    let result = (|| -> Result<()> {
        crate::filesystem::private_mounts()?;
        let fallback = &ticket.fallback;
        let generation = fallback
            .generation
            .as_ref()
            .context("fallback generation missing")?;
        let mounted =
            crate::slot_mount::SlotMount::installed(target, &fallback.partuuid, &generation.id)?;
        generation.verify_root(&mounted.point, fallback.slot, &fallback.partuuid)?;
        crate::candidate::runtime_protocol(&crate::filesystem::Root::open(&mounted.point)?)?;
        if fallback.seal.is_some() {
            crate::finalize::check_record(
                &mounted.point,
                fallback.slot,
                &fallback.partuuid,
                &ticket.candidate.partuuid,
                generation,
            )?;
        }
        mounted.seal()?;
        // With the ESP mounted, validate the exact fallback payload before any
        // selection. With the ESP absent, only the ticket's unchanged firmware
        // default and a consumed one-shot can authorize a reboot.
        let esp_mounted = target
            .mount_table()?
            .iter()
            .any(|m| m.mount_point == target.esp && m.fstype == "vfat");
        if esp_mounted {
            let boot = backend.read(&ticket.state())?;
            backend.verify_generation(&boot, fallback.slot, generation)?;
            if let Some(seal) = &fallback.seal {
                backend.verify_manifest(generation, seal)?;
            }
            let (_, default, oneshot) = backend.firmware_slots()?;
            if default != Some(fallback.slot) || oneshot {
                backend
                    .select(
                        &boot,
                        crate::state::BootChange::Default(fallback.slot),
                        || target.init(),
                    )
                    .map_err(|err| crate::system::select_failed("trial fallback", err))?;
            }
            let after = backend.firmware_slots()?;
            anyhow::ensure!(
                after.1 == Some(fallback.slot) && !after.2,
                "fallback EFI selection was not confirmed"
            );
        } else {
            let (_, default, oneshot) = backend.firmware_slots()?;
            anyhow::ensure!(
                default == Some(fallback.slot) && !oneshot,
                "ESP unavailable and firmware no longer selects the checked fallback"
            );
        }
        if let Some(sys) = sys {
            let mut next = sys.state.clone();
            next.schema = crate::state::StateSchema::default();
            next.pending_trial_slot = None;
            next.pending_trial_generation = None;
            next.known_good_slot = Some(fallback.slot);
            next.previous_good_slot = None;
            next.last_trial = ticket.assessment.clone();
            next.last_operation = Some(Operation::TrialFailed(ticket.candidate.slot).label());
            next.last_transition_unix = Some(now()?);
            match sys.store.save(&next) {
                Ok(()) => sys.store.audit(
                    Operation::TrialFailed(ticket.candidate.slot),
                    ticket.candidate.slot,
                    &sys.state,
                    &next,
                ),
                Err(err) => eprintln!(
                    "recab: warning: failed trial state was not saved; recovery ticket retained: {err:#}"
                ),
            }
        }
        init_system::reboot(target.init()?, target)?;
        Ok(())
    })();
    let detail = ticket
        .assessment
        .as_ref()
        .map(|r| r.detail.as_str())
        .unwrap_or("trial assessment failed");
    Err(classified(
        ErrorClass::TrialFailed,
        match result {
            Ok(()) => format!(
                "trial of slot {} generation {} {detail}; Rebooting into slot {} generation {}.",
                ticket.candidate.slot,
                ticket.candidate.generation.as_ref().unwrap().id,
                ticket.fallback.slot,
                ticket.fallback.generation.as_ref().unwrap().id
            ),
            Err(err) => format!(
                "{detail}; not rebooting: checked fallback recovery failed: {err:#}; recovery ticket retained"
            ),
        },
    ))
}
