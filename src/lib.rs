//! recab - A/B slot management primitives.
//!
//! recab is intentionally small:
//! - detect active/inactive slot
//! - stage a trial boot for inactive slot
//! - commit active slot as known-good
//! - rollback next boot to known-good slot
//!
//! Backend: systemd-boot entries + local persistent state.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_BOOT_ROOT: &str = "/boot";
const DEFAULT_OS_RELEASE: &str = "/etc/os-release";
const DEFAULT_PROC_CMDLINE: &str = "/proc/cmdline";
const DEFAULT_SYS_FIRMWARE_EFI: &str = "/sys/firmware/efi";
const DEFAULT_EFIVARS_PATH: &str = "/sys/firmware/efi/efivars";
const DEFAULT_STATE_PATH: &str = "/var/lib/recab/state.json";
const DEFAULT_AUDIT_PATH: &str = "/var/lib/recab/audit.log";
const DEFAULT_BOOTCTL_BIN: &str = "bootctl";

pub const EXIT_CODE_GENERIC: u8 = 1;
pub const EXIT_CODE_PERMISSION: u8 = 13;
pub const EXIT_CODE_BACKEND_UNAVAILABLE: u8 = 20;
pub const EXIT_CODE_STATE_OR_DETECTION: u8 = 21;
pub const EXIT_CODE_PARTIAL_UPDATE: u8 = 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorClass {
    Permission,
    BackendUnavailable,
    StateOrDetection,
    PartialUpdate,
}

impl ErrorClass {
    fn code(self) -> u8 {
        match self {
            ErrorClass::Permission => EXIT_CODE_PERMISSION,
            ErrorClass::BackendUnavailable => EXIT_CODE_BACKEND_UNAVAILABLE,
            ErrorClass::StateOrDetection => EXIT_CODE_STATE_OR_DETECTION,
            ErrorClass::PartialUpdate => EXIT_CODE_PARTIAL_UPDATE,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            ErrorClass::Permission => "RECAB_ERR_PERMISSION",
            ErrorClass::BackendUnavailable => "RECAB_ERR_BACKEND",
            ErrorClass::StateOrDetection => "RECAB_ERR_STATE",
            ErrorClass::PartialUpdate => "RECAB_ERR_PARTIAL_UPDATE",
        }
    }
}

fn tagged_err(class: ErrorClass, message: impl Into<String>) -> anyhow::Error {
    anyhow!("[{}] {}", class.tag(), message.into())
}

fn has_class_tag(err: &anyhow::Error, class: ErrorClass) -> bool {
    let tag = class.tag();
    err.chain().any(|cause| cause.to_string().contains(tag))
}

/// Stable exit-code mapping for recab CLI and callers.
pub fn stable_exit_code(err: &anyhow::Error) -> u8 {
    for class in [
        ErrorClass::Permission,
        ErrorClass::BackendUnavailable,
        ErrorClass::PartialUpdate,
        ErrorClass::StateOrDetection,
    ] {
        if has_class_tag(err, class) {
            return class.code();
        }
    }

    let msg = format!("{err:#}").to_ascii_lowercase();
    if msg.contains("must run as root") || msg.contains("requires root privileges") {
        return EXIT_CODE_PERMISSION;
    }
    if msg.contains("systemd-boot backend requires")
        || msg.contains("missing '/sys/firmware/efi'")
        || msg.contains("missing efivarfs")
    {
        return EXIT_CODE_BACKEND_UNAVAILABLE;
    }
    if msg.contains("unable to determine active slot")
        || msg.contains("unable to determine a/b entry prefix")
        || msg.contains("known-good slot missing")
        || msg.contains("loader default slot")
    {
        return EXIT_CODE_STATE_OR_DETECTION;
    }
    EXIT_CODE_GENERIC
}

/// System slot identifier (A/B).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    pub fn as_letter(self) -> &'static str {
        match self {
            Slot::A => "A",
            Slot::B => "B",
        }
    }

    fn as_entry_suffix(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }
}

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_letter())
    }
}

/// Current A/B view exported by `recab status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub active_slot: Slot,
    pub inactive_slot: Slot,
    pub trial_pending: bool,
    pub next_boot: Option<Slot>,
    pub default_slot: Option<Slot>,
    pub known_good_slot: Option<Slot>,
    pub entry_prefix: String,
}

/// Query current A/B state using system backend + persisted local state.
pub fn status() -> Result<Status> {
    let backend = SystemdBootBackend::from_env();
    let store = JsonStateStore::from_env();
    status_with(&backend, &store)
}

