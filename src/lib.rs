//! recab - A/B slot management primitives.
//!
//! recab is intentionally small:
//! - detect active/inactive slot
//! - write a prebuilt system into the inactive slot (`stage`)
//! - stage a trial boot for inactive slot
//! - commit active slot as known-good
//! - rollback next boot to known-good slot
//!
//! Backend: systemd-boot entries + local persistent state.

mod stage;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_SCHEMA_VERSION: u32 = 1;

/// systemd-boot's vendor GUID, which names its EFI variables.
const LOADER_VENDOR_GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

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

/// An error with a stable class. The class travels as a typed value in the
/// error chain, so the exit status never depends on message wording.
#[derive(Debug)]
struct ClassifiedError {
    class: ErrorClass,
    message: String,
}

impl std::fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.class.tag(), self.message)
    }
}

impl std::error::Error for ClassifiedError {}

fn tagged_err(class: ErrorClass, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClassifiedError {
        class,
        message: message.into(),
    })
}

/// Stable exit-code mapping for recab CLI and callers: the outermost classified
/// error decides, so a partial update caused by a backend failure reports the
/// partial update.
pub fn stable_exit_code(err: &anyhow::Error) -> u8 {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<ClassifiedError>())
        .map(|classified| classified.class.code())
        .unwrap_or(EXIT_CODE_GENERIC)
}

/// System slot identifier (A/B).
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum Slot {
    #[value(name = "A")]
    A,
    #[value(name = "B")]
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

/// Current A/B view exported by `recab status --json`.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub active_slot: Slot,
    pub inactive_slot: Slot,
    /// A one-shot trial boot is set in the firmware and not yet taken.
    pub trial_pending: bool,
    /// The slot the next boot starts: the trial slot, else the default.
    pub next_boot: Option<Slot>,
    pub default_slot: Option<Slot>,
    pub known_good_slot: Option<Slot>,
    /// The running slot is the known-good slot. After a trial boot this is
    /// false until `recab commit`; an uncommitted slot is left on next boot.
    pub committed: bool,
    /// The slot `recab rollback` would select, if any.
    pub rollback_slot: Option<Slot>,
    pub entry_prefix: String,
}

