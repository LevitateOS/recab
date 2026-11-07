//! What depends on the init system: how `recab trial` learns that startup
//! finished and what failed, how it reboots, the service that runs it on
//! every boot, the kernel options of a trial, and the machine configuration
//! `stage` carries into a new slot.
//!
//! systemd (LevitateOS) and OpenRC (AcornOS) are supported.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::state::Startup;
use crate::store::write_durable;
use crate::{tagged_err, ErrorClass};

/// The init system of a recab-managed system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitSystem {
    Systemd,
    OpenRc,
}

/// What `stage` copies from the running system into the new one, on top of
/// the identity and account files every system carries.
pub(crate) struct Carried {
    /// Files (or symlinks) copied over the image's.
    pub files: &'static [&'static str],
    /// Directories whose contents are copied over the image's, adding what
    /// the machine enabled or configured without removing what the image adds.
    pub dirs: &'static [&'static str],
}

const SYSTEMD_UNIT: &str = include_str!("services/recab-trial.service");
const OPENRC_SERVICE: &str = include_str!("services/recab-trial.openrc");

impl InitSystem {
    /// The init system running now (`RECAB_INIT_SYSTEM` overrides).
    pub(crate) fn running() -> Result<Self> {
        if let Some(init) = Self::from_override()? {
            return Ok(init);
        }
        if Path::new("/run/systemd/system").is_dir() {
            Ok(Self::Systemd)
        } else if Path::new("/run/openrc").is_dir() {
            Ok(Self::OpenRc)
        } else {
            Err(tagged_err(
                ErrorClass::BackendUnavailable,
                "cannot tell the init system: neither /run/systemd/system nor /run/openrc exists (set RECAB_INIT_SYSTEM to systemd or openrc)",
            ))
        }
    }