/// Configure a trial boot of `slot`.
pub fn set_next(slot: Slot) -> Result<Status> {
    ensure_root().context("set-next requires root privileges")?;
    let backend = SystemdBootBackend::from_env();
    let store = JsonStateStore::from_env();
    set_next_with(&backend, &store, slot)
}

/// Mark current active slot as known-good and make it default.
pub fn commit() -> Result<Status> {
    ensure_root().context("commit requires root privileges")?;
    let backend = SystemdBootBackend::from_env();
    let store = JsonStateStore::from_env();
    commit_with(&backend, &store)
}

/// Configure next boot back to previously known-good slot.
pub fn rollback() -> Result<Status> {
    ensure_root().context("rollback requires root privileges")?;
    let backend = SystemdBootBackend::from_env();
    let store = JsonStateStore::from_env();
    rollback_with(&backend, &store)
}

fn status_with<B: BootBackend, S: StateStore>(backend: &B, store: &S) -> Result<Status> {
    let active = backend.active_slot()?;
    let default = backend.default_slot()?;
    let entry_prefix = backend.entry_prefix()?;
    let state = load_state(store)?;
    Ok(compose_status(active, default, &state, entry_prefix))
}

fn set_next_with<B: BootBackend, S: StateStore>(
    backend: &B,
    store: &S,
    slot: Slot,
) -> Result<Status> {
    let active = backend.active_slot()?;
    let default_before = backend.default_slot()?;
    let entry_prefix = backend.entry_prefix()?;
    let previous = load_state(store)?;
    let mut next_state = previous.clone();

    let fallback_slot = previous
        .known_good_slot
        .or(default_before)
        .unwrap_or(active);

    if active == slot {
        next_state.pending_trial_slot = None;
        next_state.last_operation = Some("set-next-noop-active".to_string());
        next_state.last_transition_unix = Some(now_unix_secs()?);
        save_state(store, &next_state, "set-next-noop-active")?;
        append_audit_best_effort(
            store,
            &AuditEvent::new("set-next-noop-active", active, &previous, &next_state)?,
        );
        return Ok(compose_status(
            active,
            default_before,
            &next_state,
            entry_prefix,
        ));
    }

    if previous.pending_trial_slot == Some(slot) {
        return Ok(compose_status(
            active,
            default_before,
            &previous,
            entry_prefix,
        ));
    }

    if default_before != Some(fallback_slot) {
        return Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "cannot stage trial boot: loader default slot ({}) does not match known-good slot ({fallback_slot}).\n\
Run 'recab commit' to set a stable fallback default before 'recab set-next {slot}'.",
                default_before
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "none".to_string())
            ),
        ));
    }

    next_state.known_good_slot = Some(fallback_slot);
    next_state.pending_trial_slot = Some(slot);
    next_state.last_operation = Some(format!("set-next-{}", slot.as_letter()));
    next_state.last_transition_unix = Some(now_unix_secs()?);
    save_state(store, &next_state, "set-next")?;

    if let Err(err) = backend.set_trial_slot(slot).with_context(|| {
        format!(
            "failed to stage trial boot for slot {} via systemd-boot backend",
            slot
        )
    }) {
        return Err(revert_state_or_escalate(store, &previous, "set-next", err));
    }

    append_audit_best_effort(
        store,
        &AuditEvent::new("set-next", active, &previous, &next_state)?,
    );

    Ok(compose_status(
        active,
        default_before,
        &next_state,
        entry_prefix,
    ))
}

fn commit_with<B: BootBackend, S: StateStore>(backend: &B, store: &S) -> Result<Status> {
    let active = backend.active_slot()?;
    let entry_prefix = backend.entry_prefix()?;
    let previous = load_state(store)?;
    let mut next_state = previous.clone();

    next_state.known_good_slot = Some(active);
    next_state.pending_trial_slot = None;
    next_state.last_operation = Some(format!("commit-{}", active.as_letter()));
    next_state.last_transition_unix = Some(now_unix_secs()?);
    save_state(store, &next_state, "commit")?;

    if let Err(err) = backend.set_default_slot(active).with_context(|| {
        format!(
            "failed to set slot {} as default via systemd-boot backend",
            active
        )
    }) {
        return Err(revert_state_or_escalate(store, &previous, "commit", err));
    }

    append_audit_best_effort(
        store,
        &AuditEvent::new("commit", active, &previous, &next_state)?,
    );
    Ok(compose_status(
        active,
        Some(active),
        &next_state,
        entry_prefix,
    ))
}

