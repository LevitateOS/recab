//! recab - A/B slot management for systemd-boot systems.
//!
//! - set up the A/B boot layout at install time (`init`)
//! - which slot booted, which boots next (`status`)
//! - write a prebuilt system into the inactive slot (`stage`)
//! - boot the other slot once (`set_next`)
//! - keep the running slot (`commit`), or go back (`rollback`)
//! - on each boot, commit a healthy trial or reboot out of a failed one (`trial`)
//!
//! `state` holds the slot state machine without I/O; `systemd_boot` reads and
//! writes the boot selection; `store` keeps recab's state and audit log.

mod error;
mod init;
mod init_system;
mod stage;
mod state;
mod store;
mod systemd_boot;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use error::{
    stable_exit_code, EXIT_CODE_BACKEND_UNAVAILABLE, EXIT_CODE_GENERIC, EXIT_CODE_PARTIAL_UPDATE,
    EXIT_CODE_PERMISSION, EXIT_CODE_STATE_OR_DETECTION, EXIT_CODE_TRIAL_FAILED,
};
pub use distro_profile::InitSystem;
pub use init_system::InitSystemExt;

use error::{tagged_err, ErrorClass};
use state::{
    committed, judge_trial, known_good, plan, rollback_target, BootChange, Operation,
    PersistentState, Request, Startup, Transition, TrialVerdict,
};
use store::{Lock, Store};
use systemd_boot::{ApplyError, Boot, SystemdBoot};

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

/// How the last judged trial boot ended.
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
}

/// The prebuilt system `stage` writes.
#[derive(Debug, Clone)]
pub struct StageRequest {
    /// EROFS rootfs image (a file or a partition), extracted with recstrap.
    pub rootfs: PathBuf,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
}

/// The installed system `init` sets up.
#[derive(Debug, Clone)]
pub struct InitRequest {
    /// The installed system's root, with its ESP mounted at `boot/` and its
    /// state partition at `var/`.
    pub root: PathBuf,
    /// Slot A's partition (holding the installed system) and slot B's.
    pub slot_a: PathBuf,
    pub slot_b: PathBuf,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    /// Extra kernel options for both entries (consoles, for example).
    pub options: String,
}

/// What `trial` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrialOutcome {
    /// Not a trial recab scheduled, or no verdict: nothing done.
    NotATrial,
    /// The trial booted healthy and was committed.
    Committed(Slot),
}

/// One recab run's view, holding the run lock when it may change anything.
struct System {
    backend: SystemdBoot,
    store: Store,
    boot: Boot,
    state: PersistentState,
    _lock: Option<Lock>,
}

impl System {
    /// Read-only view.
    fn read() -> Result<Self> {
        Self::open(false)
    }

    /// View for a change: holds the lock until dropped.
    fn locked() -> Result<Self> {
        Self::open(true)
    }

    fn open(lock: bool) -> Result<Self> {
        let backend = SystemdBoot::from_env()?;
        let store = Store::from_env();
        // Only root changes anything; a run that is not root is refused by
        // its root check later and has nothing to serialise.
        // SAFETY: geteuid has no preconditions.
        let lock = if lock && unsafe { libc::geteuid() } == 0 { Some(store.lock()?) } else { None };
        let boot = backend.read()?;
        let state = store.load()?;
        Ok(Self { backend, store, boot, state, _lock: lock })
    }
}

/// Query current A/B state from the boot configuration and recab's state.
/// Only reads, so it works anywhere the files are readable.
pub fn status() -> Result<Status> {
    let sys = System::read()?;
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
        entry_prefix: sys.boot.prefix,
    })
}

/// Set up the A/B boot layout of a freshly installed system: both slot
/// entries, slot A's kernel, loader.conf, recab's state, the trial service,
/// and systemd-boot's firmware variables pointing at slot A.
pub fn init(request: &InitRequest) -> Result<()> {
    require_root("init")?;
    init::run(request)
}