/// Where recab reads and writes. Every path can be redirected with its
/// `RECAB_*` variable, which is how recab runs against a scratch tree.
struct Config {
    boot_root: PathBuf,
    loader_conf: PathBuf,
    entries_dir: PathBuf,
    os_release: PathBuf,
    proc_cmdline: PathBuf,
    sys_firmware_efi: PathBuf,
    efivars: PathBuf,
    bootctl: String,
    state_path: PathBuf,
    audit_path: PathBuf,
    /// The slot entries' name prefix, when not inferred.
    entry_prefix: Option<String>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let path = |var: &str, default: &str| {
            PathBuf::from(std::env::var(var).unwrap_or_else(|_| default.to_string()))
        };
        let boot_root = path("RECAB_BOOT_ROOT", "/boot");
        let entry_prefix = match std::env::var("RECAB_ENTRY_PREFIX") {
            Ok(prefix) if prefix.trim().is_empty() => {
                return Err(tagged_err(
                    ErrorClass::StateOrDetection,
                    "RECAB_ENTRY_PREFIX is set but empty",
                ))
            }
            Ok(prefix) => Some(prefix),
            Err(_) => None,
        };
        Ok(Self {
            boot_root: boot_root.clone(),
            loader_conf: boot_root.join("loader/loader.conf"),
            entries_dir: boot_root.join("loader/entries"),
            os_release: path("RECAB_OS_RELEASE_PATH", "/etc/os-release"),
            proc_cmdline: path("RECAB_PROC_CMDLINE_PATH", "/proc/cmdline"),
            sys_firmware_efi: path("RECAB_SYS_FIRMWARE_EFI_PATH", "/sys/firmware/efi"),
            efivars: path("RECAB_EFIVARS_PATH", "/sys/firmware/efi/efivars"),
            bootctl: std::env::var("RECAB_BOOTCTL_BIN").unwrap_or_else(|_| "bootctl".to_string()),
            state_path: path("RECAB_STATE_PATH", "/var/lib/recab/state.json"),
            audit_path: path("RECAB_AUDIT_PATH", "/var/lib/recab/audit.log"),
            entry_prefix,
        })
    }

    /// Changing the boot selection needs a UEFI system with efivarfs and
    /// bootctl, which writes the firmware variables systemd-boot obeys.
    fn ensure_can_change_boot(&self) -> Result<()> {
        for (dir, missing) in [
            (&self.sys_firmware_efi, "this host does not appear to be booted in UEFI mode"),
            (&self.efivars, "mount efivarfs or boot under UEFI"),
        ] {
            if !dir.is_dir() {
                return Err(tagged_err(
                    ErrorClass::BackendUnavailable,
                    format!(
                        "changing the boot selection needs '{}', which is missing: {missing}",
                        dir.display()
                    ),
                ));
            }
        }
        let available = Command::new(&self.bootctl)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !available {
            return Err(tagged_err(
                ErrorClass::BackendUnavailable,
                format!(
                    "changing the boot selection needs '{}' (systemd-boot's bootctl); install it or point RECAB_BOOTCTL_BIN at it",
                    self.bootctl
                ),
            ));
        }
        Ok(())
    }

    fn bootctl(&self, args: &[&str]) -> Result<()> {
        let output = Command::new(&self.bootctl)
            .args(args)
            .output()
            .with_context(|| format!("running '{}' with args {:?}", self.bootctl, args))?;
        if output.status.success() {
            return Ok(());
        }
        bail!(
            "{} {:?} failed (exit {}): {}\n{}",
            self.bootctl,
            args,
            output.status.code().unwrap_or(1),
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    /// A systemd-boot EFI variable holding an entry name, if set.
    fn loader_efivar(&self, name: &str) -> Option<String> {
        let bytes = fs::read(self.efivars.join(format!("{name}-{LOADER_VENDOR_GUID}"))).ok()?;
        // 4 bytes of attributes, then a NUL-terminated UTF-16LE string.
        let units: Vec<u16> = bytes
            .get(4..)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|unit| *unit != 0)
            .collect();
        String::from_utf16(&units).ok().filter(|value| !value.is_empty())
    }
}

/// The boot configuration as one recab run sees it.
struct Boot {
    prefix: String,
    active: Slot,
    /// The slot systemd-boot boots by default.
    default: Option<Slot>,
    /// The slot of a one-shot entry systemd-boot has not taken yet.
    oneshot: Option<Slot>,
}

impl Boot {
    fn read(config: &Config) -> Result<Self> {
        // systemd-boot prefers the firmware variable bootctl set-default
        // writes over loader.conf's `default`.
        let default_entry = match config.loader_efivar("LoaderEntryDefault") {
            Some(entry) => Some(entry),
            None => read_optional(&config.loader_conf)?
                .as_deref()
                .and_then(loader_default),
        };
        let prefix = entry_prefix(config, default_entry.as_deref())?;
        let active = active_slot(config, &prefix)?;
        let slot_of = |entry: Option<String>| {
            entry
                .as_deref()
                .and_then(parse_entry_name)
                .filter(|(entry_prefix, _)| *entry_prefix == prefix)
                .map(|(_, slot)| slot)
        };
        let default = slot_of(default_entry);
        // systemd-boot removes the variable when it boots the entry, so it is
        // the record of whether a trial is still to come.
        let oneshot = slot_of(config.loader_efivar("LoaderEntryOneShot"));
        Ok(Self {
            prefix,
            active,
            default,
            oneshot,
        })
    }

    fn entry(&self, slot: Slot) -> String {
        entry_name(&self.prefix, slot)
    }
}

/// The slot to fall back to: the one committed as good, or else the default.
fn known_good(state: &PersistentState, boot: &Boot) -> Option<Slot> {
    state.known_good_slot.or(boot.default)
}

