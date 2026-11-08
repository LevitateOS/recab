//! Everything recab reads and runs, decided once per command: the paths of
//! the boot partition, the firmware variables, the running system and
//! recab's own files, and the programs it calls.
//!
//! [`Config::running`] is the only code that reads the environment. Each path
//! and program can be redirected with its `RECAB_*` variable, which is how
//! recab runs against a scratch tree; no module reads a variable of its own.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::error::{tagged_err, ErrorClass};

#[derive(Clone)]
pub(crate) struct Config {
    /// The ESP, where the slot entries, loader.conf and kernels live.
    pub boot_root: PathBuf,
    /// The slot entries' name prefix, when not inferred.
    pub entry_prefix: Option<String>,
    pub os_release: PathBuf,
    pub proc_cmdline: PathBuf,
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
                return Err(tagged_err(ErrorClass::StateOrDetection, "RECAB_ENTRY_PREFIX is set but empty"))
            }
            prefix => prefix,
        };
        Ok(Self {
            boot_root: path("RECAB_BOOT_ROOT", "/boot"),
            entry_prefix,
            os_release: path("RECAB_OS_RELEASE_PATH", "/etc/os-release"),
            proc_cmdline: path("RECAB_PROC_CMDLINE_PATH", "/proc/cmdline"),
            sys_firmware_efi: path("RECAB_SYS_FIRMWARE_EFI_PATH", "/sys/firmware/efi"),
            efivarfs: var("RECAB_EFIVARS_PATH").is_none(),
            efivars: path("RECAB_EFIVARS_PATH", "/sys/firmware/efi/efivars"),
            state_path: path("RECAB_STATE_PATH", "/var/lib/recab/state.json"),
            audit_path: path("RECAB_AUDIT_PATH", "/var/lib/recab/audit.log"),
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

    /// The system installed under `root` (for `recab init`): its ESP at
    /// `boot/`, its os-release and its state under `var/`. The firmware
    /// variables stay this machine's, which is what boots the target.
    pub fn install_target(self, root: &Path) -> Self {
        Self {
            boot_root: root.join("boot"),
            entry_prefix: None,
            os_release: root.join("etc/os-release"),
            state_path: root.join("var/lib/recab/state.json"),
            audit_path: root.join("var/lib/recab/audit.log"),
            lock_path: root.join("run/lock/recab.lock"),
            ..self
        }
    }
}