/// Write a prebuilt system into the inactive slot: format it, extract the
/// rootfs image, carry the machine's configuration over, and install the
/// slot's kernel and boot entry. The slot is not booted until `set_next`.
pub fn stage(request: &StageRequest) -> Result<()> {
    let sys = System::locked()?;
    let transition = plan(Request::Stage, &sys.boot.selection, &sys.state)?
        .expect("stage always writes the inactive slot");
    require_root("stage")?;
    let active = sys.boot.selection.active;
    let slot = active.other();
    let profile = init_system::running_profile()?;
    // A firmware variable systemd-boot skips today, because it names this
    // slot without a kernel, would take effect once the slot is written:
    // stage must be able to read and reset them.
    sys.backend.ensure_can_change()?;
    let inputs = stage::check_inputs(&sys.backend, &sys.boot, slot, request)?;
    // The plan made sure the running slot is the known-good one and the
    // default (if any); this changes nothing systemd-boot boots now.
    sys.backend.reset_stale_selection(&sys.boot, active, slot)?;
    let target = stage::check_target(&sys.backend, &sys.boot, slot, &inputs)?;
    // Recorded before the slot is touched: from here its old system is gone,
    // so nothing may roll back to it, even if the write fails.
    let next = stamped(&transition)?;
    sys.store.save(&next, "stage")?;
    sys.store
        .audit(transition.operation, sys.boot.selection.active, &sys.state, &next);
    stage::write_slot(&sys.backend, &sys.boot, slot, request, &target, &profile)
}

/// Configure a one-shot trial boot of the inactive `slot`.
pub fn set_next(slot: Slot) -> Result<()> {
    change(System::locked()?, Request::SetNext(slot), |_| {})
}

/// Mark the running slot as known-good and make it the default.
pub fn commit() -> Result<()> {
    change(System::locked()?, Request::Commit, |_| {})
}

/// Configure the next boot back to the known-good slot.
pub fn rollback() -> Result<()> {
    change(System::locked()?, Request::Rollback, |_| {})
}

/// Decide a trial boot once startup has finished (at most `timeout`). Only
/// the boot of the slot `set_next` scheduled is judged: committed when no
/// unit failed; otherwise not committed, the known-good slot made the next
/// boot, and the machine rebooted into it.
pub fn trial(timeout: Duration) -> Result<TrialOutcome> {
    let init = init_system::running_profile()?.init;
    let (startup, failed) = init_system::wait_for_startup(init, timeout)?;
    let problem = match &startup {
        Startup::Running => String::new(),
        Startup::Failed(state) => format!("finished starting in state '{state}'"),
        Startup::TimedOut => format!("did not finish starting within {}s", timeout.as_secs()),
        Startup::Inconclusive(_) => return Ok(TrialOutcome::NotATrial),
    };
    let failed = if failed.is_empty() {
        String::new()
    } else {
        format!(" (failed: {})", failed.join(", "))
    };
    let fail = |message: String| tagged_err(ErrorClass::TrialFailed, message);

    let sys = match System::locked() {
        Ok(sys) => sys,
        // The ESP or /var may be what broke. The firmware still knows
        // whether this boot was a one-shot trial, and its default is the
        // fallback.
        Err(err) => {
            let firmware_trial = SystemdBoot::from_env().ok().and_then(|b| b.firmware_trial());
            if startup == Startup::Running || firmware_trial != Some(true) {
                return Err(err);
            }
            require_root("trial")?;
            init.reboot()?;
            return Err(fail(format!(
                "this one-shot trial boot {problem}{failed}, and the slots could not be read ({err:#}); rebooting into the default entry"
            )));
        }
    };
    let sel = sys.boot.selection;
    let active = sel.active;
    let verdict = match judge_trial(&startup, &sel, &sys.state) {
        // No state on /var but a one-shot boot that failed: /var itself may
        // be what failed to mount.
        TrialVerdict::Ignore
            if startup != Startup::Running
                && !sys.store.exists()
                && sys.backend.firmware_trial() == Some(true) =>
        {
            TrialVerdict::Fail
        }
        verdict => verdict,
    };
    let record = |committed: bool, detail: String| -> Result<TrialRecord> {
        Ok(TrialRecord { slot: active, committed, detail, unix_time: now()? })
    };
    match verdict {
        TrialVerdict::Ignore => Ok(TrialOutcome::NotATrial),
        TrialVerdict::Commit => {
            let record = record(true, "startup finished with no failed units".to_string())?;
            change(sys, Request::Commit, |next| next.last_trial = Some(record))?;
            Ok(TrialOutcome::Committed(active))
        }
        TrialVerdict::Fail => {
            require_root("trial")?;
            let detail = format!("{problem}{failed}");
            let mut next = sys.state.clone();
            next.pending_trial_slot = None;
            next.last_trial = Some(record(false, detail.clone())?);
            next.last_operation = Some(Operation::TrialFailed(active).label());
            next.last_transition_unix = Some(now()?);
            if let Err(err) = sys.store.save(&next, "trial") {
                eprintln!("recab: warning: the failed trial was not recorded: {err:#}");
            } else {
                sys.store.audit(Operation::TrialFailed(active), active, &sys.state, &next);
            }
            let Some(fallback) = known_good(&next, &sel).filter(|slot| *slot != active) else {
                return Err(fail(format!(
                    "the trial boot of slot {active} {detail}; not committing it, and there is no known-good slot to return to"
                )));
            };
            if let Err(err) = sys.backend.ensure_bootable(&sys.boot, fallback) {
                return Err(fail(format!(
                    "the trial boot of slot {active} {detail}; not committing it, but not rebooting: the known-good slot {fallback} cannot boot ({err:#})"
                )));
            }
            if sel.next_boot() != Some(fallback) {
                drop(sys);
                rollback()?;
            }
            init.reboot()?;
            Err(fail(format!(
                "the trial boot of slot {active} {detail}; not committing it. Rebooting into slot {fallback}."
            )))
        }
    }
}

