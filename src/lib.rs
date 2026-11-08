//! recab - A/B slot management for systemd-boot systems.
//!
//! - set up the boot layout at install time, A/B or one root (`init`)
//! - which slot booted, which boots next (`status`)
//! - write a prebuilt system into the inactive slot (`stage`), from files or
//!   from a release medium
//! - boot the other slot once (`set_next`)
//! - keep the running slot (`commit`), or go back (`rollback`)
//! - on each boot, commit a healthy trial or reboot out of a failed one (`trial`)
//!
//! `state` holds the slot state machine without I/O; `systemd_boot` owns the
//! installed ESP (writes and reads entries, loader.conf and the boot
//! selection); `store` keeps recab's state and audit log.

pub mod carried;
mod config;
mod error;
mod init;
mod init_system;
mod package;
mod stage;
mod state;
mod store;
mod systemd_boot;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use config::state_file;
pub use systemd_boot::{
    entry_name, layout_entry_name, ENTRIES_DIR, INITRD_FILE, KERNEL_FILE, LOADER_CONF,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use error::{
    stable_exit_code, EXIT_CODE_BACKEND_UNAVAILABLE, EXIT_CODE_GENERIC, EXIT_CODE_PARTIAL_UPDATE,
    EXIT_CODE_PERMISSION, EXIT_CODE_STATE_OR_DETECTION, EXIT_CODE_TRIAL_FAILED,
};
pub use distro_profile::InitSystem;
pub use init_system::{crash_option, Health, InitSystemName};
pub use package::{trial_service_link, RECAB_BIN, TRIAL_TIMEOUT_SECS};

use error::{tagged_err, ErrorClass};
use config::Config;
use state::{
    committed, judge_trial, known_good, plan, plan_stage, rollback_target, BootChange, Operation,
    PersistentState, Request, Startup, Transition, TrialVerdict, TrialView,
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

/// The prebuilt system `stage` writes: files, or a release medium holding them.
#[derive(Debug, Clone)]
pub enum StageSource {
    Files(StageRequest),
    /// A release medium (its device, or where it is mounted): the rootfs,
    /// kernel and, when it carries one, the installed initramfs at the live
    /// protocol's paths.
    Media(PathBuf),
}

/// The prebuilt system `stage` writes, as files.
#[derive(Debug, Clone)]
pub struct StageRequest {
    /// EROFS rootfs image (a file or a partition), extracted with recstrap.
    pub rootfs: PathBuf,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
}

/// The root partitions `init` writes entries for.
#[derive(Debug, Clone)]
pub enum InitLayout {
    /// Slot A's partition (holding the installed system) and slot B's.
    Ab { slot_a: PathBuf, slot_b: PathBuf },
    /// The one root partition of a mutable install.
    Mutable { root_device: PathBuf },
}

/// The installed system `init` sets up.
#[derive(Debug, Clone)]
pub struct InitRequest {
    /// The installed system's root, with its ESP and its state partition
    /// mounted where the layout puts them (`boot/`, `var/`).
    pub root: PathBuf,
    pub layout: InitLayout,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    /// Extra kernel options for the entries (consoles, for example).
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

/// One recab command's view: its configuration, the boot selection and
/// recab's state, read once, under the run lock when the command may change
/// anything.
struct System {
    config: Config,
    backend: SystemdBoot,
    store: Store,
    boot: Boot,
    state: PersistentState,
    _lock: Option<Lock>,
}

impl System {
    /// Read-only view (`status`) of this system, or of the system installed
    /// under `root`; works without root.
    fn read(root: Option<&Path>) -> Result<Self> {
        let config = Config::running()?;
        let config = match root {
            Some(root) => config.install_target(root),
            None => config,
        };
        Self::open(config, None)
    }

    /// View for `operation`, which changes the system: refused unless root,
    /// and holding the run lock until dropped.
    fn locked(operation: &str) -> Result<Self> {
        Self::open(Config::running()?, Some(operation))
    }

    fn open(config: Config, operation: Option<&str>) -> Result<Self> {
        let backend = SystemdBoot::new(&config);
        let store = Store::new(&config);
        let lock = match operation {
            Some(operation) => {
                require_root(operation)?;
                Some(store.lock()?)
            }
            None => None,
        };
        let boot = backend.read()?;
        let state = store.load()?;
        Ok(Self { config, backend, store, boot, state, _lock: lock })
    }
}

/// Query current A/B state from the boot configuration and recab's state, of
/// this system or of the system installed under `root` (its root partition
/// mounted there, the ESP and state partitions below it). Only reads, so it
/// works anywhere the files are readable.
pub fn status(root: Option<&Path>) -> Result<Status> {
    let sys = System::read(root)?;
    let slots = sys.backend.slot_views(&sys.boot)?;
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
        slots,
    })
}

/// Set up the boot layout of a freshly installed system: the target's
/// systemd-boot, the entries (both slots', or the one root's), the installed
/// kernel, loader.conf, for A/B recab's state, and
/// systemd-boot's firmware variables pointing at the installed entry.
pub fn init(request: &InitRequest) -> Result<()> {
    require_root("init")?;
    init::run(Config::running()?.install_target(&request.root), request)
}

/// Write a prebuilt system into the inactive slot: format it, extract the
/// rootfs image, carry the machine's configuration over, and install the
/// slot's kernel and boot entry. The slot is not booted until `set_next`.
pub fn stage(source: &StageSource) -> Result<()> {
    let sys = System::locked("stage")?;
    // A medium stays mounted (read-only) until the slot is written.
    let (_media, request) = match source {
        StageSource::Files(request) => (None, request.clone()),
        StageSource::Media(media) => {
            let (mounted, request) = stage::open_media(&sys.config, media)?;
            (mounted, request)
        }
    };
    let request = &request;
    let transition = plan_stage(&sys.boot.selection, &sys.state)?;
    let active = sys.boot.selection.active;
    let slot = active.other();
    let profile = init_system::running_profile(&sys.config)?;
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
    stage::write_slot(&sys.config, &sys.backend, &sys.boot, slot, request, &target, &profile)
}

/// Configure a one-shot trial boot of the inactive `slot`.
pub fn set_next(slot: Slot) -> Result<()> {
    change(System::locked("set-next")?, Request::SetNext(slot), |_| {})
}

/// Mark the running slot as known-good and make it the default.
pub fn commit() -> Result<()> {
    change(System::locked("commit")?, Request::Commit, |_| {})
}

/// Configure the next boot back to the known-good slot.
pub fn rollback() -> Result<()> {
    change(System::locked("rollback")?, Request::Rollback, |_| {})
}

/// Decide a trial boot once startup has finished (at most `timeout`). Only
/// the boot of the slot `set_next` scheduled is judged: committed when no
/// unit failed; otherwise not committed, the known-good slot made the next
/// boot, and the machine rebooted into it. The verdict is [`judge_trial`]'s,
/// and everything after it happens under the one lock.
pub fn trial(timeout: Duration) -> Result<TrialOutcome> {
    let config = Config::running()?;
    let init = init_system::running_profile(&config)?.init;
    // Every image runs the trial service: with no recab state and no one-shot
    // boot in the firmware, this is no trial (a live or mutable system).
    let firmware = SystemdBoot::new(&config);
    if !Store::new(&config).exists()? && firmware.firmware_trial()? != Some(true) {
        return Ok(TrialOutcome::NotATrial);
    }
    let health = init_system::wait_finished(&config, init, timeout)?;
    let (startup, failed) = (health.startup, health.failed);
    let problem = match &startup {
        Startup::Running | Startup::Inconclusive(_) => String::new(),
        Startup::Failed(state) => format!("finished starting in state '{state}'"),
        Startup::TimedOut => format!("did not finish starting within {}s", timeout.as_secs()),
    };
    let failed = if failed.is_empty() {
        String::new()
    } else {
        format!(" (failed: {})", failed.join(", "))
    };
    let fail = |message: String| tagged_err(ErrorClass::TrialFailed, message);

    // The ESP or /var may be what broke; the firmware still knows whether
    // this boot was a one-shot trial, and its default is the fallback.
    let sys = System::open(config.clone(), Some("trial"));
    let view = match &sys {
        Ok(sys) => Some(TrialView {
            sel: &sys.boot.selection,
            state: &sys.state,
            state_exists: sys.store.exists()?,
        }),
        Err(_) => None,
    };
    let verdict = judge_trial(&startup, view, || firmware.firmware_trial())?;
    let sys = match (verdict, sys) {
        (TrialVerdict::Ignore, _) => return Ok(TrialOutcome::NotATrial),
        (TrialVerdict::Unreadable, Err(err)) => return Err(err),
        (TrialVerdict::RebootUnread, Err(err)) => {
            require_root("trial")?;
            init_system::reboot(init, &config)?;
            return Err(fail(format!(
                "this one-shot trial boot {problem}{failed}, and the slots could not be read ({err:#}); rebooting into the default entry"
            )));
        }
        (_, Ok(sys)) => sys,
        (verdict, Err(err)) => unreachable!("verdict {verdict:?} needs the slots, which could not be read: {err:#}"),
    };
    let sel = sys.boot.selection;
    let active = sel.active;
    let record = |committed: bool, detail: String| -> Result<TrialRecord> {
        Ok(TrialRecord { slot: active, committed, detail, unix_time: now()? })
    };
    if verdict == TrialVerdict::Commit {
        let record = record(true, "startup finished with no failed units".to_string())?;
        change(sys, Request::Commit, |next| next.last_trial = Some(record))?;
        return Ok(TrialOutcome::Committed(active));
    }

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
        // Planned from the state just written, under the same lock.
        change(System { state: next, ..sys }, Request::Rollback, |_| {})?;
    }
    init_system::reboot(init, &config)?;
    Err(fail(format!(
        "the trial boot of slot {active} {detail}; not committing it. Rebooting into slot {fallback}."
    )))
}

