//! `recab trial`, run by the trial service on every boot: once startup
//! ended, commit a healthy trial boot of the slot `set-next` scheduled, or
//! record a failed one and reboot into the known-good slot. The verdict is
//! the state machine's ([`judge`]); this applies it.

use anyhow::Result;
use std::time::Duration;

use crate::error::{classified, ErrorClass};
use crate::init_system;
use crate::state::{judge, Judgment, Operation, Reboot, Slot, Startup, TrialInput, TrialView};
use crate::store::now;
use crate::system::{lock, System};
use crate::systemd_boot::SystemdBoot;
use crate::target::Target;

/// What `trial` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialOutcome {
    /// Not a trial recab scheduled, or no verdict: nothing done (beyond
    /// forgetting a trial another boot used up).
    NotATrial,
    /// The trial booted healthy and was committed.
    Committed(Slot),
}

/// Decide this boot once startup has finished (at most `timeout`).
pub fn trial(timeout: Duration) -> Result<TrialOutcome> {
    let target = Target::running()?;
    // Startup first: by then the ESP and /var are mounted, if they mount.
    let health = init_system::health(&target, timeout)?;
    if health.startup == Startup::Inconclusive {
        return Ok(TrialOutcome::NotATrial);
    }
    // Every image runs the trial service. One that is no A/B system, with
    // no one-shot boot in the firmware, has nothing to judge (a live or
    // mutable system); this needs neither root nor the lock.
    let backend = SystemdBoot::new(&target);
    let firmware_trial = backend.firmware_trial()?;
    if !matches!(backend.is_ab(), Ok(true)) && firmware_trial != Some(true) {
        return Ok(TrialOutcome::NotATrial);
    }
    let input = TrialInput {
        startup: health.startup,
        firmware_trial,
        detail: health.describe(timeout),
        unix_time: now()?,
    };
    let lock = lock(&target, "trial")?;
    let init = target.init()?;
    // The ESP or /var may be what broke: then the firmware alone tells.
    let sys = System::read_locked(target.clone(), lock);
    let view = sys.as_ref().ok().map(|sys| TrialView {
        sel: &sys.boot.selection,
        state: &sys.state,
        state_found: sys.state_found,
    });
    let fail = |message: String| classified(ErrorClass::TrialFailed, message);
    match judge(&input, view) {
        Judgment::Ignore => sys.map(|_| TrialOutcome::NotATrial),
        Judgment::Change(transition) => {
            sys?.apply(&transition)?;
            Ok(match transition.operation {
                Operation::Commit(slot) => TrialOutcome::Committed(slot),
                _ => TrialOutcome::NotATrial,
            })
        }
        Judgment::Failed { transition, reboot } => {
            let what = match &sys {
                Ok(sys) => format!("the trial boot of slot {}", sys.boot.selection.active),
                Err(_) => "this one-shot trial boot".to_string(),
            };
            let detail = &input.detail;
            // Recorded and its fallback selected before the reboot; a failure
            // here must not keep a broken slot running.
            if let (Some(transition), Ok(sys)) = (&transition, &sys) {
                if let Err(err) = sys.apply(transition) {
                    eprintln!("recab: warning: the failed trial was not recorded: {err:#}");
                }
            }
            match reboot {
                Reboot::Into(slot) => {
                    init_system::reboot(init, &target)?;
                    Err(fail(format!("{what} {detail}; not committing it. Rebooting into slot {slot}.")))
                }
                Reboot::FirmwareDefault => {
                    init_system::reboot(init, &target)?;
                    let why = sys.err().map(|err| format!(" ({err:#})")).unwrap_or_default();
                    Err(fail(format!(
                        "{what} {detail}, and recab's slots or state could not be read{why}; rebooting into the firmware's default entry"
                    )))
                }
                Reboot::Not(why) => Err(fail(format!("{what} {detail}; not committing it, and not rebooting: {why}"))),
            }
        }
    }
}
