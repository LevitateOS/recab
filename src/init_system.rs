//! recab's init adapter: what a trial needs from the running init system,
//! the one from the system's distro profile (the `REC_*` fields of its
//! os-release). Three operations: wait until startup finished and name what
//! failed ([`wait_finished`]), reboot ([`reboot`]), and the kernel option
//! that reboots on a crashing PID 1 ([`crash_option`]).
//!
//! systemd and OpenRC are supported.

use anyhow::{Context, Result};
use distro_profile::{InitSystem, Profile};
use serde::Serialize;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::state::Startup;
use crate::error::{classified, refuse, ErrorClass};

/// The profile of the running system, from its os-release.
pub(crate) fn running_profile(config: &Config) -> Result<Profile> {
    let path = &config.os_release;
    let text = fs::read_to_string(path).map_err(|error| {
        classified(
            ErrorClass::BackendUnavailable,
            format!("cannot read '{}': {error}", path.display()),
        )
    })?;
    Profile::from_os_release(&text).map_err(|error| {
        classified(
            ErrorClass::BackendUnavailable,
            format!("{}: {error}", path.display()),
        )
    })
}

/// The profile of the system installed under `root`, from its os-release.
pub(crate) fn installed_profile(root: &Path) -> Result<Profile> {
    let text = target_root::os_release(root)?.ok_or_else(|| {
        refuse(format!("'{}' has no os-release", root.display()),
        )
    })?;
    Profile::from_os_release(&text).map_err(|error| {
        refuse(format!("{}'s os-release: {error}", root.display()),
        )
    })
}

/// The kernel option that turns a crashing PID 1 into a reboot, for an init
/// that has one; OpenRC's init dying panics the kernel, which `panic=` already
/// reboots.
pub fn crash_option(init: InitSystem) -> Option<&'static str> {
    match init {
        InitSystem::Systemd => Some("systemd.crash_action=reboot"),
        InitSystem::Openrc => None,
    }
}

/// How startup of the running system ended (`recab health --json`).
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub init: InitSystem,
    /// Startup ran to its end (systemd `running` or `degraded`; OpenRC's
    /// default runlevel entered with nothing starting).
    pub finished: bool,
    /// The init's own word: systemd's `is-system-running` answer, OpenRC's
    /// softlevel, or `timed out`.
    pub state: String,
    /// The units or services that failed (and, on OpenRC, crashed daemons),
    /// sorted, by bare name; listed even when startup did not finish.
    pub failed: Vec<String>,
    /// What a trial makes of it.
    #[serde(skip)]
    #[cfg_attr(feature = "contracts", schemars(skip))]
    #[cfg_attr(feature = "contracts", ts(skip))]
    pub(crate) startup: Startup,
}

/// The word `state` takes when the deadline passed.
const TIMED_OUT: &str = "timed out";
/// How often an init's progress is looked at while waiting.
const POLL: Duration = Duration::from_millis(250);

/// Wait until startup finished, at most `timeout`, and name what failed.
pub(crate) fn wait_finished(config: &Config, init: InitSystem, timeout: Duration) -> Result<Health> {
    let (finished, state, mut failed, startup) = match init {
        InitSystem::Systemd => systemd_startup(&config.systemctl, timeout)?,
        InitSystem::Openrc => openrc_startup(config, timeout)?,
    };
    failed.sort();
    failed.dedup();
    Ok(Health { init, finished, state, failed, startup })
}

type Waited = (bool, String, Vec<String>, Startup);

/// `systemctl is-system-running --wait`: `running`, or failed units
/// (`degraded`), or the emergency/rescue target (`maintenance`).
fn systemd_startup(systemctl: &str, timeout: Duration) -> Result<Waited> {
    let mut child = Command::new(systemctl)
        .args(["is-system-running", "--wait"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running '{systemctl} is-system-running --wait'"))?;
    let deadline = Instant::now() + timeout;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill().context("stopping 'systemctl is-system-running --wait'")?;
            child.wait().context("waiting for 'systemctl is-system-running --wait'")?;
            return Ok((false, TIMED_OUT.to_string(), systemd_failed_units(systemctl)?, Startup::TimedOut));
        }
        std::thread::sleep(POLL);
    }
    let mut state = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut state)?;
    }
    let state = state.trim().to_string();
    let failed = systemd_failed_units(systemctl)?;
    // Anything else (`stopping`, `offline`, `unknown`, ...) is a shutdown or
    // an unreachable systemd: no verdict on the boot.
    let (finished, startup) = match state.as_str() {
        "running" => (true, Startup::Running),
        "degraded" => (true, Startup::Failed(state.clone())),
        "maintenance" => (false, Startup::Failed(state.clone())),
        "" => (false, Startup::Inconclusive("no answer".to_string())),
        other => (false, Startup::Inconclusive(other.to_string())),
    };
    Ok((finished, state, failed, startup))
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
fn openrc_startup(config: &Config, timeout: Duration) -> Result<Waited> {
    let dir = &config.openrc_dir;
    let deadline = Instant::now() + timeout;
    loop {
        let level = target_root::read_optional(&dir.join("softlevel"))?.unwrap_or_default();
        let level = level.trim().to_string();
        match level.as_str() {
            "default" if target_root::dir_names(&dir.join("starting"))?.is_empty() && !openrc_running()? => {
                let failed = openrc_failed(config)?;
                let startup = if failed.is_empty() {
                    Startup::Running
                } else {
                    Startup::Failed("services failed".to_string())
                };
                return Ok((true, level, failed, startup));
            }
            "single" => {
                return Ok((false, level, openrc_failed(config)?, Startup::Failed("single-user runlevel".to_string())))
            }
            "shutdown" | "reboot" => {
                return Ok((false, level.clone(), openrc_failed(config)?, Startup::Inconclusive(level)))
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Ok((false, TIMED_OUT.to_string(), openrc_failed(config)?, Startup::TimedOut));
        }
        std::thread::sleep(POLL);
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
    // rc-status exits non-zero when it lists a crashed daemon: its output is
    // the answer; a missing rc-status is an error.
    let out = Command::new(&config.rc_status)
        .arg("--crashed")
        .output()
        .with_context(|| format!("running '{} --crashed'", config.rc_status))?;
    let crashed = String::from_utf8_lossy(&out.stdout);
    // It exits non-zero when it lists a crashed daemon; non-zero with nothing
    // listed is rc-status itself failing.
    if !out.status.success() && crashed.trim().is_empty() {
        anyhow::bail!(
            "'{} --crashed' failed ({}): {}",
            config.rc_status,
            target_root::status_text(out.status),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    failed.extend(crashed.split_whitespace().map(str::to_string));
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
