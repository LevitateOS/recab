//! What depends on the system's distro profile (the `REC_*` fields of its
//! os-release): how `recab trial` learns that startup finished and what
//! failed, how it reboots, the service that runs it on every boot, the kernel
//! options of a trial, and the machine configuration `stage` carries into a
//! new slot.
//!
//! systemd and OpenRC are supported.

use anyhow::{bail, Context, Result};
use distro_profile::{InitSystem, PrivilegeTool, Profile};
use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::state::Startup;
use crate::{tagged_err, ErrorClass};

/// What `stage` copies from the running system into the new one, on top of
/// the identity and account files every system carries.
pub(crate) struct Carried {
    /// Files (or symlinks) copied over the image's.
    pub files: Vec<&'static str>,
    /// Directories whose contents are copied over the image's, adding what
    /// the machine enabled or configured without removing what the image adds.
    pub dirs: Vec<&'static str>,
}

const SYSTEMD_UNIT: &str = include_str!("services/recab-trial.service");
const OPENRC_SERVICE: &str = include_str!("services/recab-trial.openrc");

/// The profile of the running system, from its os-release.
pub(crate) fn running_profile(config: &Config) -> Result<Profile> {
    let path = &config.os_release;
    let text = fs::read_to_string(path).map_err(|error| {
        tagged_err(
            ErrorClass::BackendUnavailable,
            format!("cannot read '{}': {error}", path.display()),
        )
    })?;
    Profile::from_os_release(&text).map_err(|error| {
        tagged_err(
            ErrorClass::BackendUnavailable,
            format!("{}: {error}", path.display()),
        )
    })
}

/// The profile of the system installed under `root`, from its os-release.
pub(crate) fn installed_profile(root: &Path) -> Result<Profile> {
    let text = target_root::os_release(root)?.ok_or_else(|| {
        tagged_err(
            ErrorClass::StateOrDetection,
            format!("'{}' has no os-release", root.display()),
        )
    })?;
    Profile::from_os_release(&text).map_err(|error| {
        tagged_err(
            ErrorClass::StateOrDetection,
            format!("{}'s os-release: {error}", root.display()),
        )
    })
}

/// The machine configuration the system keeps outside the files every system
/// carries: its init system's and its privilege tool's.
pub(crate) fn carried(profile: &Profile) -> Carried {
    let mut carried = match profile.init {
        InitSystem::Systemd => Carried {
            files: vec![],
            dirs: vec![
                "etc/systemd/system",
                "etc/NetworkManager/system-connections",
            ],
        },
        InitSystem::Openrc => Carried {
            files: vec!["etc/network/interfaces", "etc/inittab"],
            dirs: vec!["etc/runlevels", "etc/conf.d"],
        },
    };
    match profile.privilege {
        PrivilegeTool::Sudo => {}
        PrivilegeTool::Doas => {
            carried.files.push("etc/doas.conf");
            carried.dirs.push("etc/doas.d");
        }
    }
    carried
}

/// The trial service's file, relative to the root.
fn trial_service_path(init: InitSystem) -> &'static str {
    match init {
        InitSystem::Systemd => "usr/lib/systemd/system/recab-trial.service",
        InitSystem::Openrc => "etc/init.d/recab-trial",
    }
}

/// What recab does differently per init system.
pub trait InitSystemExt: Copy {
    /// The link that enables the trial service, relative to the root.
    fn trial_service_link(self) -> &'static str;

    /// Kernel options a trial's entry carries until it is committed.
    fn trial_options(self) -> &'static [&'static str];

    /// The trial service is enabled under `root`.
    fn trial_service_enabled(self, root: &Path) -> Result<bool>;

    /// Write the trial service's file under `root` (a system, or the package
    /// tree Recipe packages recab from).
    fn write_trial_service(self, root: &Path) -> Result<()>;

    /// Enable the packaged service, as `systemctl enable` or `rc-update add`
    /// would; never replace package contents.
    fn enable_trial_service(self, root: &Path) -> Result<()>;

}

impl InitSystemExt for InitSystem {
    fn trial_service_link(self) -> &'static str {
        match self {
            InitSystem::Systemd => "etc/systemd/system/sysinit.target.wants/recab-trial.service",
            InitSystem::Openrc => "etc/runlevels/boot/recab-trial",
        }
    }

    /// systemd: the initramfs reboots instead of opening an emergency shell,
    /// and a crashing PID 1 reboots. OpenRC systems boot without an
    /// initramfs: a root that cannot mount and a dying init both panic, which
    /// `panic=` already turns into a reboot.
    fn trial_options(self) -> &'static [&'static str] {
        match self {
            InitSystem::Systemd => &["rd.emergency=reboot", "systemd.crash_action=reboot"],
            InitSystem::Openrc => &[],
        }
    }

    fn trial_service_enabled(self, root: &Path) -> Result<bool> {
        Ok(target_root::exists(&root.join(self.trial_service_link()))?)
    }

    fn write_trial_service(self, root: &Path) -> Result<()> {
        let (content, mode) = match self {
            InitSystem::Systemd => (SYSTEMD_UNIT, 0o644),
            InitSystem::Openrc => (OPENRC_SERVICE, 0o755),
        };
        let path = root.join(trial_service_path(self));
        target_root::write_durable(&path, content.as_bytes(), Some(mode))
            .with_context(|| format!("writing '{}'", path.display()))
    }

    fn enable_trial_service(self, root: &Path) -> Result<()> {
        let path = root.join(trial_service_path(self));
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
        if target_root::exists(&link)? {
            fs::remove_file(&link).with_context(|| format!("replacing '{}'", link.display()))?;
        }
        let target = Path::new("/").join(trial_service_path(self));
        symlink(&target, &link).with_context(|| format!("enabling '{}'", link.display()))
    }
}