/// How startup of the running system ended, waiting at most `timeout` for
/// it to finish. Only reads; needs no A/B layout or state.
pub fn health(timeout: Duration) -> Result<Health> {
    let config = Config::running()?;
    let init = init_system::running_profile(&config)?.init;
    init_system::wait_finished(&config, init, timeout)
}

/// Write recab's package contents into the tree `root` (what Recipe
/// packages): the trial service for `init` (else the init system of `root`'s
/// os-release), enabled, and the carried-state list.
pub fn install_unit(root: &Path, package: Option<InitSystem>) -> Result<()> {
    let init = match package {
        Some(init) => init,
        None => init_system::installed_profile(root)?.init,
    };
    package::write(root, init)
}

fn require_root(operation: &str) -> Result<()> {
    if !target_root::is_root() {
        return Err(tagged_err(
            ErrorClass::Permission,
            format!("{operation} must run as root"),
        ));
    }
    Ok(())
}

pub(crate) fn now() -> Result<u64> {
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
    sys.backend.ensure_can_change()?;
    // A trial falls back to the default the firmware variable names: make it
    // the known-good default `plan` checked, not a stale value it skipped.
    if let (BootChange::OneShot(slot), Some(default)) = (transition.boot, sys.boot.selection.default) {
        sys.backend.reset_stale_selection(&sys.boot, default, slot)?;
    }

    let mut next = stamped(&transition)?;
    annotate(&mut next);
    let init = init_system::running_profile(&sys.config)?.init;
    match sys.backend.apply(&sys.boot, transition.boot, init) {
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
