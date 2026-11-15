//! The system recab looks at in one run, decided once: the running system,
//! or a system installed under a root (`init`, `verify-installation`). Every
//! path and fact recab reads about it comes from here.

use anyhow::{Context, Result};
use distro_profile::{os_release_fact, os_release_value, InitSystem};
use std::path::{Path, PathBuf};

use crate::error::{classified, refuse, ErrorClass};
use recpart::policy::{ESP_MOUNT, STATE_MOUNT};

/// recab's directory under the shared `/var` partition.
const STATE_DIR: &str = "lib/recab";

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
    /// The variables (efivarfs).
    pub efivars: PathBuf,
}

#[derive(Clone)]
pub(crate) struct Target {
    pub kind: Kind,
    /// The system's `/`.
    pub root: PathBuf,
    /// Its ESP, where the entries, loader.conf and kernels live.
    pub esp: PathBuf,
    /// recab's directory on the shared `/var` (the installation record).
    pub state_dir: PathBuf,
    /// The run lock: always this machine's, whichever system the run looks at.
    pub lock: PathBuf,
    /// `None` for an installed system, whose firmware recab cannot see.
    pub firmware: Option<Firmware>,
    /// This machine's `/proc`.
    pub proc_dir: PathBuf,
    /// OpenRC's state directory and the system's runlevels.
    pub openrc_dir: PathBuf,
    pub runlevels_dir: PathBuf,
    /// The os-release, read once.
    os_release: String,
    os_release_path: PathBuf,
}

/// `absolute` (a path on the system) under `root`.
fn under(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

/// The system's os-release, and the file it came from.
fn os_release(root: &Path) -> Result<(String, PathBuf)> {
    let opened = crate::filesystem::Root::open(root)?;
    for file in ["etc/os-release", "usr/lib/os-release"] {
        if let Some(text) = opened.optional_text(Path::new(file))? {
            return Ok((text, under(root, file)));
        }
    }
    Err(refuse(format!("'{}' has no os-release", root.display())))
}

impl Target {
    /// The running system.
    pub fn running() -> Result<Self> {
        let root = PathBuf::from("/");
        let (os_release, os_release_path) = os_release(&root)?;
        let efi_dir = PathBuf::from("/sys/firmware/efi");
        Ok(Self {
            kind: Kind::Running,
            esp: under(&root, ESP_MOUNT),
            state_dir: under(&root, STATE_MOUNT).join(STATE_DIR),
            lock: PathBuf::from("/run/lock/recab.lock"),
            firmware: Some(Firmware {
                efivars: efi_dir.join("efivars"),
                efi_dir,
            }),
            proc_dir: PathBuf::from("/proc"),
            openrc_dir: PathBuf::from("/run/openrc"),
            runlevels_dir: under(&root, "etc/runlevels"),
            root,
            os_release,
            os_release_path,
        })
    }

    /// The system installed under `root`: its ESP and `/var` where the
    /// layout mounts them, its os-release, its root the partition mounted at
    /// `root`. The lock stays this machine's.
    pub fn installed(root: &Path) -> Result<Self> {
        let running = Self::running()?;
        let (os_release, os_release_path) = os_release(root)?;
        Ok(Self {
            kind: Kind::Installed,
            root: root.to_path_buf(),
            esp: under(root, ESP_MOUNT),
            state_dir: under(root, STATE_MOUNT).join(STATE_DIR),
            runlevels_dir: under(root, "etc/runlevels"),
            firmware: None,
            os_release,
            os_release_path,
            ..running
        })
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
        os_release_fact(&self.os_release, "REC_INIT").map_err(|err| {
            classified(
                ErrorClass::BackendUnavailable,
                format!("{}: {err}", self.os_release_path.display()),
            )
        })
    }

    /// Wait for, then hold, the exclusive recab lock (released when dropped).
    pub fn lock(&self) -> Result<std::fs::File> {
        use std::os::fd::AsRawFd;
        if let Some(parent) = self.lock.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating '{}'", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock)
            .with_context(|| format!("opening lock '{}'", self.lock.display()))?;
        // SAFETY: flock on a file descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(anyhow::Error::from(std::io::Error::last_os_error())
                .context(format!("locking '{}'", self.lock.display())));
        }
        Ok(file)
    }
}

/// The PARTUUID of `device`, which recab names root partitions by.
pub(crate) fn partuuid(device: &Path) -> Result<String> {
    target_root::partuuid(device)
        .with_context(|| format!("reading the PARTUUID of '{}'", device.display()))?
        .map(|id| id.to_ascii_lowercase())
        .ok_or_else(|| {
            refuse(format!(
                "'{}' has no PARTUUID; root partitions must be GPT partitions",
                device.display()
            ))
        })
}

/// Root, for `command`.
pub(crate) fn require_root(command: &str) -> Result<()> {
    if !target_root::is_root() {
        return Err(classified(
            ErrorClass::Permission,
            format!("{command} must run as root"),
        ));
    }
    Ok(())
}
