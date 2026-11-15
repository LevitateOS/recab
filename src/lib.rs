//! recab manages the two root filesystems of an A/B installation, A and B.
//!
//! - The booted root is locked: mounted read-only, its block device
//!   read-only.
//! - The other root is unlocked: its block device writable, mounted
//!   read-write at [`OTHER_MOUNT`]. Changes are made there.
//! - Which is which follows only from which root booted.
//! - `copy` makes the other root a copy of the booted root.
//! - The boot loader's default is the root that last finished starting;
//!   starting the other root is a one-time pick (`boot-other`), and
//!   `started` makes a root the default once it finished starting.
//!
//! The initramfs (recinit's installed template) does the locking at boot.

mod ab;
mod boot;
mod compare;
#[cfg(feature = "contracts")]
pub mod contract;
mod efivars;
mod entry;
mod error;
mod filesystem;
mod fstab;
mod immutable;
mod init;
mod init_system;
mod installation;
mod inventory;
mod other;
mod package;
mod runtime;
mod sync;
mod target;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

pub use ab::{boot_other, copy, started, status, Copied, Started, Status, STATUS_SCHEMA_VERSION};
pub use compare::{
    compare, compare_roots, ComparedSystem, Comparison, Difference, DifferenceKind, FolderSize,
    Same, COMPARISON_SCHEMA_VERSION,
};
pub use distro_profile::InitSystem;
pub use entry::{entry_name, layout_name, ENTRIES_DIR};
pub use error::{error_text, stable_exit_code, ErrorClass, ErrorPayload};
pub use init::{InitLayout, InitRequest};
pub use init_system::{Health, Startup, STARTUP_TIMEOUT_SECS};
pub use installation::{installation_file, verify as verify_installation, VerifiedInstallation};
pub use other::OTHER_MOUNT;
pub use package::{started_service_link, RECAB_BIN};
pub use sync::SyncReport;

/// Root identifier (A/B). Its one spelling (`A`, `B`) is used by the CLI
/// and the JSON.
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

    /// `A` or `B`, in either case; nothing else.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Slot::ALL
            .into_iter()
            .find(|slot| slot.to_string().eq_ignore_ascii_case(text))
            .ok_or_else(|| format!("'{text}' is not a root (A or B)"))
    }
}

/// One `--machine-events` line on stderr: a command started, finished or
/// failed.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub schema_version: u32,
    /// `started`, `finished` or `failed`.
    pub event: String,
    pub operation: String,
    pub message: String,
    /// The exit status, once the command ended.
    pub code: Option<u8>,
}

/// Set up a freshly installed system: for A/B, root B becomes a copy of
/// root A, both boot entries are written and A is the default; for a
/// mutable install, its one entry.
pub fn init(request: &InitRequest) -> Result<()> {
    target::require_root("init")?;
    let target = target::Target::installed(&request.root)?;
    let _lock = target.lock()?;
    init::run(target, request)
}

/// How startup of the running system ended, waiting at most `timeout` for
/// it to finish. Only reads; needs no A/B layout.
pub fn health(timeout: Duration) -> Result<Health> {
    init_system::health(&target::Target::running()?, timeout)
}

/// Write recab's package contents for `init` into the tree `root` (what
/// Recipe packages): the enabled `recab-started` service and the read-only
/// root's runtime policy.
pub fn install_unit(root: &Path, init: InitSystem, bin: &str) -> Result<()> {
    package::write(root, init, bin)
}
