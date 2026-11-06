//! What `recab trial` asks systemd: how startup ended, which units failed,
//! and the reboot that ends a failed trial.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) struct Systemd {
    systemctl: String,
}

/// How system startup ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Startup {
    /// Started with no failed units.
    Running,
    /// Finished in another state: `degraded`, `maintenance`, ...
    Finished(String),
    /// Still starting when the deadline passed.
    TimedOut,
}

impl Systemd {
    pub fn from_env() -> Self {
        Self {
            systemctl: std::env::var("RECAB_SYSTEMCTL_BIN").unwrap_or_else(|_| "systemctl".to_string()),
        }
    }

    /// Wait until startup finishes, at most `timeout`.
    pub fn wait_for_startup(&self, timeout: Duration) -> Result<Startup> {
        let mut child = Command::new(&self.systemctl)
            .args(["is-system-running", "--wait"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("running '{} is-system-running --wait'", self.systemctl))?;
        let deadline = Instant::now() + timeout;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Startup::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let mut state = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout.read_to_string(&mut state)?;
        }
        Ok(match state.trim() {
            "running" => Startup::Running,
            "" => Startup::Finished("unknown".to_string()),
            other => Startup::Finished(other.to_string()),
        })
    }

    /// Names of the units in the failed state.
    pub fn failed_units(&self) -> Vec<String> {
        Command::new(&self.systemctl)
            .args(["list-units", "--failed", "--no-legend", "--plain", "--no-pager"])
            .output()
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter_map(|line| line.split_whitespace().next().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Ask systemd for a clean reboot without waiting for it.
    pub fn reboot(&self) -> Result<()> {
        let status = Command::new(&self.systemctl)
            .args(["--no-block", "reboot"])
            .status()
            .with_context(|| format!("running '{} reboot'", self.systemctl))?;
        if !status.success() {
            bail!("'{} reboot' failed ({status})", self.systemctl);
        }
        Ok(())
    }
}