fn committed(state: &PersistentState, boot: &Boot) -> bool {
    known_good(state, boot) == Some(boot.active)
}

/// What rollback selects: from an uncommitted slot, or to cancel a pending
/// trial, the known-good slot; from a committed slot, the slot that was good
/// before it.
fn rollback_target(state: &PersistentState, boot: &Boot) -> Option<Slot> {
    if !committed(state, boot) || boot.oneshot.is_some() {
        known_good(state, boot)
    } else {
        state.previous_good_slot
    }
}

/// Query current A/B state from the boot configuration and recab's state.
/// Only reads, so it works anywhere the files are readable.
pub fn status() -> Result<Status> {
    let config = Config::from_env()?;
    let boot = Boot::read(&config)?;
    let state = load_state(&config)?;
    Ok(Status {
        active_slot: boot.active,
        inactive_slot: boot.active.other(),
        trial_pending: boot.oneshot.is_some(),
        next_boot: boot.oneshot.or(boot.default),
        default_slot: boot.default,
        known_good_slot: known_good(&state, &boot),
        committed: committed(&state, &boot),
        rollback_slot: rollback_target(&state, &boot),
        entry_prefix: boot.prefix,
    })
}

/// Write a prebuilt system into the inactive slot: format it, extract the
/// rootfs image, carry the machine's configuration over, and install the
/// slot's kernel and boot entry. The slot is not booted until `set_next`.
pub fn stage(request: &StageRequest) -> Result<()> {
    let config = Config::from_env()?;
    let boot = Boot::read(&config)?;
    let previous = load_state(&config)?;
    let slot = boot.active.other();

    if !committed(&previous, &boot) {
        return Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "slot {} is running uncommitted, so slot {slot} is the fallback; staging over it would leave no known-good system.\n\
Run 'recab commit' to keep slot {}, or reboot into slot {slot}.",
                boot.active, boot.active
            ),
        ));
    }
    if boot.oneshot == Some(slot) {
        return Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!("a trial boot of slot {slot} is pending; run 'recab rollback' to cancel it before staging slot {slot} again"),
        ));
    }
    require_root("stage")?;

    stage::write_slot(&config, &boot, slot, request)?;

    let mut next = previous.clone();
    next.known_good_slot = Some(boot.active);
    if next.pending_trial_slot == Some(slot) {
        next.pending_trial_slot = None;
    }
    if next.previous_good_slot == Some(slot) {
        next.previous_good_slot = None;
    }
    record(&config, &boot, Operation::Stage(slot), previous, next)
}

/// The prebuilt system `stage` writes.
#[derive(Debug, Clone)]
pub struct StageRequest {
    /// EROFS rootfs image, extracted with recstrap.
    pub rootfs: PathBuf,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
}

/// Configure a one-shot trial boot of the inactive `slot`.
pub fn set_next(slot: Slot) -> Result<()> {
    change(Request::SetNext(slot))
}

/// Mark the running slot as known-good and make it the default.
pub fn commit() -> Result<()> {
    change(Request::Commit)
}

/// Configure the next boot back to the known-good slot.
pub fn rollback() -> Result<()> {
    change(Request::Rollback)
}

enum Request {
    SetNext(Slot),
    Commit,
    Rollback,
}

/// A state transition; its names are used in the state file and audit log.
#[derive(Debug, Clone, Copy)]
enum Operation {
    Stage(Slot),
    SetNext(Slot),
    Commit(Slot),
    Rollback(Slot),
}

impl Operation {
    /// The slot the operation selects.
    fn slot(self) -> Slot {
        match self {
            Operation::Stage(slot)
            | Operation::SetNext(slot)
            | Operation::Commit(slot)
            | Operation::Rollback(slot) => slot,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Operation::Stage(_) => "stage",
            Operation::SetNext(_) => "set-next",
            Operation::Commit(_) => "commit",
            Operation::Rollback(_) => "rollback",
        }
    }

