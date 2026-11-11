//! recab owns symmetric immutable A/B continuation and system updates.
//!
//! Installation publishes two equivalent personalized systems. Preparation
//! synchronizes the inactive filesystem incrementally; Recipe changes and
//! verified releases share that preparation. Finalization seals the candidate,
//! then a generation-bound trial commits it or returns to the protected system.
//! Boot selection, startup health and recovery remain part of this lifecycle.

mod candidate;
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
mod fstab;
mod generation;
pub mod history;
mod immutable;
mod init;
mod init_system;
mod installation;
pub mod merge;
mod merge_accounts;
pub mod operations;
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
pub use entry::{ENTRIES_DIR, entry_name, layout_name};
pub use error::{ErrorClass, ErrorPayload, error_text, stable_exit_code};
pub use init::{InitLayout, InitRequest};
pub use init_system::{Health, TRIAL_TIMEOUT_SECS};
pub use package::{RECAB_BIN, trial_service_link};
pub use stage::{StageRequest, StageSource};
pub use state::Startup;
pub use state::{Slot, TrialRecord};
pub use status::{
    Action, ActionView, After, Lifecycle, LifecycleState, OtherSystem, STATUS_SCHEMA_VERSION,
    SlotView, Status, Verdict, status,
};
pub use target::state_file;
pub use trial::{TrialOutcome, trial};

pub use change::{ChangeProgress, ChangeRequest, change, changes, edit};
pub use compare::{
    COMPARISON_SCHEMA_VERSION, ComparedSystem, Comparison, Difference, DifferenceKind, FolderSize,
    Same, compare, compare_roots,
};
pub use finalize::{BootSeal, Finalization, FinalizeRequest, finalize};
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
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.: ".contains(&b))
    });
    let _ = ACTOR.set(plain);
}

pub(crate) fn actor() -> Option<String> {
    ACTOR.get().cloned().flatten()
}
use store::Store;
pub use sync::SyncReport;
use system::{System, require_root};
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
pub fn stage(source: &StageSource, change_flavour: bool) -> Result<()> {
    stage::merge(source, change_flavour)
        .map_err(|e| error::or_class(e, ErrorClass::StateOrDetection))
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

/// Record the complete release inventory and account baseline before the
/// extracted image is personalized. Existing baselines survive setup retries.
pub fn baseline(root: &Path) -> Result<()> {
    merge::capture_baseline(root)
}

/// Write recab's package contents for `init` into the tree `root` (what
/// Recipe packages): the enabled trial service, immutable runtime policy and
/// compatibility record bound to the executable.
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

/// Select a generation using the preparation snapshot shown by a frontend.
pub fn checked_selection(
    action: &str,
    slot: Option<Slot>,
    preparation: Option<&str>,
    revision: Option<u64>,
    generation: Option<&str>,
) -> anyhow::Result<()> {
    let sys = system::System::locked(action)?;
    if preparation.is_some() || revision.is_some() {
        match sys.state.preparation.as_ref() {
            Some(p) if Some(p.id.as_str()) == preparation && Some(p.revision) == revision => {}
            None if preparation == Some("none") && revision == Some(0) => {}
            _ => return Err(error::refuse("stale preparation id/revision")),
        }
    }
    let request = match action {
        "set-next" => state::Request::SetNext(slot.ok_or_else(|| error::refuse("slot required"))?),
        "commit" => state::Request::Commit,
        "rollback" => state::Request::Rollback,
        _ => return Err(error::refuse("unknown selection operation")),
    };
    let selected = match action {
        "set-next" => slot,
        "commit" => Some(sys.boot.selection.active),
        _ => state::rollback_target(&sys.state, &sys.boot.selection),
    };
    if let Some(expected) = generation {
        if !selected
            .and_then(|s| sys.state.require_ready(s).ok())
            .is_some_and(|g| g.id == expected)
        {
            return Err(error::refuse(
                "stale generation: refresh status before selecting a system",
            ));
        }
    }
    sys.run(request)
        .map_err(|e| error::or_class(e, error::ErrorClass::StateOrDetection))
}

pub use installation::{VerifiedInstallation, verify as verify_installation};
