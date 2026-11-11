//! `recab history`: what happened to this computer's systems, newest first.
//!
//! Read-only. It reads two things recab already keeps and writes neither: the
//! audit log (`audit.log`, one record per state transition, each holding the
//! state before and after) and the state file (the open preparation's changes,
//! which no transition has audited yet). Nothing here is remembered anywhere
//! else, so nothing is claimed that those two do not say.
//!
//! What the log can and cannot carry:
//! - Every transition has a time and a name, and the state on both sides. The
//!   slot a transition concerns is not written by itself; it is read from the
//!   preparation (prepare, update, finalize, discard) or from the state's own
//!   `last_operation` (the others), never guessed.
//! - A finished change is not a transition and is not audited. It is read from
//!   the preparations the records hold, and from the state file, once each.
//!   This is the only place `--actor` is kept, so only a change has an actor.
//! - The end of `prepare` is not audited either: an entry for it says the next
//!   system was begun (or reopened), or that it stopped.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

use crate::error::{ErrorClass, classified};
use crate::state::{PersistentState, PreparationPhase, Slot};
use crate::store::Store;
use crate::target::Target;

/// The schema of [`History`].
pub const HISTORY_SCHEMA_VERSION: u32 = 1;

/// A log larger than this is read from its end: an old computer keeps
/// answering, and says it left the oldest records out.
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// One thing that happened.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Seconds since the epoch.
    pub time: u64,
    /// `init`, `prepare`, `change`, `update`, `finalize`, `set-next`,
    /// `trial-kept`, `trial-failed`, `trial-missed`, `commit`, `rollback` or
    /// `discard`: the audit log's own names, and `change` and `trial-kept`
    /// for what its records show without naming (see the module).
    pub kind: String,
    /// The system it concerns, when the record shows which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<Slot>,
    /// The preparation it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation: Option<String>,
    /// `change`: the change's id, its action (`install`, `update`, `remove`,
    /// `edit`) and package or path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Who asked, as the caller named itself (`--actor`): only for a change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// `started`, `done` or `interrupted` for prepare, update, finalize and
    /// discard; `restart_cancelled` for a commit that took a set restart back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// recab's own words, where the record holds them: how startup ended for
    /// a trial, why an operation stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What `recab history --json` prints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct History {
    pub schema_version: u32,
    /// Newest first.
    pub entries: Vec<HistoryEntry>,
    /// Audit records read, and lines that could not be read.
    pub records: usize,
    pub unreadable: usize,
    /// The log was longer than recab reads; the oldest records are missing.
    pub truncated: bool,
    /// An audit log exists. Without one only the open preparation's changes
    /// can be listed.
    pub log_found: bool,
}

#[derive(Deserialize)]
struct Record {
    schema: u32,
    timestamp_unix: u64,
    operation: String,
    active_slot: Slot,
    before: PersistentState,
    after: PersistentState,
}

fn slot_of_label(label: Option<&str>, operation: &str) -> Option<Slot> {
    let (name, slot) = label?.rsplit_once('-')?;
    if name != operation {
        return None;
    }
    slot.parse().ok()
}

fn entry(time: u64, kind: &str) -> HistoryEntry {
    HistoryEntry {
        time,
        kind: kind.into(),
        slot: None,
        preparation: None,
        change: None,
        action: None,
        package: None,
        actor: None,
        outcome: None,
        detail: None,
    }
}

/// The entry one audit record stands for.
fn from_record(record: &Record) -> HistoryEntry {
    let operation = record.operation.as_str();
    let mut out = entry(record.timestamp_unix, operation);
    match operation {
        "prepare" | "update" | "finalize" | "discard" => {
            let prep = record
                .after
                .preparation
                .as_ref()
                .or(record.before.preparation.as_ref());
            out.slot = prep.map(|p| p.slot);
            out.preparation = prep.map(|p| p.id.clone());
            let after = record.after.preparation.as_ref();
            let (outcome, detail) = match after.map(|p| (&p.phase, p.failure.clone())) {
                Some((PreparationPhase::Interrupted, failure)) => ("interrupted", failure),
                Some((PreparationPhase::Prepared | PreparationPhase::Ready, _)) => ("done", None),
                // A discard that left no preparation behind is finished.
                None if operation == "discard" => ("done", None),
                _ => ("started", None),
            };
            out.outcome = Some(outcome.into());
            out.detail = detail;
        }
        "commit" => {
            let kept = record.after.last_trial.as_ref().filter(|trial| {
                trial.committed && record.before.last_trial.as_ref() != Some(*trial)
            });
            if let Some(trial) = kept {
                out.kind = "trial-kept".into();
                out.slot = Some(trial.slot);
                out.detail = Some(trial.detail.clone());
            } else {
                out.slot = slot_of_label(record.after.last_operation.as_deref(), operation)
                    .or(Some(record.active_slot));
                if record.before.pending_trial_slot.is_some()
                    && record.after.pending_trial_slot.is_none()
                {
                    out.outcome = Some("restart_cancelled".into());
                }
            }
        }
        "trial-failed" => {
            // The log's active slot is the candidate that failed.
            out.slot = Some(record.active_slot);
            out.detail = record
                .after
                .last_trial
                .as_ref()
                .map(|trial| trial.detail.clone());
        }
        "set-next" => {
            out.slot = slot_of_label(record.after.last_operation.as_deref(), operation)
                .or(record.after.pending_trial_slot);
        }
        _ => {
            out.slot = slot_of_label(record.after.last_operation.as_deref(), operation);
        }
    }
    out
}