    /// `last_operation` in the state file.
    fn label(self) -> String {
        match self {
            Operation::Stage(slot) => format!("stage-{slot}"),
            Operation::SetNext(slot) => format!("set-next-{slot}"),
            Operation::Commit(slot) => format!("commit-{slot}"),
            Operation::Rollback(slot) => format!("rollback-to-{slot}"),
        }
    }
}

fn require_root(operation: &str) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(tagged_err(
            ErrorClass::Permission,
            format!("{operation} must run as root"),
        ));
    }
    Ok(())
}

fn change(request: Request) -> Result<()> {
    let config = Config::from_env()?;
    let boot = Boot::read(&config)?;
    let previous = load_state(&config)?;

    let (operation, mut next) = match request {
        Request::SetNext(slot) if slot == boot.active => {
            return Err(tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "slot {slot} is the running slot; set-next stages the inactive slot ({}). To cancel a pending trial, run 'recab rollback'.",
                    slot.other()
                ),
            ));
        }
        Request::SetNext(slot) => {
            let entry = boot.entry(slot);
            ensure_entry_exists(&config, &entry)?;
            if let Some(missing) = missing_boot_file(&config, &entry)? {
                return Err(tagged_err(
                    ErrorClass::StateOrDetection,
                    format!(
                        "slot {slot} is not bootable: its entry '{entry}' names '{missing}', which is not on the boot partition.\n\
Write a system into slot {slot} with 'recab stage' first."
                    ),
                ));
            }
            let fallback = known_good(&previous, &boot).unwrap_or(boot.active);
            if boot.default != Some(fallback) {
                return Err(tagged_err(
                    ErrorClass::StateOrDetection,
                    format!(
                        "cannot stage trial boot: default slot ({}) is not the known-good slot ({fallback}), so a failed trial would not fall back to it.\n\
Run 'recab commit' to set a stable fallback default before 'recab set-next {slot}'.",
                        boot.default.map_or("none".to_string(), |s| s.to_string())
                    ),
                ));
            }
            let mut next = previous.clone();
            next.known_good_slot = Some(fallback);
            next.pending_trial_slot = Some(slot);
            (Operation::SetNext(slot), next)
        }
        Request::Commit => {
            let mut next = previous.clone();
            let good = known_good(&previous, &boot);
            if good != Some(boot.active) {
                // The slot that was good until now is what rollback returns to.
                next.previous_good_slot = good;
            }
            next.known_good_slot = Some(boot.active);
            next.pending_trial_slot = None;
            (Operation::Commit(boot.active), next)
        }
        Request::Rollback => {
            let target = rollback_target(&previous, &boot).ok_or_else(|| {
                let reason = if committed(&previous, &boot) {
                    format!(
                        "slot {} is committed and no other slot has been known-good since it was written",
                        boot.active
                    )
                } else {
                    format!(
                        "no known-good slot in state '{}' and no default slot",
                        config.state_path.display()
                    )
                };
                tagged_err(ErrorClass::StateOrDetection, format!("cannot rollback: {reason}"))
            })?;
            let mut next = previous.clone();
            if target != boot.active && committed(&previous, &boot) {
                // Leaving a committed slot: it stays a slot to roll back to.
                next.previous_good_slot = Some(boot.active);
            }
            next.known_good_slot = Some(target);
            next.pending_trial_slot = None;
            (Operation::Rollback(target), next)
        }
    };

    require_root(operation.name())?;
    config.ensure_can_change_boot()?;

    // What the transition writes: the default entry (loader.conf and the
    // firmware variable) or the one-shot entry. Any pending one-shot is
    // cleared when the default changes, so the next boot is the default.
    let entry = boot.entry(operation.slot());
    let (new_default, mut commands) = match operation {
        Operation::SetNext(_) => (None, vec![vec!["set-oneshot".to_string(), entry]]),
        Operation::Commit(_) | Operation::Rollback(_) => (
            Some(entry.clone()),
            vec![vec!["set-default".to_string(), entry]],
        ),
        Operation::Stage(_) => unreachable!("stage changes no boot selection"),
    };
    if new_default.is_some() && config.loader_efivar("LoaderEntryOneShot").is_some() {
        commands.push(vec!["set-oneshot".to_string(), String::new()]);
    }
    let target_entry = boot.entry(operation.slot());
    ensure_entry_exists(&config, &target_entry)?;

    if let Operation::SetNext(slot) = operation {
        if previous.pending_trial_slot == Some(slot) {
            // Already staged: make sure the one-shot entry is still set.
            return config.bootctl(&["set-oneshot", &target_entry]);
        }
    }

    next.last_operation = Some(operation.label());
    next.last_transition_unix = Some(now_unix_secs()?);
    apply_change(&config, operation, &previous, &next, new_default.as_deref(), &commands)?;
    audit(&config, &boot, operation, previous, next);
    Ok(())
}