fn rollback_with<B: BootBackend, S: StateStore>(backend: &B, store: &S) -> Result<Status> {
    let active = backend.active_slot()?;
    let entry_prefix = backend.entry_prefix()?;
    let previous = load_state(store)?;
    let mut next_state = previous.clone();

    let known_good = next_state.known_good_slot.ok_or_else(|| {
        tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "cannot rollback: known-good slot missing from state '{}'",
                store.state_path().display()
            ),
        )
    })?;

    next_state.pending_trial_slot = None;
    next_state.last_operation = Some(format!("rollback-to-{}", known_good.as_letter()));
    next_state.last_transition_unix = Some(now_unix_secs()?);
    save_state(store, &next_state, "rollback")?;

    if let Err(err) = backend.set_default_slot(known_good).with_context(|| {
        format!(
            "failed to set known-good slot {} as default for rollback",
            known_good
        )
    }) {
        return Err(revert_state_or_escalate(store, &previous, "rollback", err));
    }

    append_audit_best_effort(
        store,
        &AuditEvent::new("rollback", active, &previous, &next_state)?,
    );
    Ok(compose_status(
        active,
        Some(known_good),
        &next_state,
        entry_prefix,
    ))
}

fn compose_status(
    active: Slot,
    default_slot: Option<Slot>,
    state: &PersistentState,
    entry_prefix: String,
) -> Status {
    let known_good = state.known_good_slot.or(default_slot);
    let next_boot = state.pending_trial_slot;
    Status {
        active_slot: active,
        inactive_slot: active.other(),
        trial_pending: next_boot.is_some(),
        next_boot,
        default_slot,
        known_good_slot: known_good,
        entry_prefix,
    }
}

fn ensure_root() -> Result<()> {
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        return Err(tagged_err(
            ErrorClass::Permission,
            "must run as root for mutating recab operations",
        ));
    }
    Ok(())
}

fn load_state<S: StateStore>(store: &S) -> Result<PersistentState> {
    store.load().map_err(|err| {
        tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "failed loading recab state '{}': {err:#}",
                store.state_path().display()
            ),
        )
    })
}

fn save_state<S: StateStore>(store: &S, state: &PersistentState, operation: &str) -> Result<()> {
    store.save(state).map_err(|err| {
        tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "failed writing recab state during {operation} to '{}': {err:#}",
                store.state_path().display()
            ),
        )
    })
}

fn append_audit_best_effort<S: StateStore>(store: &S, event: &AuditEvent) {
    let _ = store.append_audit(event);
}

fn revert_state_or_escalate<S: StateStore>(
    store: &S,
    previous: &PersistentState,
    operation: &str,
    cause: anyhow::Error,
) -> anyhow::Error {
    match save_state(store, previous, &format!("{operation}-revert")) {
        Ok(()) => cause.context(format!(
            "{operation} aborted; restored previous recab state snapshot"
        )),
        Err(revert_err) => tagged_err(
            ErrorClass::PartialUpdate,
            format!(
                "{operation} failed and state restore also failed.\n\
primary error: {cause:#}\n\
state-restore error: {revert_err:#}"
            ),
        ),
    }
}

fn now_unix_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before UNIX_EPOCH")?
        .as_secs())
}

trait BootBackend {
    fn active_slot(&self) -> Result<Slot>;
    fn default_slot(&self) -> Result<Option<Slot>>;
    fn set_default_slot(&self, slot: Slot) -> Result<()>;
    fn set_trial_slot(&self, slot: Slot) -> Result<()>;
    fn entry_prefix(&self) -> Result<String>;
}