/// The changes a state's preparation holds, as entries.
fn changes_of(
    state: &PersistentState,
    seen: &mut HashSet<String>,
    into: &mut Vec<(u64, HistoryEntry)>,
    order: u64,
) {
    let Some(preparation) = &state.preparation else {
        return;
    };
    for change in &preparation.changes {
        if !seen.insert(change.id.clone()) {
            continue;
        }
        let mut out = entry(change.unix_time, "change");
        out.slot = Some(preparation.slot);
        out.preparation = Some(preparation.id.clone());
        out.change = Some(change.id.clone());
        out.action = Some(change.action.clone());
        out.package = Some(change.package.clone());
        out.actor = change.actor.clone();
        into.push((order, out));
    }
}

/// The audit log's text, from its end when it is longer than recab reads.
fn read_log(path: &std::path::Path) -> Result<Option<(String, bool)>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(classified(
                ErrorClass::Permission,
                format!(
                    "the audit log '{}' cannot be read by this user",
                    path.display()
                ),
            ));
        }
        Err(error) => return Err(error).with_context(|| format!("opening '{}'", path.display())),
    };
    let length = file.metadata()?.len();
    let truncated = length > MAX_LOG_BYTES;
    if truncated {
        file.seek(SeekFrom::Start(length - MAX_LOG_BYTES))?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_LOG_BYTES).read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        // The first line is cut somewhere in its middle.
        text = text
            .split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default();
    }
    Ok(Some((text, truncated)))
}

/// What happened to this computer's systems, newest first.
pub fn history() -> Result<History> {
    let target = Target::running()?;
    let store = Store::new(&target);
    let log = read_log(&target.audit_path())?;
    let mut ordered: Vec<(u64, HistoryEntry)> = Vec::new();
    let mut seen = HashSet::new();
    let (mut records, mut unreadable) = (0usize, 0usize);
    let (truncated, log_found) = match &log {
        Some((_, truncated)) => (*truncated, true),
        None => (false, false),
    };
    for line in log
        .iter()
        .flat_map(|(text, _)| text.lines())
        .filter(|line| !line.trim().is_empty())
    {
        // A record this recab does not understand is counted, not guessed at.
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            unreadable += 1;
            continue;
        };
        if record.schema != 1 {
            unreadable += 1;
            continue;
        }
        let order = records as u64 * 2;
        records += 1;
        // A change is older than the record that first shows it.
        changes_of(&record.before, &mut seen, &mut ordered, order);
        changes_of(&record.after, &mut seen, &mut ordered, order);
        ordered.push((order + 1, from_record(&record)));
    }
    if let Some(state) = store.load()? {
        changes_of(&state, &mut seen, &mut ordered, records as u64 * 2 + 1);
    }
    // Newest first; the later in the log, the newer, when times are equal.
    ordered.sort_by_key(|a| std::cmp::Reverse((a.1.time, a.0)));
    Ok(History {
        schema_version: HISTORY_SCHEMA_VERSION,
        entries: ordered.into_iter().map(|(_, entry)| entry).collect(),
        records,
        unreadable,
        truncated,
        log_found,
    })
}

/// UTC date and time of `seconds` since the epoch ("2026-11-10 21:04:12").
fn utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let day_of_year = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The text form: one line to an entry.
pub fn print(history: &History) {
    for entry in &history.entries {
        let mut line = format!("{}  {:<12}", utc(entry.time), entry.kind);
        if let Some(slot) = entry.slot {
            line += &format!(" slot {slot}");
        }
        if let (Some(action), Some(package)) = (&entry.action, &entry.package) {
            line += &format!(" {action} {package}");
        }
        if let Some(outcome) = &entry.outcome {
            line += &format!(" [{outcome}]");
        }
        if let Some(actor) = &entry.actor {
            line += &format!(" (by {actor})");
        }
        if let Some(detail) = &entry.detail {
            line += &format!(": {detail}");
        }
        println!("{line}");
    }
    if history.entries.is_empty() {
        println!("recab has recorded nothing yet.");
    }
    if !history.log_found {
        println!("(no audit log: only the open preparation's changes can be listed)");
    }
    if history.unreadable > 0 {
        println!("({} log lines could not be read)", history.unreadable);
    }
    if history.truncated {
        println!("(the log is long: the oldest records are left out)");
    }
}
