//! The system recab looks at in one run, decided once: the running system,
//! or a system installed under a root (`init`, `status --root`). Every path,
//! program and fact recab reads about it comes from here, and each path and
//! program the scratch-tree tests redirect has its `RECAB_*` variable; no
//! other module reads the environment, `/`, `/proc` or an os-release.

use anyhow::{Context, Result};
use distro_profile::{os_release_fact, os_release_value, InitSystem};
use std::path::{Path, PathBuf};

use crate::error::{classified, refuse, ErrorClass};
use recpart::policy::{ESP_MOUNT, STATE_MOUNT};

/// recab's directory under the state partition's mount point.
const STATE_DIR: &str = "lib/recab";
const STATE_FILE: &str = "state.json";
const AUDIT_FILE: &str = "audit.log";

/// recab's state file on an installed system, as an absolute path.
pub fn state_file() -> String {
    format!("{STATE_MOUNT}/{STATE_DIR}/{STATE_FILE}")
}

/// recab's audit log on an installed system, as an absolute path.
#[cfg(feature = "contracts")]
pub fn audit_file() -> String {
    format!("{STATE_MOUNT}/{STATE_DIR}/{AUDIT_FILE}")
}

/// Which system the run looks at.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The running system: its root partition is the kernel command line's
    /// `root=`, and its firmware variables are this machine's.
    Running,
    /// A system mounted at `root` (an install target): its root partition is
    /// the one mounted there; which firmware boots it is unknown.
    Installed,
}

/// The firmware's variables, as this machine exposes them.
#[derive(Clone)]
pub(crate) struct Firmware {
    /// `/sys/firmware/efi`, present when the machine booted through UEFI.
    pub efi_dir: PathBuf,
    /// The variables (efivarfs, or a directory of plain files in a scratch
    /// tree: which one is read from the filesystem itself).
    pub efivars: PathBuf,
}

/// The programs recab runs, each named once.
#[derive(Clone)]
pub(crate) struct Programs {
    pub systemctl: String,
    pub rc_status: String,
    /// busybox init's reboot.
    pub reboot: String,
    pub mount: String,
    pub umount: String,
}

