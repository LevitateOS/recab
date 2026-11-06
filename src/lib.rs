//! recab - A/B slot management for systemd-boot systems.
//!
//! - which slot booted, which boots next (`status`)
//! - write a prebuilt system into the inactive slot (`stage`)
//! - boot the other slot once (`set_next`)
//! - keep the running slot (`commit`), or go back (`rollback`)
//! - on each boot, commit a healthy trial or reboot out of a failed one (`trial`)
//!
//! [`state`] holds the slot state machine without I/O; [`systemd_boot`] reads
//! and writes the boot selection; [`store`] keeps recab's state and audit log.

mod error;
mod stage;
mod state;
mod store;
mod systemd_boot;
mod trial;
mod unit;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use error::{
    stable_exit_code, EXIT_CODE_BACKEND_UNAVAILABLE, EXIT_CODE_GENERIC, EXIT_CODE_PARTIAL_UPDATE,
    EXIT_CODE_PERMISSION, EXIT_CODE_STATE_OR_DETECTION, EXIT_CODE_TRIAL_FAILED,
};
pub use unit::UNIT_NAME;

use error::{tagged_err, ErrorClass};
use state::{committed, known_good, plan, rollback_target, BootChange, PersistentState, Request, Transition};
use store::Store;
use systemd_boot::{ApplyError, Boot, SystemdBoot};
use trial::{Startup, Systemd};

/// System slot identifier (A/B).
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum Slot {
    #[value(name = "A")]
    A,
    #[value(name = "B")]
    B,
}

impl Slot {
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    pub fn as_letter(self) -> &'static str {
        match self {
            Slot::A => "A",
            Slot::B => "B",
        }
    }

    /// The slot's suffix in entry and directory names (`<prefix>-a`).
    pub(crate) fn entry_suffix(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }
}

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_letter())
    }
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
    pub entry_prefix: String,
}

/// The prebuilt system `stage` writes.
#[derive(Debug, Clone)]
pub struct StageRequest {
    /// EROFS rootfs image (a file or a partition), extracted with recstrap.
    pub rootfs: PathBuf,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
}

/// What `trial` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialOutcome {
    /// The running slot was already committed.
    NotATrial,
    /// The trial booted healthy and was committed.
    Committed(Slot),
}

/// One recab run's view: backend, store, boot selection and state.
struct System {
    backend: SystemdBoot,
    store: Store,
    boot: Boot,
    state: PersistentState,
}

impl System {
    fn load() -> Result<Self> {
        let backend = SystemdBoot::from_env()?;
        let store = Store::from_env();
        let boot = backend.read()?;
        let state = store.load()?;
        Ok(Self { backend, store, boot, state })
    }
}

/// Query current A/B state from the boot configuration and recab's state.
/// Only reads, so it works anywhere the files are readable.
pub fn status() -> Result<Status> {
    let sys = System::load()?;
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
        entry_prefix: sys.boot.prefix,
    })
}

/// Write a prebuilt system into the inactive slot: format it, extract the
/// rootfs image, carry the machine's configuration over, and install the
/// slot's kernel and boot entry. The slot is not booted until `set_next`.
pub fn stage(request: &StageRequest) -> Result<()> {
    let sys = System::load()?;
    let transition = plan(Request::Stage, &sys.boot.selection, &sys.state)?
        .expect("stage always writes the inactive slot");
    require_root("stage")?;
    let slot = sys.boot.selection.active.other();
    stage::write_slot(&sys.backend, &sys.boot, slot, request)?;
    let next = stamped(&transition)?;
    sys.store.save(&next, "stage")?;
    sys.store
        .audit(transition.operation, sys.boot.selection.active, &sys.state, &next);
    Ok(())
}

/// Configure a one-shot trial boot of the inactive `slot`.
pub fn set_next(slot: Slot) -> Result<()> {
    change(Request::SetNext(slot))
}

/// Mark the running slot as known-good and make it the default.
pub fn commit() -> Result<()> {
    change(Request::Commit)
}

/// Configure the next boot back to the known-good slot.
pub fn rollback() -> Result<()> {
    change(Request::Rollback)
}