/// Save the state after an operation that changed no boot selection.
fn record(
    config: &Config,
    boot: &Boot,
    operation: Operation,
    previous: PersistentState,
    mut next: PersistentState,
) -> Result<()> {
    next.last_operation = Some(operation.label());
    next.last_transition_unix = Some(now_unix_secs()?);
    save_state(config, &next, operation.name())?;
    audit(config, boot, operation, previous, next);
    Ok(())
}

fn audit(
    config: &Config,
    boot: &Boot,
    operation: Operation,
    before: PersistentState,
    after: PersistentState,
) {
    let event = AuditEvent {
        schema: STATE_SCHEMA_VERSION,
        timestamp_unix: after.last_transition_unix.unwrap_or_default(),
        operation: operation.name(),
        active_slot: boot.active,
        before,
        after,
    };
    if let Err(err) = append_audit(config, &event) {
        eprintln!(
            "recab: warning: {} succeeded but the audit log '{}' was not written: {err:#}",
            operation.name(),
            config.audit_path.display()
        );
    }
}

/// Write the new state and loader.conf, then run bootctl. A failure before
/// bootctl changed anything restores both files; after a firmware variable
/// changed, recab cannot undo it and reports a partial update.
fn apply_change(
    config: &Config,
    operation: Operation,
    previous: &PersistentState,
    next: &PersistentState,
    new_default: Option<&str>,
    commands: &[Vec<String>],
) -> Result<()> {
    let name = operation.name();
    let loader_before = read_optional(&config.loader_conf)?;
    save_state(config, next, name)?;

    let mut firmware_changed = false;
    let result = (|| -> Result<()> {
        if let Some(entry) = new_default {
            let content = loader_with_default(loader_before.as_deref().unwrap_or(""), entry);
            write_atomic(&config.loader_conf, content.as_bytes())
                .with_context(|| format!("writing '{}'", config.loader_conf.display()))?;
        }
        for args in commands {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            config.bootctl(&args)?;
            firmware_changed = true;
        }
        Ok(())
    })();
    let Err(cause) = result else {
        return Ok(());
    };

    if firmware_changed {
        return Err(tagged_err(
            ErrorClass::PartialUpdate,
            format!(
                "{name} changed systemd-boot's firmware variables and then failed; the state file and loader.conf keep the new selection.\nerror: {cause:#}"
            ),
        ));
    }
    let restored = save_state(config, previous, &format!("{name}-revert")).and_then(|()| {
        match &loader_before {
            Some(content) => write_atomic(&config.loader_conf, content.as_bytes()),
            None if config.loader_conf.exists() => Ok(fs::remove_file(&config.loader_conf)?),
            None => Ok(()),
        }
    });
    match restored {
        Ok(()) => Err(cause.context(format!(
            "{name} aborted; restored the previous state and loader.conf"
        ))),
        Err(revert_err) => Err(tagged_err(
            ErrorClass::PartialUpdate,
            format!(
                "{name} failed and restoring the previous state and loader.conf also failed.\n\
primary error: {cause:#}\n\
restore error: {revert_err:#}"
            ),
        )),
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(anyhow!(err).context(format!("reading '{}'", path.display()))),
    }
}

