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

mod carried;
mod efivars;
mod entry;
#[cfg(feature = "contracts")]
pub mod contract;
mod error;
mod init;
mod init_system;
mod package;
mod stage;
mod state;
mod status;
mod store;
mod system;
mod systemd_boot;
mod target;
mod trial;
mod trial_options;

use anyhow::Result;
use std::path::Path;
use std::time::Duration;

pub use target::state_file;
pub use init::{InitLayout, InitRequest};
pub use stage::{StageRequest, StageSource};
pub use distro_profile::InitSystem;
pub use error::{error_text, stable_exit_code, ErrorClass, ErrorPayload};
pub use init_system::{Health, TRIAL_TIMEOUT_SECS};
pub use state::Startup;
pub use package::{trial_service_link, RECAB_BIN};
pub use state::{Slot, TrialRecord};
pub use status::{status, SlotView, Status};
pub use trial::{trial, TrialOutcome};
pub use entry::{entry_name, layout_name, ENTRIES_DIR};

use state::{plan, Request};
use store::Store;
use system::{require_root, System};
use target::Target;

/// Set up the boot layout of a freshly installed system: the target's
/// systemd-boot, the entries (both slots', or the one root's), the installed
/// kernel, loader.conf, this machine's firmware variables and, for A/B,
/// recab's state.
pub fn init(request: &InitRequest) -> Result<()> {
    require_root("init")?;
    let target = Target::installed(&request.root)?;
    let _lock = Store::new(&target).lock()?;
    init::run(target, request)
}

/// Write a prebuilt system into the inactive slot: format it, extract the
/// rootfs image, carry the machine's configuration over, and install the
/// slot's kernel and boot entry. The slot is not booted until `set_next`.
/// An image of another flavour than the running system's is refused unless
/// `change_flavour`.
pub fn stage(source: &StageSource, change_flavour: bool) -> Result<()> {
    let sys = System::locked("stage")?;
    // A medium stays mounted (read-only) until the slot is written.
    let (_media, request) = match source {
        StageSource::Files(request) => (None, request.clone()),
        StageSource::Media(media) => stage::open_media(&sys.target, media)?,
    };
    let transition = plan(Request::Stage, &sys.boot.selection, &sys.state)?.expect("a stage always writes the slot");
    let slot = sys.boot.selection.active.other();
    let init = sys.target.init()?;
    // Stage resets stale firmware variables before it writes the slot.
    sys.backend.can_change()?;
    let inputs = stage::check_inputs(&sys.backend, &sys.boot, slot, &request)?;
    let partition = stage::check_target(&sys.target, &sys.backend, &sys.boot, slot, &inputs)?;
    stage::check_flavour(&sys.target, &inputs, change_flavour)?;
    // Recorded before the slot is touched: from here its old system is gone,
    // so nothing may roll back to it, even if the write fails.
    sys.apply(&transition)?;
    let write = sys.backend.begin_slot_write(&sys.boot, slot)?;
    stage::write_slot(&sys.target, write, slot, &request, &partition, init)
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

/// Write recab's package contents for `init` into the tree `root` (what
/// Recipe packages): the trial service running `bin`, enabled, and the
/// carried-state list.
pub fn install_unit(root: &Path, init: InitSystem, bin: &str) -> Result<()> {
    package::write(root, init, bin)
}