    /// The init system installed under `root` (`RECAB_INIT_SYSTEM` overrides).
    pub(crate) fn installed_in(root: &Path) -> Result<Self> {
        if let Some(init) = Self::from_override()? {
            return Ok(init);
        }
        let has = |rel: &str| root.join(rel).exists();
        if has("sbin/openrc-run") || has("usr/sbin/openrc-run") || has("usr/bin/openrc-run") {
            Ok(Self::OpenRc)
        } else if has("usr/lib/systemd/systemd") {
            Ok(Self::Systemd)
        } else {
            Err(tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "cannot tell the init system installed in '{}': no openrc-run and no systemd (set RECAB_INIT_SYSTEM to systemd or openrc)",
                    root.display()
                ),
            ))
        }
    }

    fn from_override() -> Result<Option<Self>> {
        match std::env::var("RECAB_INIT_SYSTEM").as_deref() {
            Err(_) => Ok(None),
            Ok("systemd") => Ok(Some(Self::Systemd)),
            Ok("openrc") => Ok(Some(Self::OpenRc)),
            Ok(other) => bail!("RECAB_INIT_SYSTEM is '{other}', not systemd or openrc"),
        }
    }

    /// The link that enables the trial service, relative to the root.
    pub fn trial_service_link(self) -> &'static str {
        match self {
            Self::Systemd => "etc/systemd/system/sysinit.target.wants/recab-trial.service",
            Self::OpenRc => "etc/runlevels/boot/recab-trial",
        }
    }

    /// The trial service's file, relative to the root.
    fn trial_service_path(self) -> &'static str {
        match self {
            Self::Systemd => "usr/lib/systemd/system/recab-trial.service",
            Self::OpenRc => "etc/init.d/recab-trial",
        }
    }

    /// Kernel options a trial's entry carries until it is committed.
    /// systemd: the initramfs reboots instead of opening an emergency shell,
    /// and a crashing PID 1 reboots. OpenRC systems boot without an
    /// initramfs: a root that cannot mount and a dying init both panic, which
    /// `panic=` already turns into a reboot.
    pub(crate) fn trial_options(self) -> &'static [&'static str] {
        match self {
            Self::Systemd => &["rd.emergency=reboot", "systemd.crash_action=reboot"],
            Self::OpenRc => &[],
        }
    }

    /// The machine configuration this init system keeps outside the files
    /// every system carries.
    pub(crate) fn carried(self) -> Carried {
        match self {
            Self::Systemd => Carried {
                files: &[],
                dirs: &[
                    "etc/systemd/system",
                    "etc/NetworkManager/system-connections",
                ],
            },
            Self::OpenRc => Carried {
                files: &["etc/network/interfaces", "etc/inittab", "etc/doas.conf"],
                dirs: &["etc/runlevels", "etc/conf.d", "etc/doas.d"],
            },
        }
    }

    /// The trial service is enabled under `root`.
    pub(crate) fn trial_service_enabled(self, root: &Path) -> bool {
        root.join(self.trial_service_link())
            .symlink_metadata()
            .is_ok()
    }

    /// Write the trial service under `root` and enable it, as
    /// `systemctl enable` or `rc-update add` would.
    pub(crate) fn install_trial_service(self, root: &Path) -> Result<()> {
        let (content, mode) = match self {
            Self::Systemd => (SYSTEMD_UNIT, 0o644),
            Self::OpenRc => (OPENRC_SERVICE, 0o755),
        };
        let path = root.join(self.trial_service_path());
        write_durable(&path, content.as_bytes())
            .with_context(|| format!("writing '{}'", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        self.enable_trial_service(root)
    }

    /// Enable the service packaged by Recipe; never replace package contents.
    pub(crate) fn enable_trial_service(self, root: &Path) -> Result<()> {
        let path = root.join(self.trial_service_path());
        if !path.is_file() {
            bail!(
                "{} is missing; deploy a complete recab package produced by Recipe",
                path.display()
            );
        }
        let link = root.join(self.trial_service_link());
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating '{}'", parent.display()))?;
        }
        if link.symlink_metadata().is_ok() {
            fs::remove_file(&link).with_context(|| format!("replacing '{}'", link.display()))?;
        }
        let target = Path::new("/").join(self.trial_service_path());
        symlink(&target, &link).with_context(|| format!("enabling '{}'", link.display()))
    }

    /// Wait until startup finishes, at most `timeout`, and name what failed.
    pub(crate) fn wait_for_startup(self, timeout: Duration) -> Result<(Startup, Vec<String>)> {
        match self {
            Self::Systemd => systemd_startup(timeout),
            Self::OpenRc => openrc_startup(timeout),
        }
    }

    /// Ask for a clean reboot without waiting for it.
    pub(crate) fn reboot(self) -> Result<()> {
        let (program, args): (String, Vec<&str>) = match (std::env::var("RECAB_REBOOT_BIN"), self) {
            (Ok(program), _) => (program, vec![]),
            (Err(_), Self::Systemd) => (systemctl(), vec!["--no-block", "reboot"]),
            (Err(_), Self::OpenRc) => {
                // openrc-init is shut down with openrc-shutdown; busybox init
                // (with an inittab) with reboot.
                let pid1 = fs::read_to_string("/proc/1/comm").unwrap_or_default();
                if pid1.trim() == "openrc-init" {
                    ("openrc-shutdown".to_string(), vec!["-r", "now"])
                } else {
                    ("reboot".to_string(), vec![])
                }
            }
        };
        let status = Command::new(&program)
            .args(&args)
            .status()
            .with_context(|| format!("running '{program}'"))?;
        if !status.success() {
            bail!("'{program} {}' failed ({status})", args.join(" "));
        }
        Ok(())
    }
}

fn systemctl() -> String {
    std::env::var("RECAB_SYSTEMCTL_BIN").unwrap_or_else(|_| "systemctl".to_string())
}