#[derive(Clone)]
pub(crate) struct Target {
    pub kind: Kind,
    /// The system's `/`.
    pub root: PathBuf,
    /// Its ESP, where the slot entries, loader.conf and kernels live.
    pub esp: PathBuf,
    /// recab's state directory (state file and audit log).
    pub state_dir: PathBuf,
    /// The run lock: always this machine's, whichever system the run looks at.
    pub lock: PathBuf,
    /// `None` for an installed system, whose firmware recab cannot see.
    pub firmware: Option<Firmware>,
    /// This machine's `/proc`.
    pub proc_dir: PathBuf,
    /// Where `stage` mounts what it reads and writes.
    pub stage_dir: PathBuf,
    /// OpenRC's state directory and its runlevels.
    pub openrc_dir: PathBuf,
    pub runlevels_dir: PathBuf,
    pub programs: Programs,
    /// The os-release, read once.
    os_release: String,
    os_release_path: PathBuf,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn path(name: &str, default: impl AsRef<Path>) -> PathBuf {
    var(name).map_or_else(|| default.as_ref().to_path_buf(), PathBuf::from)
}

/// `absolute` (a path on the system) under `root`.
fn under(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

/// The system's os-release, and the file it came from.
fn os_release(root: &Path, file: Option<PathBuf>) -> Result<(String, PathBuf)> {
    let missing = || refuse(format!("'{}' has no os-release", root.display()));
    match file {
        Some(file) => {
            let text = target_root::read_optional(&file)?.ok_or_else(missing)?;
            Ok((text, file))
        }
        None => {
            let opened = crate::filesystem::Root::open(root)?;
            let text = match opened.optional_text(Path::new("etc/os-release"))? {
                Some(text) => text,
                None => opened
                    .optional_text(Path::new("usr/lib/os-release"))?
                    .ok_or_else(missing)?,
            };
            Ok((text, under(root, "etc/os-release")))
        }
    }
}

impl Target {
    /// The running system, with the `RECAB_*` overrides.
    pub fn running() -> Result<Self> {
        let root = path("RECAB_ROOT", "/");
        let (os_release, os_release_path) =
            os_release(&root, var("RECAB_OS_RELEASE_PATH").map(PathBuf::from))?;
        let efi_dir = path("RECAB_SYS_FIRMWARE_EFI_PATH", "/sys/firmware/efi");
        let program = |name: &str, default: &str| var(name).unwrap_or_else(|| default.to_string());
        Ok(Self {
            kind: Kind::Running,
            esp: path("RECAB_BOOT_ROOT", under(&root, ESP_MOUNT)),
            state_dir: path("RECAB_STATE_DIR", under(&root, STATE_MOUNT).join(STATE_DIR)),
            lock: path("RECAB_LOCK_PATH", "/run/lock/recab.lock"),
            firmware: Some(Firmware {
                efivars: path("RECAB_EFIVARS_PATH", efi_dir.join("efivars")),
                efi_dir,
            }),
            proc_dir: path("RECAB_PROC_PATH", "/proc"),
            stage_dir: path("RECAB_STAGE_DIR", "/run/recab"),
            openrc_dir: path("RECAB_OPENRC_DIR", "/run/openrc"),
            runlevels_dir: path("RECAB_RUNLEVELS_DIR", under(&root, "etc/runlevels")),
            programs: Programs {
                systemctl: program("RECAB_SYSTEMCTL_BIN", "systemctl"),
                rc_status: program("RECAB_RC_STATUS_BIN", "rc-status"),
                reboot: program("RECAB_REBOOT_BIN", "reboot"),
                mount: "mount".to_string(),
                umount: "umount".to_string(),
            },
            root,
            os_release,
            os_release_path,
        })
    }

    /// The system installed under `root`: its ESP and state where the layout
    /// mounts them, its os-release, its root the partition mounted at
    /// `root`. The lock and programs stay this machine's.
    pub fn installed(root: &Path) -> Result<Self> {
        let running = Self::running()?;
        let (os_release, os_release_path) = os_release(root, None)?;
        Ok(Self {
            kind: Kind::Installed,
            root: root.to_path_buf(),
            esp: under(root, ESP_MOUNT),
            state_dir: under(root, STATE_MOUNT).join(STATE_DIR),
            firmware: None,
            os_release,
            os_release_path,
            ..running
        })
    }

    pub fn state_path(&self) -> PathBuf {
        self.state_dir.join(STATE_FILE)
    }

    pub fn audit_path(&self) -> PathBuf {
        self.state_dir.join(AUDIT_FILE)
    }

    /// The kernel command line of the running system.
    pub fn cmdline_path(&self) -> PathBuf {
        self.proc_dir.join("cmdline")
    }

    /// This machine's mount table.
    pub fn mount_table(&self) -> Result<Vec<target_root::Mount>> {
        Ok(target_root::mount_table_at(
            &self.proc_dir.join("self/mountinfo"),
        )?)
    }

    /// Where `stage` mounts `what` (`media`, `image`, `slot-a`).
    pub fn stage_mount(&self, what: &str) -> PathBuf {
        self.stage_dir.join(what)
    }

    /// The entries' name prefix: the os-release `ID`, lower-cased. `init`
    /// names the entries by it and every reader finds them by it.
    pub fn entry_prefix(&self) -> Result<String> {
        os_release_value(&self.os_release, "ID")
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
            .map(|id| id.to_ascii_lowercase())
            .ok_or_else(|| {
                refuse(format!(
                    "'{}' has no ID, which names the boot entries",
                    self.os_release_path.display()
                ))
            })
    }

    /// The os-release `NAME` (entry titles), else its `ID`.
    pub fn os_name(&self) -> Result<String> {
        match os_release_value(&self.os_release, "NAME") {
            Some(name) => Ok(name),
            None => self.entry_prefix(),
        }
    }

    /// The init system of the distro profile (`REC_INIT`).
    pub fn init(&self) -> Result<InitSystem> {
        os_release_fact(&self.os_release, "REC_INIT").map_err(|err| self.no_profile(err))
    }

    fn no_profile(&self, err: impl std::fmt::Display) -> anyhow::Error {
        classified(
            ErrorClass::BackendUnavailable,
            format!("{}: {err}", self.os_release_path.display()),
        )
    }
}

/// The PARTUUID of `device`, which recab names root partitions by.
pub(crate) fn partuuid(device: &Path) -> Result<String> {
    target_root::partuuid(device)
        .with_context(|| format!("reading the PARTUUID of '{}'", device.display()))?
        .ok_or_else(|| {
            refuse(format!(
                "'{}' has no PARTUUID; root partitions must be GPT partitions",
                device.display()
            ))
        })
}
