//! recab's init adapter: what a trial needs from the running init system,
//! the one from the system's distro profile (the `REC_*` fields of its
//! os-release). Three operations: wait until startup finished and say how it
//! ended ([`health`]), reboot ([`reboot`]), and the kernel option that
//! reboots on a crashing PID 1 ([`crash_option`]).
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

/// Seconds startup may take before a trial counts as failed: `recab trial`'s
/// (and so the trial service's) deadline, and `recab health`'s wait.
pub const TRIAL_TIMEOUT_SECS: u64 = 900;

/// How startup of the running system ended (`recab health --json`): the
/// verdict a trial judges by.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub init: InitSystem,
    /// How startup ended.
    pub startup: Startup,
    /// Startup ran to its end, healthy or not (`startup` is `running` or
    /// `failed`).
    pub finished: bool,
    /// The init's own word: systemd's `is-system-running` answer, OpenRC's
    /// softlevel, or `timed out`.
    pub state: String,
    /// The units or services that failed (and, on OpenRC, crashed daemons),
    /// sorted, by bare name; listed even when startup did not finish.
    pub failed: Vec<String>,
}

impl Health {
    fn new(init: InitSystem, startup: Startup, state: String, mut failed: Vec<String>) -> Self {
        failed.sort();
        failed.dedup();
        let finished = matches!(startup, Startup::Running | Startup::Failed);
        Self { init, startup, finished, state, failed }
    }

    /// How startup ended, in words (a trial's record and message), for a
    /// wait of at most `timeout`.
    pub fn describe(&self, timeout: Duration) -> String {
        let how = match self.startup {
            Startup::Running => "finished starting".to_string(),
            Startup::Failed => format!("finished starting in state '{}'", self.state),
            Startup::Emergency => format!("ended in the init's emergency state '{}'", self.state),
            Startup::TimedOut => format!("did not finish starting within {}s", timeout.as_secs()),
            Startup::Inconclusive => format!("ended without a verdict ('{}')", self.state),
        };
        if self.failed.is_empty() {
            how
        } else {
            format!("{how} (failed: {})", self.failed.join(", "))
        }
    }
}

/// The word `state` takes when the deadline passed.
const TIMED_OUT: &str = "timed out";
/// How often an init's progress is looked at while waiting.
const POLL: Duration = Duration::from_millis(250);

/// Wait until startup of the system `target` runs finished, at most
/// `timeout`, and name what failed.
pub(crate) fn health(target: &Target, timeout: Duration) -> Result<Health> {
    let init = target.init()?;
    match init {
        InitSystem::Systemd => systemd_startup(&target.programs.systemctl, timeout),
        InitSystem::Openrc => openrc_startup(target, timeout),
    }
}

/// `systemctl is-system-running --wait`: `running`, failed units
/// (`degraded`), or the emergency/rescue target (`maintenance`); any other
/// answer (`stopping`, `offline`, ...) is a shutdown or an unreachable
/// systemd, with no verdict.
fn systemd_startup(systemctl: &str, timeout: Duration) -> Result<Health> {
    let init = InitSystem::Systemd;
    let mut child = Command::new(systemctl)
        .args(["is-system-running", "--wait"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running '{systemctl} is-system-running --wait'"))?;
    let deadline = Instant::now() + timeout;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill().context("stopping 'systemctl is-system-running --wait'")?;
            child.wait().context("waiting for 'systemctl is-system-running --wait'")?;
            return Ok(Health::new(init, Startup::TimedOut, TIMED_OUT.to_string(), systemd_failed_units(systemctl)?));
        }
        std::thread::sleep(POLL);
    }
    let read = |stream: Option<&mut dyn Read>| -> Result<String> {
        let mut text = String::new();
        if let Some(stream) = stream {
            stream.read_to_string(&mut text)?;
        }
        Ok(text.trim().to_string())
    };
    let answer = read(child.stdout.as_mut().map(|s| s as &mut dyn Read))?;
    let complaint = read(child.stderr.as_mut().map(|s| s as &mut dyn Read))?;
    let failed = systemd_failed_units(systemctl)?;
    let startup = match answer.as_str() {
        "running" => Startup::Running,
        "degraded" => Startup::Failed,
        "maintenance" => Startup::Emergency,
        _ => Startup::Inconclusive,
    };
    // No answer at all: say what systemctl said instead.
    let state = if answer.is_empty() { format!("no answer: {complaint}") } else { answer };
    Ok(Health::new(init, startup, state, failed))
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
/// It failed when a service failed to start, and ended in the emergency
/// state in the single-user runlevel. OpenRC's state directory holds the
/// runlevel being entered (`softlevel`) and one entry per service in each
/// state (`starting/`, `failed/`).
fn openrc_startup(target: &Target, timeout: Duration) -> Result<Health> {
    let init = InitSystem::Openrc;
    let dir = &target.openrc_dir;
    let deadline = Instant::now() + timeout;
    loop {
        let level = target_root::read_optional(&dir.join("softlevel"))?.unwrap_or_default();
        let level = level.trim().to_string();
        let startup = match level.as_str() {
            "default" if target_root::dir_names(&dir.join("starting"))?.is_empty() && !openrc_running(target)? => {
                let failed = openrc_failed(target)?;
                let startup = if failed.is_empty() { Startup::Running } else { Startup::Failed };
                return Ok(Health::new(init, startup, level, failed));
            }
            "single" => Some(Startup::Emergency),
            "shutdown" | "reboot" => Some(Startup::Inconclusive),
            _ => None,
        };
        if let Some(startup) = startup {
            return Ok(Health::new(init, startup, level, openrc_failed(target)?));
        }
        if Instant::now() >= deadline {
            return Ok(Health::new(init, Startup::TimedOut, TIMED_OUT.to_string(), openrc_failed(target)?));
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