/// Install and enable the trial service of the init system under `root`
/// (`/` for this system, or an install target).
pub fn install_unit(root: &Path) -> Result<()> {
    init_system::installed_profile(root)?
        .init
        .install_trial_service(root)
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

fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock before UNIX_EPOCH"))?
        .as_secs())
}

/// The transition's next state, stamped with the operation and time.
fn stamped(transition: &Transition) -> Result<PersistentState> {
    let mut next = transition.next.clone();
    next.last_operation = Some(transition.operation.label());
    next.last_transition_unix = Some(now()?);
    Ok(next)
}

/// Run a boot-selection request on a locked system. The boot selection is
/// written first and recab's state second: after a crash between the two,
/// the machine boots what was asked, and the state is what is stale.
fn change(sys: System, request: Request, annotate: impl FnOnce(&mut PersistentState)) -> Result<()> {
    let Some(transition) = plan(request, &sys.boot.selection, &sys.state)? else {
        return Ok(());
    };
    let active = sys.boot.selection.active;
    // systemd-boot skips an entry it cannot boot: never select one.
    match transition.boot {
        BootChange::OneShot(slot) | BootChange::Default(slot) if slot != active => {
            sys.backend.ensure_bootable(&sys.boot, slot)?;
        }
        _ => {}
    }
    let name = transition.operation.name();
    require_root(name)?;
    sys.backend.ensure_can_change()?;
    // A trial falls back to the default the firmware variable names: make it
    // the known-good default `plan` checked, not a stale value it skipped.
    if let (BootChange::OneShot(slot), Some(default)) = (transition.boot, sys.boot.selection.default) {
        sys.backend.reset_stale_selection(&sys.boot, default, slot)?;
    }

    let mut next = stamped(&transition)?;
    annotate(&mut next);
    // Without a known init system a trial still gets panic=, not its options.
    let trial_options = init_system::running_profile()
        .map(|profile| profile.init.trial_options())
        .unwrap_or(&[]);
    match sys.backend.apply(&sys.boot, transition.boot, trial_options) {
        Ok(()) => {}
        Err(ApplyError::Untouched(cause)) => {
            return Err(cause.context(format!("{name} aborted; the boot selection is unchanged")));
        }
        Err(ApplyError::FirmwareChanged(cause)) => {
            return Err(tagged_err(
                ErrorClass::PartialUpdate,
                format!("{name} changed the boot selection only in part, and recab's state was not updated: {cause:#}"),
            ));
        }
        Err(ApplyError::RestoreFailed { cause, restore }) => {
            return Err(tagged_err(
                ErrorClass::PartialUpdate,
                format!(
                    "{name} failed and loader.conf could not be put back; recab's state was not updated.\n\
primary error: {cause:#}\nrestore error: {restore:#}"
                ),
            ));
        }
    }
    sys.store.save(&next, name).map_err(|err| {
        tagged_err(
            ErrorClass::PartialUpdate,
            format!("{name} changed the boot selection, but recab's state was not saved: {err:#}"),
        )
    })?;
    sys.store.audit(transition.operation, active, &sys.state, &next);
    Ok(())
}
