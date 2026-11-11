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

mod candidate;
mod carry;
mod change;
mod compare;
#[cfg(feature = "contracts")]
pub mod contract;
mod efivars;
mod elf;
mod entry;
mod error;
mod filesystem;
mod finalize;
mod generation;
mod immutable;
pub mod merge;
mod merge_accounts;
mod init;
mod installation;
mod init_system;
mod package;
mod prepare;
pub mod release;
mod runtime;
mod slot_mount;
mod stage;
mod state;
mod status;
mod store;
mod sync;
mod system;
mod systemd_boot;
mod target;
mod trial;
mod trial_options;
mod trial_record;
pub mod update;

use anyhow::Result;
use std::path::Path;
use std::time::Duration;

pub use distro_profile::InitSystem;
pub use entry::{entry_name, layout_name, ENTRIES_DIR};
pub use error::{error_text, stable_exit_code, ErrorClass, ErrorPayload};
pub use init::{InitLayout, InitRequest};
pub use init_system::{Health, TRIAL_TIMEOUT_SECS};
pub use package::{trial_service_link, RECAB_BIN};
pub use stage::{StageRequest, StageSource};
pub use state::Startup;
pub use state::{Slot, TrialRecord};
pub use status::{
    status, Action, ActionView, After, Lifecycle, LifecycleState, OtherSystem, SlotView, Status,
    Verdict, STATUS_SCHEMA_VERSION,
};
pub use target::state_file;
pub use trial::{trial, TrialOutcome};

pub use change::{change, changes, edit, ChangeProgress, ChangeRequest};
pub use compare::{
    compare, compare_roots, ComparedSystem, Comparison, Difference, DifferenceKind, FolderSize, Same,
    COMPARISON_SCHEMA_VERSION,
};
pub use finalize::{finalize, BootSeal, Finalization, FinalizeRequest};
pub use generation::{Generation, Inventory};
pub use prepare::{discard, prepare};
use state::Request;
pub use state::{ChangeRecord, Preparation, PreparationPhase, Readiness, SlotState};

static ACTOR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Names who asked for this run (`--actor`), for the record of changes: a
/// front end says "person" or names the agent it started. Kept only when it
/// is a short plain label; it identifies nobody and grants nothing.
pub fn set_actor(actor: Option<String>) {
    let plain = actor.filter(|name| {
        !name.is_empty()
            && name.len() <= 64
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.: ".contains(&b))
    });
    let _ = ACTOR.set(plain);
}

pub(crate) fn actor() -> Option<String> {
    ACTOR.get().cloned().flatten()
}
use store::Store;
pub use sync::SyncReport;
use system::{require_root, System};
use target::Target;

/// Populate and seal equivalent personalized A/B roots, then publish both
/// boot entries and durable installation state. A matching initialization
/// retries its own saved generation; it never resets a live ledger.
/// Mutable installs retain their single-root boot layout.
pub fn init(request: &InitRequest) -> Result<()> {
    require_root("init")?;
    let target = Target::installed(&request.root)?;
    let _lock = Store::new(&target).lock()?;
    init::run(target, request)
}

/// Merge a verified release into the prepared inactive system. Conflicts leave
/// it unbootable; stage never formats or discards a preparation.
pub fn stage(source:&StageSource,change_flavour:bool)->Result<()> {
    stage::merge(source,change_flavour).map_err(|e|error::or_class(e,ErrorClass::StateOrDetection))
}

/// Schedule one trial boot of the inactive `slot`.
pub fn set_next(slot: Slot) -> Result<()> {
    System::locked("set-next")?.run(Request::SetNext(slot))
}

/// Mark the running slot as known-good and make it the default.
pub fn commit() -> Result<()> {
    System::locked("commit")?.run(Request::Commit)
}

/// Configure the next boot back to the known-good slot.
pub fn rollback() -> Result<()> {
    System::locked("rollback")?.run(Request::Rollback)
}

/// How startup of the running system ended, waiting at most `timeout` for
/// it to finish. Only reads; needs no A/B layout or state.
pub fn health(timeout: Duration) -> Result<Health> {
    init_system::health(&Target::running()?, timeout)
}

/// Record the baseline of the image tree `root`: what it ships at the paths
/// its carried list names.
pub fn baseline(root: &Path) -> Result<()> {
    merge::capture_baseline(root)?;
    if carry::Baseline::read(root)?.is_some() { return Ok(()); }
    let list = carry::read_list(root)?;
    carry::Baseline::write(root, &list)
}

/// Write recab's package contents for `init` into the tree `root` (what
/// Recipe packages): the trial service running `bin`, enabled, and the
/// carried-state list.
pub fn install_unit(root: &Path, init: InitSystem, bin: &str) -> Result<()> {
    package::write(root, init, bin)
}

/// Inspect system contents without writing or scheduling a boot.
pub fn inventory(root: &Path) -> Result<Inventory> {
    Inventory::capture(root)
}

/// Preserve classified failures; classify acquisition/selection refusals.
pub fn update_error(error: anyhow::Error) -> anyhow::Error {
    error::or_class(error, ErrorClass::StateOrDetection)
}
