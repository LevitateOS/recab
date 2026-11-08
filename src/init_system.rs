//! recab's init adapter: what a trial needs from the running init system,
//! the one from the system's distro profile (the `REC_*` fields of its
//! os-release). Three operations: wait until startup finished and name what
//! failed ([`wait_finished`]), reboot ([`reboot`]), and the kernel option
//! that reboots on a crashing PID 1 ([`crash_option`]).
//!
//! systemd and OpenRC are supported.

use anyhow::{Context, Result};
use distro_profile::InitSystem;
use serde::Serialize;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::target::Target;
use crate::state::Startup;

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
pub(crate) fn wait_finished(target: &Target, init: InitSystem, timeout: Duration) -> Result<Health> {
    let (finished, state, mut failed, startup) = match init {
        InitSystem::Systemd => systemd_startup(&target.programs.systemctl, timeout)?,
        InitSystem::Openrc => openrc_startup(target, timeout)?,
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
fn openrc_startup(target: &Target, timeout: Duration) -> Result<Waited> {
    let dir = &target.openrc_dir;
    let deadline = Instant::now() + timeout;
    loop {
        let level = target_root::read_optional(&dir.join("softlevel"))?.unwrap_or_default();
        let level = level.trim().to_string();
        match level.as_str() {
            "default" if target_root::dir_names(&dir.join("starting"))?.is_empty() && !openrc_running(target)? => {
                let failed = openrc_failed(target)?;
                let startup = if failed.is_empty() {
                    Startup::Running
                } else {
                    Startup::Failed("services failed".to_string())
                };
                return Ok((true, level, failed, startup));
            }
            "single" => {
                return Ok((false, level, openrc_failed(target)?, Startup::Failed("single-user runlevel".to_string())))
            }
            "shutdown" | "reboot" => {
                return Ok((false, level.clone(), openrc_failed(target)?, Startup::Inconclusive(level)))
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Ok((false, TIMED_OUT.to_string(), openrc_failed(target)?, Startup::TimedOut));
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
fn openrc_failed(target: &Target) -> Result<Vec<String>> {
    let dir = &target.openrc_dir;
    let mut up = service_names(&dir.join("started"))?;
    up.extend(service_names(&dir.join("inactive"))?);
    let mut failed: Vec<String> = service_names(&dir.join("failed"))?;
    for level in BOOT_RUNLEVELS {
        for name in service_names(&target.runlevels_dir.join(level))? {
            if !up.contains(&name) && !failed.contains(&name) {
                failed.push(name);
            }
        }
    }
    // rc-status exits non-zero when it lists a crashed daemon: its output is
    // the answer; a missing rc-status is an error.
    let out = Command::new(&target.programs.rc_status)
        .arg("--crashed")
        .output()
        .with_context(|| format!("running '{} --crashed'", target.programs.rc_status))?;
    let crashed = String::from_utf8_lossy(&out.stdout);
    // It exits non-zero when it lists a crashed daemon; non-zero with nothing
    // listed is rc-status itself failing.
    if !out.status.success() && crashed.trim().is_empty() {
        anyhow::bail!(
            "'{} --crashed' failed ({}): {}",
            target.programs.rc_status,
            target_root::status_text(out.status),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    failed.extend(crashed.split_whitespace().map(str::to_string));
    Ok(failed)
}

/// An `openrc` process (running a runlevel) exists.
fn openrc_running(target: &Target) -> Result<bool> {
    for name in target_root::dir_names(&target.proc_dir)? {
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // A process that exited since the listing has no comm any more.
        let comm = target_root::read_optional(&target.proc_dir.join(&name).join("comm"));
        match comm {
            Ok(Some(comm)) if comm.trim() == "openrc" => return Ok(true),
            Ok(_) => {}
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(false)
}

/// Ask for a clean reboot without waiting for it: systemd's, or busybox
/// init's (AcornOS boots busybox init from an inittab that runs OpenRC).
pub(crate) fn reboot(init: InitSystem, target: &Target) -> Result<()> {
    let programs = &target.programs;
    let mut command = match init {
        InitSystem::Systemd => {
            let mut command = Command::new(&programs.systemctl);
            command.args(["--no-block", "reboot"]);
            command
        }
        InitSystem::Openrc => Command::new(&programs.reboot),
    };
    target_root::run(&mut command)?;
    Ok(())
}