fn ensure_entry_exists(config: &Config, entry: &str) -> Result<()> {
    let path = config.entries_dir.join(entry);
    if !path.is_file() {
        return Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "required systemd-boot entry missing: '{}'.\n\
Expected slot entries under '{}' (for example: <prefix>-a.conf / <prefix>-b.conf).",
                path.display(),
                config.entries_dir.display()
            ),
        ));
    }
    Ok(())
}

/// The first file an entry boots (`linux` or `efi`) that is missing from the
/// boot partition, if any.
fn missing_boot_file(config: &Config, entry: &str) -> Result<Option<String>> {
    let path = config.entries_dir.join(entry);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("reading entry '{}'", path.display()))?;
    Ok(entry_boot_files(&content)
        .into_iter()
        .find(|file| !config.boot_root.join(file.trim_start_matches('/')).is_file()))
}

/// The files a systemd-boot entry loads from the boot partition.
fn entry_boot_files(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("linux" | "efi" | "initrd") => parts.next().map(str::to_string),
                _ => None,
            }
        })
        .collect()
}

fn now_unix_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before UNIX_EPOCH")?
        .as_secs())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistentState {
    schema: u32,
    known_good_slot: Option<Slot>,
    /// The slot that was known-good before `known_good_slot`, while it still
    /// holds that system: what rollback from a committed slot returns to.
    #[serde(default)]
    previous_good_slot: Option<Slot>,
    pending_trial_slot: Option<Slot>,
    last_operation: Option<String>,
    last_transition_unix: Option<u64>,
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA_VERSION,
            known_good_slot: None,
            previous_good_slot: None,
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
    operation: &'static str,
    active_slot: Slot,
    before: PersistentState,
    after: PersistentState,
}

fn load_state(config: &Config) -> Result<PersistentState> {
    let path = &config.state_path;
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

fn save_state(config: &Config, state: &PersistentState, operation: &str) -> Result<()> {
    let payload = serde_json::to_vec_pretty(state).context("serializing recab state")?;
    write_atomic(&config.state_path, &payload).map_err(|err| {
        tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "failed writing recab state during {operation} to '{}': {err:#}",
                config.state_path.display()
            ),
        )
    })
}

fn append_audit(config: &Config, event: &AuditEvent) -> Result<()> {
    let path = &config.audit_path;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())?;
    Ok(())
}

/// Replace `path` in one step: write a sibling temp file, then rename it.
fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("'{}' has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating parent directory '{}'", parent.display()))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, content).with_context(|| format!("writing temp file '{}'", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming '{}' to '{}'", tmp.display(), path.display()))?;
    Ok(())
}

/// loader.conf's default entry: systemd-boot uses the last `default` line
/// that names one.
fn loader_default(content: &str) -> Option<String> {
    content
        .lines()
        .filter_map(|line| line.trim().strip_prefix("default"))
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .filter_map(|rest| rest.split_whitespace().next())
        .next_back()
        .map(str::to_string)
}

/// loader.conf with `entry` as its only `default`. A file recab creates also
/// gets a menu timeout and the command-line editor disabled.
fn loader_with_default(content: &str, entry: &str) -> String {
    let mut lines: Vec<String> = vec![format!("default {entry}")];
    lines.extend(
        content
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                !(trimmed == "default" || trimmed.starts_with("default ") || trimmed.starts_with("default\t"))
            })
            .map(str::to_string),
    );
    for (key, line) in [("timeout", "timeout 5"), ("editor", "editor 0")] {
        if !lines.iter().any(|l| l.trim_start().starts_with(key)) {
            lines.push(line.to_string());
        }
    }
    lines.join("\n") + "\n"
}