trait StateStore {
    fn load(&self) -> Result<PersistentState>;
    fn save(&self, state: &PersistentState) -> Result<()>;
    fn append_audit(&self, event: &AuditEvent) -> Result<()>;
    fn state_path(&self) -> &Path;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistentState {
    schema: u32,
    known_good_slot: Option<Slot>,
    pending_trial_slot: Option<Slot>,
    last_operation: Option<String>,
    last_transition_unix: Option<u64>,
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA_VERSION,
            known_good_slot: None,
            pending_trial_slot: None,
            last_operation: None,
            last_transition_unix: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct AuditEvent {
    schema: u32,
    timestamp_unix: u64,
    operation: String,
    active_slot: Slot,
    before: PersistentState,
    after: PersistentState,
}

impl AuditEvent {
    fn new(
        operation: &str,
        active_slot: Slot,
        before: &PersistentState,
        after: &PersistentState,
    ) -> Result<Self> {
        Ok(Self {
            schema: STATE_SCHEMA_VERSION,
            timestamp_unix: now_unix_secs()?,
            operation: operation.to_string(),
            active_slot,
            before: before.clone(),
            after: after.clone(),
        })
    }
}

struct JsonStateStore {
    state_path: PathBuf,
    audit_path: PathBuf,
}

impl JsonStateStore {
    fn from_env() -> Self {
        let state_path = std::env::var("RECAB_STATE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_STATE_PATH));
        let audit_path = std::env::var("RECAB_AUDIT_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_AUDIT_PATH));
        Self {
            state_path,
            audit_path,
        }
    }
}

impl StateStore for JsonStateStore {
    fn load(&self) -> Result<PersistentState> {
        if !self.state_path.is_file() {
            return Ok(PersistentState::default());
        }
        let bytes = fs::read(&self.state_path)
            .with_context(|| format!("reading state '{}'", self.state_path.display()))?;
        let mut state: PersistentState = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing state '{}'", self.state_path.display()))?;
        if state.schema == 0 {
            state.schema = STATE_SCHEMA_VERSION;
        }
        Ok(state)
    }

    fn save(&self, state: &PersistentState) -> Result<()> {
        write_json_atomic(&self.state_path, state)
            .with_context(|| format!("writing state '{}'", self.state_path.display()))
    }

    fn append_audit(&self, event: &AuditEvent) -> Result<()> {
        if let Some(parent) = self.audit_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating audit directory '{}'", parent.display()))?;
        }
        let payload =
            serde_json::to_string(event).context("serializing recab audit event as json line")?;
        let mut fh = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_path)
            .with_context(|| format!("opening audit log '{}'", self.audit_path.display()))?;
        fh.write_all(payload.as_bytes())
            .with_context(|| format!("appending audit log '{}'", self.audit_path.display()))?;
        fh.write_all(b"\n")
            .with_context(|| format!("appending audit newline '{}'", self.audit_path.display()))?;
        Ok(())
    }

    fn state_path(&self) -> &Path {
        &self.state_path
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        anyhow!(
            "cannot atomically write path without parent: '{}'",
            path.display()
        )
    })?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating parent directory '{}'", parent.display()))?;

    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let payload = serde_json::to_vec_pretty(value).context("serializing json payload")?;
    fs::write(&tmp, payload).with_context(|| format!("writing temp file '{}'", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "renaming temp state '{}' to '{}'",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

struct SystemdBootBackend {
    loader_conf_path: PathBuf,
    entries_dir: PathBuf,
    os_release_path: PathBuf,
    proc_cmdline_path: PathBuf,
    sys_firmware_efi_path: PathBuf,
    efivars_path: PathBuf,
    bootctl_bin: String,
}

impl SystemdBootBackend {
    fn from_env() -> Self {
        let boot_root = std::env::var("RECAB_BOOT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_BOOT_ROOT));
        let loader_conf_path = boot_root.join("loader/loader.conf");
        let entries_dir = boot_root.join("loader/entries");
        let os_release_path = std::env::var("RECAB_OS_RELEASE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_OS_RELEASE));
        let proc_cmdline_path = std::env::var("RECAB_PROC_CMDLINE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_PROC_CMDLINE));
        let sys_firmware_efi_path = std::env::var("RECAB_SYS_FIRMWARE_EFI_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_SYS_FIRMWARE_EFI));
        let efivars_path = std::env::var("RECAB_EFIVARS_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_EFIVARS_PATH));
        let bootctl_bin =
            std::env::var("RECAB_BOOTCTL_BIN").unwrap_or_else(|_| DEFAULT_BOOTCTL_BIN.to_string());

        Self {
            loader_conf_path,
            entries_dir,
            os_release_path,
            proc_cmdline_path,
            sys_firmware_efi_path,
            efivars_path,
            bootctl_bin,
        }
    }

    fn ensure_uefi_ready(&self) -> Result<()> {
        validate_uefi_paths(&self.sys_firmware_efi_path, &self.efivars_path)
    }

    fn entry_filename_for(&self, slot: Slot) -> Result<String> {
        let prefix = self.entry_prefix()?;
        Ok(format!("{}-{}.conf", prefix, slot.as_entry_suffix()))
    }

    fn bootctl_available(&self) -> bool {
        match Command::new(&self.bootctl_bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) => status.success(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => false,
        }
    }

    fn run_bootctl(&self, args: &[&str]) -> Result<()> {
        let output = Command::new(&self.bootctl_bin)
            .args(args)
            .output()
            .with_context(|| format!("running '{}' with args {:?}", self.bootctl_bin, args))?;
        if output.status.success() {
            return Ok(());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} {:?} failed (exit {}): {}\n{}",
            self.bootctl_bin,
            args,
            output.status.code().unwrap_or(1),
            stdout.trim(),
            stderr.trim()
        );
    }

    fn update_loader_default(&self, entry: &str) -> Result<()> {
        if let Some(parent) = self.loader_conf_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating loader config parent '{}'", parent.display()))?;
        }

        let mut lines = if self.loader_conf_path.is_file() {
            fs::read_to_string(&self.loader_conf_path)
                .with_context(|| {
                    format!(
                        "reading loader config '{}'",
                        self.loader_conf_path.display()
                    )
                })?
                .lines()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let mut replaced = false;
        for line in &mut lines {
            let trimmed = line.trim_start();
            if trimmed.starts_with("default ") || trimmed == "default" {
                *line = format!("default {}", entry);
                replaced = true;
            }
        }
        if !replaced {
            lines.insert(0, format!("default {}", entry));
        }
        if !lines
            .iter()
            .any(|line| line.trim_start().starts_with("timeout "))
        {
            lines.push("timeout 5".to_string());
        }
        if !lines
            .iter()
            .any(|line| line.trim_start().starts_with("editor "))
        {
            lines.push("editor 0".to_string());
        }

        let mut serialized = lines.join("\n");
        if !serialized.ends_with('\n') {
            serialized.push('\n');
        }
        fs::write(&self.loader_conf_path, serialized).with_context(|| {
            format!(
                "writing loader config with updated default '{}'",
                self.loader_conf_path.display()
            )
        })?;
        Ok(())
    }

    fn ensure_entry_exists(&self, entry: &str) -> Result<()> {
        let path = self.entries_dir.join(entry);
        if !path.is_file() {
            return Err(tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "required systemd-boot entry missing: '{}'.\n\
Expected slot entries under '{}' (for example: <prefix>-a.conf / <prefix>-b.conf).",
                    path.display(),
                    self.entries_dir.display()
                ),
            ));
        }
        Ok(())
    }

    fn detect_slot_from_cmdline(&self) -> Option<Slot> {
        let cmdline = fs::read_to_string(&self.proc_cmdline_path).ok()?;
        slot_from_cmdline(&cmdline)
    }

    fn parse_loader_default_entry(&self) -> Result<Option<String>> {
        if !self.loader_conf_path.is_file() {
            return Ok(None);
        }
        let content = fs::read_to_string(&self.loader_conf_path)
            .with_context(|| format!("reading '{}'", self.loader_conf_path.display()))?;
        Ok(loader_default_entry_from_content(&content))
    }

    fn infer_prefix_from_entries_dir(&self) -> Result<Option<String>> {
        if !self.entries_dir.is_dir() {
            return Ok(None);
        }

        let mut slots_by_prefix: BTreeMap<String, BTreeSet<Slot>> = BTreeMap::new();
        for entry in fs::read_dir(&self.entries_dir)
            .with_context(|| format!("reading entries dir '{}'", self.entries_dir.display()))?
        {
            let entry = entry.with_context(|| {
                format!("iterating entries dir '{}'", self.entries_dir.display())
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some((prefix, slot)) = prefix_and_slot_from_entry_name(&name) {
                slots_by_prefix.entry(prefix).or_default().insert(slot);
            }
        }

        for (prefix, slots) in slots_by_prefix {
            if slots.contains(&Slot::A) && slots.contains(&Slot::B) {
                return Ok(Some(prefix));
            }
        }
        Ok(None)
    }
}

impl BootBackend for SystemdBootBackend {
    fn active_slot(&self) -> Result<Slot> {
        self.ensure_uefi_ready()?;
        if let Ok(forced) = std::env::var("RECAB_ACTIVE_SLOT") {
            return match forced.to_ascii_uppercase().as_str() {
                "A" => Ok(Slot::A),
                "B" => Ok(Slot::B),
                _ => Err(tagged_err(
                    ErrorClass::StateOrDetection,
                    "RECAB_ACTIVE_SLOT must be 'A' or 'B'",
                )),
            };
        }

        if let Some(slot) = self.detect_slot_from_cmdline() {
            return Ok(slot);
        }

        if let Some(default_slot) = self.default_slot()? {
            return Ok(default_slot);
        }

        Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "unable to determine active slot from '{}' or loader default '{}'.\n\
Set RECAB_ACTIVE_SLOT=A|B to override detection in constrained environments.",
                self.proc_cmdline_path.display(),
                self.loader_conf_path.display()
            ),
        ))
    }

    fn default_slot(&self) -> Result<Option<Slot>> {
        self.ensure_uefi_ready()?;
        let Some(entry) = self.parse_loader_default_entry()? else {
            return Ok(None);
        };
        Ok(slot_from_entry_name(&entry))
    }

    fn set_default_slot(&self, slot: Slot) -> Result<()> {
        self.ensure_uefi_ready()?;
        let entry = self.entry_filename_for(slot)?;
        self.ensure_entry_exists(&entry)?;
        self.update_loader_default(&entry)?;

        if self.bootctl_available() {
            self.run_bootctl(&["set-default", &entry])
                .context("synchronizing systemd-boot default entry")?;
        }
        Ok(())
    }

    fn set_trial_slot(&self, slot: Slot) -> Result<()> {
        self.ensure_uefi_ready()?;
        if !self.bootctl_available() {
            return Err(tagged_err(
                ErrorClass::BackendUnavailable,
                format!(
                    "systemd-boot backend requires '{}' in PATH for trial boot (set-next).\n\
Install systemd-boot userspace tools and retry, or ensure RECAB_BOOTCTL_BIN points to bootctl.",
                    self.bootctl_bin
                ),
            ));
        }
        let entry = self.entry_filename_for(slot)?;
        self.ensure_entry_exists(&entry)?;
        self.run_bootctl(&["set-oneshot", &entry])
            .context("configuring one-shot trial boot entry")?;
        Ok(())
    }

    fn entry_prefix(&self) -> Result<String> {
        self.ensure_uefi_ready()?;
        if let Ok(prefix) = std::env::var("RECAB_ENTRY_PREFIX") {
            if prefix.trim().is_empty() {
                return Err(tagged_err(
                    ErrorClass::StateOrDetection,
                    "RECAB_ENTRY_PREFIX is set but empty",
                ));
            }
            return Ok(prefix);
        }

        if let Some(entry) = self.parse_loader_default_entry()? {
            if let Some(prefix) = prefix_from_entry_name(&entry) {
                return Ok(prefix);
            }
        }

        if let Some(id) = os_release_id(&self.os_release_path)? {
            if entry_pair_exists(&self.entries_dir, &id) {
                return Ok(id);
            }
        }

        if let Some(prefix) = self.infer_prefix_from_entries_dir()? {
            return Ok(prefix);
        }

        Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "unable to determine A/B entry prefix in '{}'. Expected '<prefix>-a.conf' and '<prefix>-b.conf'.\n\
Set RECAB_ENTRY_PREFIX explicitly (for example: RECAB_ENTRY_PREFIX=iuppiter).",
                self.entries_dir.display()
            ),
        ))
    }
}

fn slot_from_cmdline(cmdline: &str) -> Option<Slot> {
    let lower = cmdline.to_ascii_lowercase();
    if lower.contains("system-a") {
        return Some(Slot::A);
    }
    if lower.contains("system-b") {
        return Some(Slot::B);
    }
    None
}

fn loader_default_entry_from_content(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("default ") {
            let value = rest.split_whitespace().next().unwrap_or("").trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        } else if trimmed == "default" {
            return None;
        }
    }
    None
}

fn slot_from_entry_name(entry: &str) -> Option<Slot> {
    let lower = entry.to_ascii_lowercase();
    if lower.ends_with("-a.conf") || lower.ends_with("-a") {
        return Some(Slot::A);
    }
    if lower.ends_with("-b.conf") || lower.ends_with("-b") {
        return Some(Slot::B);
    }
    None
}

fn prefix_from_entry_name(entry: &str) -> Option<String> {
    prefix_and_slot_from_entry_name(entry).map(|(prefix, _)| prefix)
}

fn prefix_and_slot_from_entry_name(entry: &str) -> Option<(String, Slot)> {
    let name = entry.trim();
    if name.is_empty() {
        return None;
    }

    let name = Path::new(name)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(name);
    let lower = name.to_ascii_lowercase();

    if let Some(prefix) = lower.strip_suffix("-a.conf") {
        return Some((prefix.to_string(), Slot::A));
    }
    if let Some(prefix) = lower.strip_suffix("-b.conf") {
        return Some((prefix.to_string(), Slot::B));
    }
    if let Some(prefix) = lower.strip_suffix("-a") {
        return Some((prefix.to_string(), Slot::A));
    }
    if let Some(prefix) = lower.strip_suffix("-b") {
        return Some((prefix.to_string(), Slot::B));
    }
    None
}

fn os_release_id(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading os-release '{}'", path.display()))?;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("ID=") {
            let value = rest.trim_matches('"').trim().to_ascii_lowercase();
            if !value.is_empty() {
                return Ok(Some(value));
            }
        }
    }
    Ok(None)
}