/// `systemctl is-system-running --wait`: `running`, or failed units
/// (`degraded`), or the emergency/rescue target (`maintenance`).
fn systemd_startup(timeout: Duration) -> Result<(Startup, Vec<String>)> {
    let systemctl = systemctl();
    let mut child = Command::new(&systemctl)
        .args(["is-system-running", "--wait"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running '{systemctl} is-system-running --wait'"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok((Startup::TimedOut, systemd_failed_units(&systemctl)));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let mut state = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut state)?;
    }
    // Anything else (`stopping`, `offline`, `unknown`, ...) is a shutdown or
    // an unreachable systemd: no verdict on the boot.
    let startup = match state.trim() {
        "running" => return Ok((Startup::Running, Vec::new())),
        state @ ("degraded" | "maintenance") => Startup::Failed(state.to_string()),
        "" => Startup::Inconclusive("no answer".to_string()),
        other => Startup::Inconclusive(other.to_string()),
    };
    Ok((startup, systemd_failed_units(&systemctl)))
}

fn systemd_failed_units(systemctl: &str) -> Vec<String> {
    Command::new(systemctl)
        .args([
            "list-units",
            "--failed",
            "--no-legend",
            "--plain",
            "--no-pager",
        ])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// OpenRC's state directory: the runlevel being entered (`softlevel`) and
/// one entry per service in each state (`starting/`, `failed/`).
fn openrc_dir() -> PathBuf {
    PathBuf::from(std::env::var("RECAB_OPENRC_DIR").unwrap_or_else(|_| "/run/openrc".to_string()))
}

/// Startup has finished when the default runlevel is entered, no service is
/// starting, and the `openrc` process that runs the runlevel has exited.
/// It failed when a service failed to start, or when the system went to the
/// single-user runlevel.
fn openrc_startup(timeout: Duration) -> Result<(Startup, Vec<String>)> {
    let dir = openrc_dir();
    let deadline = Instant::now() + timeout;
    loop {
        let level = fs::read_to_string(dir.join("softlevel")).unwrap_or_default();
        match level.trim() {
            "default" if dir_is_empty(&dir.join("starting")) && !openrc_running() => break,
            "single" => {
                return Ok((
                    Startup::Failed("single-user runlevel".to_string()),
                    openrc_failed(&dir),
                ))
            }
            "shutdown" | "reboot" => {
                return Ok((Startup::Inconclusive(level.trim().to_string()), Vec::new()))
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Ok((Startup::TimedOut, openrc_failed(&dir)));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let failed = openrc_failed(&dir);
    if failed.is_empty() {
        Ok((Startup::Running, failed))
    } else {
        Ok((Startup::Failed("services failed".to_string()), failed))
    }
}

fn dir_is_empty(dir: &Path) -> bool {
    fs::read_dir(dir).map_or(true, |mut entries| entries.next().is_none())
}

/// The service names in `dir` (a runlevel or an OpenRC state directory).
fn service_names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The runlevels a boot starts, under `/etc/runlevels` (or `RECAB_RUNLEVELS_DIR`).
const BOOT_RUNLEVELS: [&str; 3] = ["sysinit", "boot", "default"];

fn runlevels_dir() -> PathBuf {
    PathBuf::from(std::env::var("RECAB_RUNLEVELS_DIR").unwrap_or_else(|_| "/etc/runlevels".to_string()))
}

/// Services that failed to start: each service of the boot runlevels that is neither
/// started nor inactive, those OpenRC still marks failed, and daemons `rc-status`
/// reports crashed. OpenRC empties `failed/` when the runlevel's `openrc` exits, which
/// is when startup counts as finished, so the runlevels are what tells.
fn openrc_failed(dir: &Path) -> Vec<String> {
    let up: Vec<String> = ["started", "inactive"]
        .iter()
        .flat_map(|state| service_names(&dir.join(state)))
        .collect();
    let mut failed: Vec<String> = service_names(&dir.join("failed"));
    let levels = runlevels_dir();
    for level in BOOT_RUNLEVELS {
        for name in service_names(&levels.join(level)) {
            if !up.contains(&name) && !failed.contains(&name) {
                failed.push(name);
            }
        }
    }
    let rc_status =
        std::env::var("RECAB_RC_STATUS_BIN").unwrap_or_else(|_| "rc-status".to_string());
    if let Ok(out) = Command::new(rc_status)
        .arg("--crashed")
        .stderr(Stdio::null())
        .output()
    {
        for name in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if !failed.iter().any(|f| f == name) {
                failed.push(format!("{name} (crashed)"));
            }
        }
    }
    failed.sort();
    failed
}

/// An `openrc` process (running a runlevel) exists.
fn openrc_running() -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
            && fs::read_to_string(entry.path().join("comm"))
                .is_ok_and(|comm| comm.trim() == "openrc")
    })
}