/// The active slot: the slot entry whose root= names the running root.
fn active_slot(config: &Config, prefix: &str) -> Result<Slot> {
    let cmdline = fs::read_to_string(&config.proc_cmdline).map_err(|err| {
        tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "unable to determine active slot: cannot read '{}': {err}",
                config.proc_cmdline.display()
            ),
        )
    })?;
    // The slot entries' root= selectors, written at install time, are the
    // record of which partition is which slot. The loader default is what
    // boots next, not what booted, so it is never used as the answer.
    let Some(root_selector) = root_selector_from_cmdline(&cmdline) else {
        return Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "unable to determine active slot: '{}' has no root=",
                config.proc_cmdline.display()
            ),
        ));
    };
    slot_from_entry_roots(&config.entries_dir, prefix, &root_selector)?.ok_or_else(|| {
        tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "unable to map active root selector '{root_selector}' to A/B entries under '{}' (prefix '{prefix}').\n\
Expected matching root=... options in '{}' and '{}'.",
                config.entries_dir.display(),
                entry_name(prefix, Slot::A),
                entry_name(prefix, Slot::B),
            ),
        )
    })
}

/// The slot entries' prefix: configured, else the default entry's, else the
/// os-release ID when both entries carry it, else the one prefix that has
/// both entries.
fn entry_prefix(config: &Config, default_entry: Option<&str>) -> Result<String> {
    if let Some(prefix) = &config.entry_prefix {
        return Ok(prefix.clone());
    }
    if let Some((prefix, _)) = default_entry.and_then(parse_entry_name) {
        return Ok(prefix);
    }
    let has_pair =
        |prefix: &str| [Slot::A, Slot::B].iter().all(|slot| config.entries_dir.join(entry_name(prefix, *slot)).is_file());
    if let Some(id) = os_release_id(&config.os_release)? {
        if has_pair(&id) {
            return Ok(id);
        }
    }
    let mut slots_by_prefix: BTreeMap<String, BTreeSet<Slot>> = BTreeMap::new();
    if let Ok(entries) = fs::read_dir(&config.entries_dir) {
        for entry in entries.flatten() {
            if let Some((prefix, slot)) = parse_entry_name(&entry.file_name().to_string_lossy()) {
                slots_by_prefix.entry(prefix).or_default().insert(slot);
            }
        }
    }
    if let Some((prefix, _)) = slots_by_prefix.into_iter().find(|(_, slots)| slots.len() == 2) {
        return Ok(prefix);
    }
    Err(tagged_err(
        ErrorClass::StateOrDetection,
        format!(
            "unable to determine A/B entry prefix in '{}'. Expected '<prefix>-a.conf' and '<prefix>-b.conf'.\n\
Name the entries after the os-release ID, or set RECAB_ENTRY_PREFIX.",
            config.entries_dir.display()
        ),
    ))
}

fn entry_name(prefix: &str, slot: Slot) -> String {
    format!("{prefix}-{}.conf", slot.as_entry_suffix())
}

/// `<prefix>-a.conf` → (prefix, A); bootctl reports names without `.conf`.
fn parse_entry_name(entry: &str) -> Option<(String, Slot)> {
    let name = Path::new(entry.trim()).file_name().and_then(OsStr::to_str)?;
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".conf").unwrap_or(&lower);
    [Slot::A, Slot::B].into_iter().find_map(|slot| {
        stem.strip_suffix(&format!("-{}", slot.as_entry_suffix()))
            .filter(|prefix| !prefix.is_empty())
            .map(|prefix| (prefix.to_string(), slot))
    })
}

