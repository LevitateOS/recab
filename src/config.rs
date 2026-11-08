//! Everything recab reads and runs, decided once per command: the paths of
//! the boot partition, the firmware variables, the running system and
//! recab's own files, and the programs it calls.
//!
//! [`Config::running`] is the only code that reads the environment. Each path
//! and program can be redirected with its `RECAB_*` variable, which is how
//! recab runs against a scratch tree; no module reads a variable of its own.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::error::refuse;
use recpart::policy::{ESP_MOUNT, STATE_MOUNT};

/// recab's directory under the state partition's mount point.
const STATE_DIR: &str = "lib/recab";

/// recab's state file on a system, as an absolute path.
pub fn state_file() -> String {
    format!("{STATE_MOUNT}/{STATE_DIR}/state.json")
}

/// How recab tells which partition holds the root of the system it reads.
#[derive(Clone)]
pub(crate) enum RunningRoot {
    /// The running system: the `root=` of the kernel command line in this file.
    Cmdline(PathBuf),
    /// A system mounted at this path (an install target): the partition
    /// mounted there.
    Mounted(PathBuf),
}

#[derive(Clone)]
pub(crate) struct Config {
    /// The ESP, where the slot entries, loader.conf and kernels live.
    pub boot_root: PathBuf,
    /// The slot entries' name prefix, when not the os-release ID.
    pub entry_prefix: Option<String>,
    pub os_release: PathBuf,
    pub running_root: RunningRoot,
    pub sys_firmware_efi: PathBuf,
    pub efivars: PathBuf,
    /// `efivars` is efivarfs, the firmware's variables. Off for a scratch
    /// tree (`RECAB_EFIVARS_PATH`), whose variables are plain files.
    pub efivarfs: bool,
    pub state_path: PathBuf,
    pub audit_path: PathBuf,
    pub lock_path: PathBuf,
    /// Where `stage` mounts the slot it writes (`<dir>/stage-<a|b>`).
    pub stage_dir: PathBuf,
    /// OpenRC's state directory and its runlevels.
    pub openrc_dir: PathBuf,
    pub runlevels_dir: PathBuf,
    pub systemctl: String,
    pub rc_status: String,
    /// The program that reboots, when not the init system's own way.
    pub reboot: Option<String>,
    /// The extractor `stage` runs.
    pub recstrap: String,
}

impl Config {
    /// The running system's configuration, with the `RECAB_*` overrides.
    pub fn running() -> Result<Self> {
        let var = |name: &str| std::env::var(name).ok();
        let path = |name: &str, default: &str| PathBuf::from(var(name).unwrap_or_else(|| default.to_string()));
        let program = |name: &str, default: &str| var(name).unwrap_or_else(|| default.to_string());
        let entry_prefix = match var("RECAB_ENTRY_PREFIX") {
            Some(prefix) if prefix.trim().is_empty() => {
                return Err(refuse("RECAB_ENTRY_PREFIX is set but empty"))
            }
            prefix => prefix,
        };
        let (state, audit) = state_files(Path::new("/"));
        Ok(Self {
            boot_root: path("RECAB_BOOT_ROOT", ESP_MOUNT),
            entry_prefix,
            os_release: path("RECAB_OS_RELEASE_PATH", "/etc/os-release"),
            running_root: RunningRoot::Cmdline(path("RECAB_PROC_CMDLINE_PATH", "/proc/cmdline")),
            sys_firmware_efi: path("RECAB_SYS_FIRMWARE_EFI_PATH", "/sys/firmware/efi"),
            efivarfs: var("RECAB_EFIVARS_PATH").is_none(),
            efivars: path("RECAB_EFIVARS_PATH", "/sys/firmware/efi/efivars"),
            state_path: var("RECAB_STATE_PATH").map_or(state, PathBuf::from),
            audit_path: var("RECAB_AUDIT_PATH").map_or(audit, PathBuf::from),
            lock_path: path("RECAB_LOCK_PATH", "/run/lock/recab.lock"),
            stage_dir: PathBuf::from("/run/recab"),
            openrc_dir: path("RECAB_OPENRC_DIR", "/run/openrc"),
            runlevels_dir: path("RECAB_RUNLEVELS_DIR", "/etc/runlevels"),
            systemctl: program("RECAB_SYSTEMCTL_BIN", "systemctl"),
            rc_status: program("RECAB_RC_STATUS_BIN", "rc-status"),
            reboot: var("RECAB_REBOOT_BIN"),
            recstrap: program("RECAB_RECSTRAP_BIN", "recstrap"),
        })
    }

    /// The system installed under `root` (for `recab init` and `status
    /// --root`): its ESP and its state where the layout mounts them, its
    /// os-release, and its root the partition mounted at `root`. The
    /// firmware variables stay this machine's, which is what boots the target.
    pub fn install_target(self, root: &Path) -> Self {
        let (state_path, audit_path) = state_files(root);
        Self {
            boot_root: under(root, ESP_MOUNT),
            entry_prefix: None,
            os_release: root.join("etc/os-release"),
            running_root: RunningRoot::Mounted(root.to_path_buf()),
            state_path,
            audit_path,
            lock_path: root.join("run/lock/recab.lock"),
            ..self
        }
    }
}

/// `absolute` (a path on the installed system) under `root`.
fn under(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

/// The state file and audit log of the system under `root`.
fn state_files(root: &Path) -> (PathBuf, PathBuf) {
    let dir = under(root, STATE_MOUNT).join(STATE_DIR);
    (dir.join("state.json"), dir.join("audit.log"))
}
