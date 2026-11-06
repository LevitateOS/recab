//! recab's state file and audit log, and durable file replacement.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::error::{tagged_err, ErrorClass};
use crate::state::{Operation, PersistentState, STATE_SCHEMA_VERSION};
use crate::Slot;

pub(crate) struct Store {
    pub state_path: PathBuf,
    pub audit_path: PathBuf,
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
    pub fn from_env() -> Self {
        let path = |var: &str, default: &str| {
            PathBuf::from(std::env::var(var).unwrap_or_else(|_| default.to_string()))
        };
        Self {
            state_path: path("RECAB_STATE_PATH", "/var/lib/recab/state.json"),
            audit_path: path("RECAB_AUDIT_PATH", "/var/lib/recab/audit.log"),
        }
    }

    pub fn load(&self) -> Result<PersistentState> {
        let path = &self.state_path;
        let fail = |message: String| {
            tagged_err(
                ErrorClass::StateOrDetection,
                format!("failed loading recab state '{}': {message}", path.display()),
            )
        };
        let Some(content) = read_optional(path).map_err(|err| fail(format!("{err:#}")))? else {
            return Ok(PersistentState::default());
        };
        let state: PersistentState =
            serde_json::from_str(&content).map_err(|err| fail(err.to_string()))?;
        if state.schema != STATE_SCHEMA_VERSION {
            return Err(fail(format!(
                "schema {} is not the supported schema {STATE_SCHEMA_VERSION}",
                state.schema
            )));
        }
        Ok(state)
    }

    pub fn save(&self, state: &PersistentState, during: &str) -> Result<()> {
        let payload = serde_json::to_vec_pretty(state).context("serializing recab state")?;
        write_durable(&self.state_path, &payload).map_err(|err| {
            tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "failed writing recab state during {during} to '{}': {err:#}",
                    self.state_path.display()
                ),
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
        let event = AuditEvent {
            schema: STATE_SCHEMA_VERSION,
            timestamp_unix: after.last_transition_unix.unwrap_or_default(),
            operation: operation.name(),
            active_slot,
            before,
            after,
        };
        let result = (|| -> Result<()> {
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

pub(crate) fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(anyhow!(err).context(format!("reading '{}'", path.display()))),
    }
}

/// Replace `path` so that after a crash or power loss it holds either the
/// old or the new content: write a sibling temp file, flush it to disk,
/// rename it over `path`, and flush the directory so the rename persists.
/// (On FAT, as on the ESP, the rename itself is not guaranteed atomic.)
pub(crate) fn write_durable(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("'{}' has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating parent directory '{}'", parent.display()))?;
    let tmp = temp_sibling(path);
    let written = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(content)?;
        file.sync_all()
    })();
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow!(err).context(format!("writing temp file '{}'", tmp.display())));
    }
    commit_temp(&tmp, path)
}

/// Copy `source` over `dest` the same way as [`write_durable`].
pub(crate) fn copy_durable(source: &Path, dest: &Path) -> Result<()> {
    let tmp = temp_sibling(dest);
    let copied = fs::copy(source, &tmp).and_then(|_| File::open(&tmp)?.sync_all());
    if let Err(err) = copied {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow!(err).context(format!(
            "copying '{}' to '{}'",
            source.display(),
            tmp.display()
        )));
    }
    commit_temp(&tmp, dest)
}

fn temp_sibling(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    path.with_file_name(format!(".{name}.recab-tmp-{}", std::process::id()))
}

fn commit_temp(tmp: &Path, path: &Path) -> Result<()> {
    if let Err(err) = fs::rename(tmp, path) {
        let _ = fs::remove_file(tmp);
        return Err(anyhow!(err).context(format!(
            "renaming '{}' to '{}'",
            tmp.display(),
            path.display()
        )));
    }
    sync_dir(path.parent().unwrap_or(Path::new(".")))
}

/// Flush a directory's entries. Filesystems that cannot fsync a directory
/// (EINVAL) persist renames on their own schedule; that is all they offer.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    match File::open(dir).and_then(|d| d.sync_all()) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        Err(err) => Err(anyhow!(err).context(format!("syncing directory '{}'", dir.display()))),
    }
}