fn entry_pair_exists(entries_dir: &Path, prefix: &str) -> bool {
    entries_dir.join(format!("{prefix}-a.conf")).is_file()
        && entries_dir.join(format!("{prefix}-b.conf")).is_file()
}

fn validate_uefi_paths(sys_firmware_efi_path: &Path, efivars_path: &Path) -> Result<()> {
    if !sys_firmware_efi_path.is_dir() {
        return Err(tagged_err(
            ErrorClass::BackendUnavailable,
            format!(
                "systemd-boot backend requires UEFI runtime, but '{}' is missing.\n\
This host does not appear to be booted in UEFI mode.",
                sys_firmware_efi_path.display()
            ),
        ));
    }
    if !efivars_path.is_dir() {
        return Err(tagged_err(
            ErrorClass::BackendUnavailable,
            format!(
                "systemd-boot backend requires mounted efivarfs, but '{}' is missing.\n\
Mount efivarfs or boot under UEFI before running recab.",
                efivars_path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    struct MockBackend {
        active: Slot,
        default: Option<Slot>,
        prefix: String,
        set_default_calls: Vec<Slot>,
        set_trial_calls: Vec<Slot>,
    }

    impl MockBackend {
        fn new(active: Slot, default: Option<Slot>) -> Self {
            Self {
                active,
                default,
                prefix: "iuppiter".to_string(),
                set_default_calls: Vec::new(),
                set_trial_calls: Vec::new(),
            }
        }
    }

    impl BootBackend for std::sync::Mutex<MockBackend> {
        fn active_slot(&self) -> Result<Slot> {
            Ok(self.lock().expect("lock").active)
        }

        fn default_slot(&self) -> Result<Option<Slot>> {
            Ok(self.lock().expect("lock").default)
        }

        fn set_default_slot(&self, slot: Slot) -> Result<()> {
            let mut inner = self.lock().expect("lock");
            inner.default = Some(slot);
            inner.set_default_calls.push(slot);
            Ok(())
        }

        fn set_trial_slot(&self, slot: Slot) -> Result<()> {
            let mut inner = self.lock().expect("lock");
            inner.set_trial_calls.push(slot);
            Ok(())
        }

        fn entry_prefix(&self) -> Result<String> {
            Ok(self.lock().expect("lock").prefix.clone())
        }
    }

    struct TempStore {
        inner: JsonStateStore,
    }

    impl TempStore {
        fn new(root: &Path) -> Self {
            Self {
                inner: JsonStateStore {
                    state_path: root.join("state.json"),
                    audit_path: root.join("audit.log"),
                },
            }
        }
    }

    impl StateStore for TempStore {
        fn load(&self) -> Result<PersistentState> {
            self.inner.load()
        }

        fn save(&self, state: &PersistentState) -> Result<()> {
            self.inner.save(state)
        }

        fn append_audit(&self, event: &AuditEvent) -> Result<()> {
            self.inner.append_audit(event)
        }

        fn state_path(&self) -> &Path {
            self.inner.state_path()
        }
    }

    #[test]
    fn slot_other_roundtrip() {
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
    }

    #[test]
    fn parse_slot_from_cmdline_prefers_system_labels() {
        assert_eq!(
            slot_from_cmdline("quiet root=LABEL=system-a ro"),
            Some(Slot::A)
        );
        assert_eq!(slot_from_cmdline("root=LABEL=system-b ro"), Some(Slot::B));
        assert_eq!(slot_from_cmdline("root=/dev/sda2"), None);
    }

    #[test]
    fn parse_loader_default_entry() {
        let raw = "# comment\ntimeout 3\ndefault iuppiter-a.conf\neditor 0\n";
        assert_eq!(
            loader_default_entry_from_content(raw).as_deref(),
            Some("iuppiter-a.conf")
        );
    }

    #[test]
    fn prefix_and_slot_parsing() {
        assert_eq!(
            prefix_and_slot_from_entry_name("iuppiter-a.conf"),
            Some(("iuppiter".to_string(), Slot::A))
        );
        assert_eq!(
            prefix_and_slot_from_entry_name("/boot/loader/entries/levitate-b.conf"),
            Some(("levitate".to_string(), Slot::B))
        );
        assert_eq!(prefix_and_slot_from_entry_name("broken.conf"), None);
    }

    #[test]
    fn state_store_roundtrip_and_atomic_write() {
        let temp = tempdir().expect("tempdir");
        let store = TempStore::new(temp.path());
        let mut state = store.load().expect("load default");
        state.known_good_slot = Some(Slot::A);
        state.pending_trial_slot = Some(Slot::B);
        state.last_operation = Some("set-next-B".to_string());
        state.last_transition_unix = Some(123);
        store.save(&state).expect("save");

        let loaded = store.load().expect("load");
        assert_eq!(loaded.known_good_slot, Some(Slot::A));
        assert_eq!(loaded.pending_trial_slot, Some(Slot::B));
        assert_eq!(loaded.last_operation.as_deref(), Some("set-next-B"));
        assert_eq!(loaded.last_transition_unix, Some(123));
    }

    #[test]
    fn set_next_commit_rollback_flow() {
        let temp = tempdir().expect("tempdir");
        let store = TempStore::new(temp.path());
        let backend = std::sync::Mutex::new(MockBackend::new(Slot::A, Some(Slot::A)));

        let after_set_next = set_next_with(&backend, &store, Slot::B).expect("set-next");
        assert_eq!(after_set_next.active_slot, Slot::A);
        assert_eq!(after_set_next.next_boot, Some(Slot::B));
        assert!(after_set_next.trial_pending);

        {
            let mut inner = backend.lock().expect("lock");
            inner.active = Slot::B;
        }

        let after_commit = commit_with(&backend, &store).expect("commit");
        assert_eq!(after_commit.active_slot, Slot::B);
        assert_eq!(after_commit.known_good_slot, Some(Slot::B));
        assert!(!after_commit.trial_pending);

        {
            let mut inner = backend.lock().expect("lock");
            inner.active = Slot::A;
        }

        let after_rollback = rollback_with(&backend, &store).expect("rollback");
        assert_eq!(after_rollback.default_slot, Some(Slot::B));
        assert_eq!(after_rollback.known_good_slot, Some(Slot::B));
    }

    #[test]
    fn set_next_is_idempotent_when_same_trial_already_pending() {
        let temp = tempdir().expect("tempdir");
        let store = TempStore::new(temp.path());
        let backend = std::sync::Mutex::new(MockBackend::new(Slot::A, Some(Slot::A)));

        let first = set_next_with(&backend, &store, Slot::B).expect("first set-next");
        assert_eq!(first.next_boot, Some(Slot::B));

        let second = set_next_with(&backend, &store, Slot::B).expect("second set-next");
        assert_eq!(second.next_boot, Some(Slot::B));

        let inner = backend.lock().expect("lock");
        assert_eq!(inner.set_trial_calls, vec![Slot::B]);
    }

    #[test]
    fn set_next_requires_default_to_match_known_good_for_fallback() {
        let temp = tempdir().expect("tempdir");
        let store = TempStore::new(temp.path());
        let backend = std::sync::Mutex::new(MockBackend::new(Slot::A, Some(Slot::B)));

        let mut seeded = store.load().expect("load");
        seeded.known_good_slot = Some(Slot::A);
        store.save(&seeded).expect("save");

        let err = set_next_with(&backend, &store, Slot::B).expect_err("must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("loader default slot"));
    }

    #[test]
    fn validate_uefi_paths_fails_without_efi_runtime() {
        let temp = tempdir().expect("tempdir");
        let efivars = temp.path().join("efivars");
        fs::create_dir_all(&efivars).expect("efivars dir");

        let err = validate_uefi_paths(&temp.path().join("missing-efi"), &efivars)
            .expect_err("expected missing efi error");
        assert_eq!(stable_exit_code(&err), EXIT_CODE_BACKEND_UNAVAILABLE);
    }

    #[test]
    fn stable_exit_code_maps_tagged_errors() {
        let err = tagged_err(ErrorClass::Permission, "must run as root");
        assert_eq!(stable_exit_code(&err), EXIT_CODE_PERMISSION);

        let err = tagged_err(ErrorClass::PartialUpdate, "state restore failed");
        assert_eq!(stable_exit_code(&err), EXIT_CODE_PARTIAL_UPDATE);
    }
}