/// Ask for a clean reboot without waiting for it.
pub(crate) fn reboot(init: InitSystem, config: &Config) -> Result<()> {
    let (program, args): (String, Vec<&str>) = match (&config.reboot, init) {
        (Some(program), _) => (program.clone(), vec![]),
        (None, InitSystem::Systemd) => (config.systemctl.clone(), vec!["--no-block", "reboot"]),
        (None, InitSystem::Openrc) => {
            // openrc-init is shut down with openrc-shutdown; busybox init
            // (with an inittab) with reboot.
            let pid1 = fs::read_to_string("/proc/1/comm").context("reading /proc/1/comm")?;
            if pid1.trim() == "openrc-init" {
                ("openrc-shutdown".to_string(), vec!["-r", "now"])
            } else {
                ("reboot".to_string(), vec![])
            }
        }
    };
    target_root::run(Command::new(&program).args(&args))?;
    Ok(())
}

/// Wait until startup finishes, at most `timeout`, and name what failed.
pub(crate) fn wait_for_startup(
    config: &Config,
    init: InitSystem,
    timeout: Duration,
) -> Result<(Startup, Vec<String>)> {
    match init {
        InitSystem::Systemd => systemd_startup(&config.systemctl, timeout),
        InitSystem::Openrc => openrc_startup(config, timeout),
    }
}

/// `systemctl is-system-running --wait`: `running`, or failed units
/// (`degraded`), or the emergency/rescue target (`maintenance`).
fn systemd_startup(systemctl: &str, timeout: Duration) -> Result<(Startup, Vec<String>)> {
    let mut child = Command::new(systemctl)
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
            child.kill().context("stopping 'systemctl is-system-running --wait'")?;
            child.wait().context("waiting for 'systemctl is-system-running --wait'")?;
            return Ok((Startup::TimedOut, systemd_failed_units(systemctl)?));
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
    Ok((startup, systemd_failed_units(systemctl)?))
}

fn systemd_failed_units(systemctl: &str) -> Result<Vec<String>> {
    let out = target_root::run(Command::new(systemctl).args([
        "list-units",
        "--failed",
        "--no-legend",
        "--plain",
        "--no-pager",
    ]))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect())
}

/// Startup has finished when the default runlevel is entered, no service is
/// starting, and the `openrc` process that runs the runlevel has exited.
/// It failed when a service failed to start, or when the system went to the
/// single-user runlevel. OpenRC's state directory holds the runlevel being
/// entered (`softlevel`) and one entry per service in each state
/// (`starting/`, `failed/`).
fn openrc_startup(config: &Config, timeout: Duration) -> Result<(Startup, Vec<String>)> {
    let dir = &config.openrc_dir;
    let deadline = Instant::now() + timeout;
    loop {
        let level = target_root::read_optional(&dir.join("softlevel"))?.unwrap_or_default();
        match level.trim() {
            "default" if target_root::dir_names(&dir.join("starting"))?.is_empty() && !openrc_running()? => break,
            "single" => {
                return Ok((
                    Startup::Failed("single-user runlevel".to_string()),
                    openrc_failed(config)?,
                ))
            }
            "shutdown" | "reboot" => {
                return Ok((Startup::Inconclusive(level.trim().to_string()), Vec::new()))
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Ok((Startup::TimedOut, openrc_failed(config)?));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let failed = openrc_failed(config)?;
    if failed.is_empty() {
        Ok((Startup::Running, failed))
    } else {
        Ok((Startup::Failed("services failed".to_string()), failed))
    }
}

/// The service names in `dir` (a runlevel or an OpenRC state directory).
fn service_names(dir: &Path) -> Result<Vec<String>> {
    Ok(target_root::dir_names(dir)?
        .into_iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect())
}

/// The runlevels a boot starts.
const BOOT_RUNLEVELS: [&str; 3] = ["sysinit", "boot", "default"];

/// Services that failed to start: each service of the boot runlevels that is neither
/// started nor inactive, those OpenRC still marks failed, and daemons `rc-status`
/// reports crashed. OpenRC empties `failed/` when the runlevel's `openrc` exits, which
/// is when startup counts as finished, so the runlevels are what tells.
fn openrc_failed(config: &Config) -> Result<Vec<String>> {
    let dir = &config.openrc_dir;
    let mut up = service_names(&dir.join("started"))?;
    up.extend(service_names(&dir.join("inactive"))?);
    let mut failed: Vec<String> = service_names(&dir.join("failed"))?;
    for level in BOOT_RUNLEVELS {
        for name in service_names(&config.runlevels_dir.join(level))? {
            if !up.contains(&name) && !failed.contains(&name) {
                failed.push(name);
            }
        }
    }
    // rc-status exits non-zero when it lists a crashed daemon: its output is the answer.
    let out = Command::new(&config.rc_status)
        .arg("--crashed")
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("running '{} --crashed'", config.rc_status))?;
    for name in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if !failed.iter().any(|f| f == name) {
            failed.push(format!("{name} (crashed)"));
        }
    }
    failed.sort();
    Ok(failed)
}

/// An `openrc` process (running a runlevel) exists.
fn openrc_running() -> Result<bool> {
    for name in target_root::dir_names(Path::new("/proc"))? {
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // A process that exited since the listing has no comm any more.
        let comm = target_root::read_optional(&Path::new("/proc").join(&name).join("comm"));
        match comm {
            Ok(Some(comm)) if comm.trim() == "openrc" => return Ok(true),
            Ok(_) => {}
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(false)
}