/// Decide a trial boot once startup has finished (at most `timeout`): commit
/// it when no unit failed, otherwise make the known-good slot the next boot
/// and reboot into it. A committed slot is left alone.
pub fn trial(timeout: Duration) -> Result<TrialOutcome> {
    let systemd = Systemd::from_env();
    let startup = systemd.wait_for_startup(timeout)?;
    let sys = System::load()?;
    let sel = sys.boot.selection;
    if committed(&sys.state, &sel) {
        return Ok(TrialOutcome::NotATrial);
    }
    let active = sel.active;
    let problem = match startup {
        Startup::Running => {
            commit()?;
            return Ok(TrialOutcome::Committed(active));
        }
        Startup::Finished(state) => format!("finished starting in state '{state}'"),
        Startup::TimedOut => format!("did not finish starting within {}s", timeout.as_secs()),
    };
    let failed = systemd.failed_units();
    let failed = if failed.is_empty() {
        String::new()
    } else {
        format!(" (failed units: {})", failed.join(", "))
    };
    let fail = |message: String| tagged_err(ErrorClass::TrialFailed, message);
    let Some(fallback) = known_good(&sys.state, &sel).filter(|slot| *slot != active) else {
        return Err(fail(format!(
            "the trial boot of slot {active} {problem}{failed}; not committing it, and there is no known-good slot to return to"
        )));
    };
    if sel.next_boot() != Some(fallback) {
        rollback()?;
    }
    systemd.reboot()?;
    Err(fail(format!(
        "the trial boot of slot {active} {problem}{failed}; not committing it. Rebooting into slot {fallback}."
    )))
}

/// Install and enable `recab-trial.service` under `root` (`/` for this
/// system, or an install target).
pub fn install_unit(root: &Path) -> Result<()> {
    unit::install(root)
}

fn require_root(operation: &str) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(tagged_err(
            ErrorClass::Permission,
            format!("{operation} must run as root"),
        ));
    }
    Ok(())
}

/// The transition's next state, stamped with the operation and time.
fn stamped(transition: &Transition) -> Result<PersistentState> {
    let mut next = transition.next.clone();
    next.last_operation = Some(transition.operation.label());
    next.last_transition_unix = Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow::anyhow!("system clock before UNIX_EPOCH"))?
            .as_secs(),
    );
    Ok(next)
}

/// Run a boot-selection request. The state file is written first; if the
/// boot change then fails before a firmware variable changed, the state is
/// restored, otherwise the result is a reported partial update.
fn change(request: Request) -> Result<()> {
    let sys = System::load()?;
    let Some(transition) = plan(request, &sys.boot.selection, &sys.state)? else {
        return Ok(());
    };
    if let BootChange::OneShot(slot) = transition.boot {
        sys.backend.ensure_bootable(&sys.boot, slot)?;
    }
    let name = transition.operation.name();
    require_root(name)?;
    sys.backend.ensure_can_change()?;

    let next = stamped(&transition)?;
    sys.store.save(&next, name)?;
    match sys.backend.apply(&sys.boot, transition.boot) {
        Ok(()) => {}
        Err(ApplyError::Untouched(cause)) => {
            return match sys.store.save(&sys.state, &format!("{name}-revert")) {
                Ok(()) => Err(cause.context(format!(
                    "{name} aborted; the boot selection and recab's state are unchanged"
                ))),
                Err(revert) => Err(tagged_err(
                    ErrorClass::PartialUpdate,
                    format!(
                        "{name} failed and restoring recab's state also failed; the boot selection is unchanged.\n\
primary error: {cause:#}\nrestore error: {revert:#}"
                    ),
                )),
            };
        }
        Err(ApplyError::FirmwareChanged(cause)) => {
            return Err(tagged_err(
                ErrorClass::PartialUpdate,
                format!(
                    "{name} changed systemd-boot's firmware variables and then failed; recab's state and loader.conf keep the new selection.\nerror: {cause:#}"
                ),
            ));
        }
        Err(ApplyError::RestoreFailed { cause, restore }) => {
            return Err(tagged_err(
                ErrorClass::PartialUpdate,
                format!(
                    "{name} failed and loader.conf could not be put back; recab's state keeps the new selection.\n\
primary error: {cause:#}\nrestore error: {restore:#}"
                ),
            ));
        }
    }
    sys.store
        .audit(transition.operation, sys.boot.selection.active, &sys.state, &next);
    Ok(())
}
