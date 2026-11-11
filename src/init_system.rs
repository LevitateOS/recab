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
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::state::Startup;
use crate::target::Target;

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
    /// The schema of this record.
    pub schema_version: u32,
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
        Self {
            schema_version: 1,
            init,
            startup,
            finished,
            state,
            failed,
        }
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
    let _signals = AssessmentSignals::new()?;
    let deadline = Instant::now() + timeout;
    let result = match init {
        InitSystem::Systemd => systemd_startup(&target.programs.systemctl, deadline),
        InitSystem::Openrc => openrc_startup(target, deadline),
    };
    match result {
        Err(_) if CANCELLED.load(Ordering::Relaxed) => Ok(Health::new(
            init,
            Startup::Inconclusive,
            "assessment stopped for shutdown".into(),
            vec![],
        )),
        Err(e) if e.is::<AssessmentTimeout>() => Ok(Health::new(
            init,
            Startup::TimedOut,
            TIMED_OUT.into(),
            vec![],
        )),
        other => other,
    }
}

#[derive(Debug)]
struct AssessmentTimeout;
impl std::fmt::Display for AssessmentTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("health assessment deadline exceeded")
    }
}
impl std::error::Error for AssessmentTimeout {}
static CANCELLED: AtomicBool = AtomicBool::new(false);
extern "C" fn cancel_assessment(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}
struct AssessmentSignals(Vec<(libc::c_int, libc::sigaction)>);
impl AssessmentSignals {
    fn new() -> Result<Self> {
        CANCELLED.store(false, Ordering::Relaxed);
        let mut guard = Self(vec![]);
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            let mut old = action;
            action.sa_sigaction = cancel_assessment as *const () as usize;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
            }
            if unsafe { libc::sigaction(signal, &action, &mut old) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            guard.0.push((signal, old));
        }
        Ok(guard)
    }
}
impl Drop for AssessmentSignals {
    fn drop(&mut self) {
        for (signal, old) in &self.0 {
            unsafe {
                libc::sigaction(*signal, old, std::ptr::null_mut());
            }
        }
    }
}
fn within(deadline: Instant) -> Result<()> {
    anyhow::ensure!(
        !CANCELLED.load(Ordering::Relaxed),
        "health assessment stopped for shutdown"
    );
    if Instant::now() >= deadline {
        return Err(AssessmentTimeout.into());
    }
    Ok(())
}

// Drain both pipes without blocking, including after the leader exits: a child
// may still own a pipe. Kill the whole helper group and reap on every exit path.
struct Helper(std::process::Child);
impl Drop for Helper {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
fn assess(command: &mut Command, deadline: Instant) -> Result<std::process::Output> {
    within(deadline)?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut helper = Helper(
        command
            .spawn()
            .with_context(|| format!("running {command:?}"))?,
    );
    let mut stdout = helper.0.stdout.take().unwrap();
    let mut stderr = helper.0.stderr.take().unwrap();
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let mut out = vec![];
    let mut err = vec![];
    let mut closed = [false; 2];
    loop {
        within(deadline)?;
        for (index, stream, bytes) in [
            (0, &mut stdout as &mut dyn Read, &mut out),
            (1, &mut stderr as &mut dyn Read, &mut err),
        ] {
            let mut buf = [0u8; 8192];
            // Bound each pass so noisy output cannot starve the deadline.
            for _ in 0..16 {
                match stream.read(&mut buf) {
                    Ok(0) => {
                        closed[index] = true;
                        break;
                    }
                    Ok(n) => {
                        bytes.extend_from_slice(&buf[..n]);
                        anyhow::ensure!(
                            bytes.len() <= 1024 * 1024,
                            "health helper output exceeds 1 MiB"
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if let Some(status) = helper.0.try_wait()? {
            if closed == [true, true] {
                return Ok(std::process::Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Only an explicit shutdown state has no verdict. An unreachable init or an
/// unrecognized answer is an assessment error, so a confirmed trial recovers.
fn systemd_startup(systemctl: &str, deadline: Instant) -> Result<Health> {
    let out = assess(
        Command::new(systemctl).args(["is-system-running", "--wait"]),
        deadline,
    )?;
    let answer = std::str::from_utf8(&out.stdout)?.trim();
    let startup = match answer {
        "running" if out.status.success() => Startup::Running,
        "degraded" => Startup::Failed,
        "maintenance" => Startup::Emergency,
        "stopping" => {
            return Ok(Health::new(
                InitSystem::Systemd,
                Startup::Inconclusive,
                answer.into(),
                vec![],
            ));
        }
        _ => anyhow::bail!(
            "systemctl health assessment failed ({}): {} {}",
            out.status,
            answer,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    };
    let failed = systemd_failed_units(systemctl, deadline)?;
    let startup = if startup == Startup::Running && !failed.is_empty() {
        Startup::Failed
    } else {
        startup
    };
    Ok(Health::new(
        InitSystem::Systemd,
        startup,
        answer.into(),
        failed,
    ))
}
fn systemd_failed_units(systemctl: &str, deadline: Instant) -> Result<Vec<String>> {
    let out = assess(
        Command::new(systemctl).args([
            "list-units",
            "--failed",
            "--no-legend",
            "--plain",
            "--no-pager",
        ]),
        deadline,
    )?;
    anyhow::ensure!(
        out.status.success(),
        "systemctl list-units failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(std::str::from_utf8(&out.stdout)?
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
fn openrc_startup(target: &Target, deadline: Instant) -> Result<Health> {
    let init = InitSystem::Openrc;
    let dir = &target.openrc_dir;
    loop {
        within(deadline)?;
        let level = target_root::read_optional(&dir.join("softlevel"))?.unwrap_or_default();
        let level = level.trim().to_string();
        let startup = match level.as_str() {
            "default"
                if target_root::dir_names(&dir.join("starting"))?.is_empty()
                    && !openrc_running(target)? =>
            {
                let failed = openrc_failed(target, deadline)?;
                let startup = if failed.is_empty() {
                    Startup::Running
                } else {
                    Startup::Failed
                };
                return Ok(Health::new(init, startup, level, failed));
            }
            "single" => Some(Startup::Emergency),
            "shutdown" | "reboot" => Some(Startup::Inconclusive),
            _ => None,
        };
        if let Some(startup) = startup {
            return Ok(Health::new(
                init,
                startup,
                level,
                if startup == Startup::Inconclusive {
                    vec![]
                } else {
                    openrc_failed(target, deadline)?
                },
            ));
        }
        if Instant::now() >= deadline {
            return Ok(Health::new(
                init,
                Startup::TimedOut,
                TIMED_OUT.to_string(),
                openrc_failed(target, deadline)?,
            ));
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
fn openrc_failed(target: &Target, deadline: Instant) -> Result<Vec<String>> {
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
    // rc-status --crashed exits 1 with no output when no daemon crashed,
    // and 0 when it lists crashed services (OpenRC's documented contract).
    // A missing binary, signal, unexpected status or diagnostic is an error.
    let out = assess(
        Command::new(&target.programs.rc_status).arg("--crashed"),
        deadline,
    )?;
    let crashed = String::from_utf8_lossy(&out.stdout);
    let no_crashes =
        out.status.code() == Some(1) && crashed.trim().is_empty() && out.stderr.is_empty();
    if (!out.status.success() && !no_crashes) || !out.stderr.is_empty() {
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