fn os_release_id(path: &Path) -> Result<Option<String>> {
    let Some(content) = read_optional(path)? else {
        return Ok(None);
    };
    Ok(content
        .lines()
        .filter_map(|line| line.trim().strip_prefix("ID="))
        .map(|value| value.trim_matches('"').trim().to_ascii_lowercase())
        .find(|value| !value.is_empty()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RootSelector {
    PartUuid(String),
    Uuid(String),
    Label(String),
    DevicePath(String),
}

impl RootSelector {
    fn parse(raw: &str) -> Option<Self> {
        let token = raw.trim().trim_matches('"').trim_matches('\'').trim();
        if token.is_empty() {
            return None;
        }

        let lower = token.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("partuuid=") {
            return Some(Self::PartUuid(value.to_string()));
        }
        if let Some(value) = lower.strip_prefix("/dev/disk/by-partuuid/") {
            return Some(Self::PartUuid(value.to_string()));
        }
        if let Some(value) = lower.strip_prefix("uuid=") {
            return Some(Self::Uuid(value.to_string()));
        }
        if let Some(value) = lower.strip_prefix("/dev/disk/by-uuid/") {
            return Some(Self::Uuid(value.to_string()));
        }
        if let Some(value) = lower.strip_prefix("label=") {
            return Some(Self::Label(value.to_string()));
        }
        if let Some(value) = lower.strip_prefix("/dev/disk/by-label/") {
            return Some(Self::Label(value.to_string()));
        }
        Some(Self::DevicePath(lower))
    }

    /// The block device this selector names on the running system, through
    /// the kernel's /dev/disk links.
    fn device(&self) -> Option<PathBuf> {
        let link = match self {
            Self::PartUuid(v) => PathBuf::from(format!("/dev/disk/by-partuuid/{v}")),
            Self::Uuid(v) => PathBuf::from(format!("/dev/disk/by-uuid/{v}")),
            Self::Label(v) => PathBuf::from(format!("/dev/disk/by-label/{v}")),
            Self::DevicePath(v) => PathBuf::from(v),
        };
        fs::canonicalize(link).ok()
    }

    /// Same selector, or two selectors naming the same device (a cmdline
    /// UUID= and an entry PARTUUID= for one partition).
    fn names_same_root(&self, other: &Self) -> bool {
        self == other
            || matches!((self.device(), other.device()), (Some(a), Some(b)) if a == b)
    }
}

impl std::fmt::Display for RootSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RootSelector::PartUuid(value) => write!(f, "PARTUUID={value}"),
            RootSelector::Uuid(value) => write!(f, "UUID={value}"),
            RootSelector::Label(value) => write!(f, "LABEL={value}"),
            RootSelector::DevicePath(value) => f.write_str(value),
        }
    }
}

fn root_selector_from_cmdline(cmdline: &str) -> Option<RootSelector> {
    for token in cmdline.split_whitespace() {
        if let Some(value) = token.strip_prefix("root=") {
            return RootSelector::parse(value);
        }
    }
    None
}

fn root_selector_from_entry_content(content: &str) -> Option<RootSelector> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        if parts.next() != Some("options") {
            continue;
        }

        for token in parts {
            if let Some(value) = token.strip_prefix("root=") {
                return RootSelector::parse(value);
            }
        }
    }
    None
}

fn slot_from_entry_roots(
    entries_dir: &Path,
    prefix: &str,
    target: &RootSelector,
) -> Result<Option<Slot>> {
    let mut matched: Option<Slot> = None;
    for slot in [Slot::A, Slot::B] {
        let entry_path = entries_dir.join(entry_name(prefix, slot));
        if !entry_path.is_file() {
            continue;
        }
        let content = fs::read_to_string(&entry_path)
            .with_context(|| format!("reading entry '{}'", entry_path.display()))?;
        let Some(entry_root) = root_selector_from_entry_content(&content) else {
            continue;
        };
        if !entry_root.names_same_root(target) {
            continue;
        }

        if let Some(existing) = matched {
            if existing != slot {
                return Err(tagged_err(
                    ErrorClass::StateOrDetection,
                    format!(
                        "ambiguous root selector '{target}' matches both '{}' and '{}' under '{}'",
                        entry_name(prefix, existing),
                        entry_path
                            .file_name()
                            .and_then(OsStr::to_str)
                            .unwrap_or("<unknown>"),
                        entries_dir.display()
                    ),
                ));
            }
        } else {
            matched = Some(slot);
        }
    }
    Ok(matched)
}
