//! recab's state file, audit log and run lock.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::target::Target;
use crate::error::{classified, refuse, ErrorClass};
use crate::state::{Operation, PersistentState, Slot};

/// The audit log record's schema (one JSON object per line). Each record
/// embeds the state before and after the transition, which carries its own
/// `schema`, so this number covers only the record's own fields.
const AUDIT_SCHEMA_VERSION: u32 = 1;

pub(crate) struct Store {
    state_path: PathBuf,
    audit_path: PathBuf,
    lock_path: PathBuf,
}

/// Held for one recab run that changes anything, so runs never interleave.
pub(crate) struct Lock {
    _file: File,
}

#[derive(Debug, Serialize)]
struct AuditEvent<'a> {
    schema: u32,
    timestamp_unix: u64,
    operation: &'static str,
    active_slot: Slot,
    before: &'a PersistentState,
    after: &'a PersistentState,
}

impl Store {
    pub fn new(target: &Target) -> Self {
        Self {
            state_path: target.state_path(),
            audit_path: target.audit_path(),
            lock_path: target.lock.clone(),
        }
    }

    /// Wait for, then hold, the exclusive recab lock.
    pub fn lock(&self) -> Result<Lock> {
        use std::os::fd::AsRawFd;
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating '{}'", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock_path)
            .with_context(|| format!("opening lock '{}'", self.lock_path.display()))?;
        // SAFETY: flock on a file descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(anyhow!(std::io::Error::last_os_error())
                .context(format!("locking '{}'", self.lock_path.display())));
        }
        Ok(Lock { _file: file })
    }


    /// recab's state, `None` when it has written none here.
    pub fn load(&self) -> Result<Option<PersistentState>> {
        let path = &self.state_path;
        let fail = |message: String| {
            refuse(format!("failed loading recab state '{}': {message}", path.display()),
            )
        };
        let Some(content) = target_root::read_optional(path).map_err(|err| fail(err.to_string()))? else {
            return Ok(None);
        };
        let state: PersistentState =
            serde_json::from_str(&content).map_err(|err| fail(err.to_string()))?;
        state.check_schema().map_err(fail)?;
        Ok(Some(state))
    }

    pub fn save(&self, state: &PersistentState) -> Result<()> {
        let payload = serde_json::to_vec_pretty(state).context("serializing recab state")?;
        target_root::write_durable(&self.state_path, &payload, None).map_err(|err| {
            classified(
                ErrorClass::StateOrDetection,
                format!("failed writing recab state to '{}': {err:#}", self.state_path.display()),
            )
        })
    }

    /// Append an audit event. The log is a record, not state: a failure to
    /// write it is reported as a warning and does not undo the operation.
    pub fn audit(
        &self,
        operation: Operation,
        active_slot: Slot,
        before: &PersistentState,
        after: &PersistentState,
    ) {
        let result = (|| -> Result<()> {
            let event = AuditEvent {
                schema: AUDIT_SCHEMA_VERSION,
                timestamp_unix: now()?,
                operation: operation.name(),
                active_slot,
                before,
                after,
            };
            if let Some(parent) = self.audit_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut line = serde_json::to_string(&event)?;
            line.push('\n');
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.audit_path)?;
            file.write_all(line.as_bytes())?;
            file.sync_data()?;
            Ok(())
        })();
        if let Err(err) = result {
            eprintln!(
                "recab: warning: {} succeeded but the audit log '{}' was not written: {err:#}",
                operation.name(),
                self.audit_path.display()
            );
        }
    }
}

/// Seconds since the epoch, for the state's and the audit log's timestamps.
pub(crate) fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock before UNIX_EPOCH"))?
        .as_secs())
}
