//! The systemd-boot backend: which slot booted and which boots next (slot
//! entries, loader.conf and systemd-boot's EFI variables), changing that
//! selection with bootctl, and each slot's entry and kernel files on the ESP.

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{tagged_err, ErrorClass};
use crate::state::{BootChange, Selection};
use crate::store::{copy_durable, read_optional, sync_dir, write_durable};
use crate::Slot;

/// systemd-boot's vendor GUID, which names its EFI variables.
const LOADER_VENDOR_GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// Seconds after a kernel panic before the kernel reboots. A trial slot that
/// panics then reboots, and the firmware's next boot is the default slot.
pub(crate) const PANIC_REBOOT_SECS: u32 = 10;

const KERNEL_FILE: &str = "vmlinuz";
const INITRD_FILE: &str = "initramfs.img";

/// Where the backend reads and writes. Every path can be redirected with its
/// `RECAB_*` variable, which is how recab runs against a scratch tree.
pub(crate) struct SystemdBoot {
    boot_root: PathBuf,
    loader_conf: PathBuf,
    entries_dir: PathBuf,
    os_release: PathBuf,
    proc_cmdline: PathBuf,
    sys_firmware_efi: PathBuf,
    efivars: PathBuf,
    bootctl: String,
    /// The slot entries' name prefix, when not inferred.
    entry_prefix: Option<String>,
}

/// The boot configuration as one recab run sees it.
pub(crate) struct Boot {
    pub prefix: String,
    pub selection: Selection,
}

/// Why applying a boot change failed, and what it left behind.
pub(crate) enum ApplyError {
    /// Nothing changed (loader.conf was restored if it had been written).
    Untouched(anyhow::Error),
    /// A firmware variable changed before the failure; it cannot be undone.
    FirmwareChanged(anyhow::Error),
    /// loader.conf changed and could not be put back.
    RestoreFailed { cause: anyhow::Error, restore: anyhow::Error },
}

impl SystemdBoot {
    pub fn from_env() -> Result<Self> {
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
            loader_conf: boot_root.join("loader/loader.conf"),
            entries_dir: boot_root.join("loader/entries"),
            boot_root,
            os_release: path("RECAB_OS_RELEASE_PATH", "/etc/os-release"),
            proc_cmdline: path("RECAB_PROC_CMDLINE_PATH", "/proc/cmdline"),
            sys_firmware_efi: path("RECAB_SYS_FIRMWARE_EFI_PATH", "/sys/firmware/efi"),
            efivars: path("RECAB_EFIVARS_PATH", "/sys/firmware/efi/efivars"),
            bootctl: std::env::var("RECAB_BOOTCTL_BIN").unwrap_or_else(|_| "bootctl".to_string()),
            entry_prefix,
        })
    }

    pub fn read(&self) -> Result<Boot> {
        // systemd-boot prefers the firmware variable bootctl set-default
        // writes over loader.conf's `default`.
        let default_entry = match self.loader_efivar("LoaderEntryDefault") {
            Some(entry) => Some(entry),
            None => read_optional(&self.loader_conf)?
                .as_deref()
                .and_then(loader_default),
        };
        let prefix = self.entry_prefix(default_entry.as_deref())?;
        let active = self.active_slot(&prefix)?;
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
        let oneshot = slot_of(self.loader_efivar("LoaderEntryOneShot"));
        Ok(Boot {
            prefix,
            selection: Selection { active, default, oneshot },
        })
    }

    /// Changing the boot selection needs a UEFI system with efivarfs and
    /// bootctl, which writes the firmware variables systemd-boot obeys.
    pub fn ensure_can_change(&self) -> Result<()> {
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

    /// Write `change`: the one-shot variable, or the default (firmware
    /// variable and loader.conf) with any pending one-shot cleared.
    pub fn apply(&self, boot: &Boot, change: BootChange) -> Result<(), ApplyError> {
        match change {
            BootChange::None => Ok(()),
            BootChange::OneShot(slot) => {
                let entry = self.entry_for(boot, slot).map_err(ApplyError::Untouched)?;
                self.bootctl(&["set-oneshot", &entry]).map_err(ApplyError::Untouched)
            }
            BootChange::Default(slot) => {
                let entry = self.entry_for(boot, slot).map_err(ApplyError::Untouched)?;
                let before = read_optional(&self.loader_conf).map_err(ApplyError::Untouched)?;
                let content = loader_with_default(before.as_deref().unwrap_or(""), &entry);
                write_durable(&self.loader_conf, content.as_bytes())
                    .with_context(|| format!("writing '{}'", self.loader_conf.display()))
                    .map_err(ApplyError::Untouched)?;
                if let Err(cause) = self.bootctl(&["set-default", &entry]) {
                    let restore = match &before {
                        Some(content) => write_durable(&self.loader_conf, content.as_bytes()),
                        None => fs::remove_file(&self.loader_conf).map_err(Into::into),
                    };
                    return Err(match restore {
                        Ok(()) => ApplyError::Untouched(cause),
                        Err(restore) => ApplyError::RestoreFailed { cause, restore },
                    });
                }
                if self.loader_efivar("LoaderEntryOneShot").is_some() {
                    self.bootctl(&["set-oneshot", ""]).map_err(ApplyError::FirmwareChanged)?;
                }
                Ok(())
            }
        }
    }

    /// Refuse a trial of a slot whose entry names files the ESP lacks.
    pub fn ensure_bootable(&self, boot: &Boot, slot: Slot) -> Result<()> {
        let entry = self.entry_for(boot, slot)?;
        let content = self.entry_content(&entry)?;
        let missing = entry_boot_files(&content)
            .into_iter()
            .find(|file| !self.boot_root.join(file.trim_start_matches('/')).is_file());
        if let Some(missing) = missing {
            return Err(tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "slot {slot} is not bootable: its entry '{entry}' names '{missing}', which is not on the boot partition.\n\
Write a system into slot {slot} with 'recab stage' first."
                ),
            ));
        }
        Ok(())
    }

    /// The block device the slot's entry names with `root=`. The entries are
    /// the record of which partition is which slot; the running root is never
    /// returned for the other slot.
    pub fn slot_device(&self, boot: &Boot, slot: Slot) -> Result<PathBuf> {
        let entry = self.entry_for(boot, slot)?;
        let state_err = |message: String| tagged_err(ErrorClass::StateOrDetection, message);
        let selector = root_selector_from_entry_content(&self.entry_content(&entry)?)
            .ok_or_else(|| state_err(format!("entry '{entry}' has no root= option naming its partition")))?;
        let device = selector
            .device()
            .ok_or_else(|| state_err(format!("entry '{entry}' names root '{selector}', which is not present")))?;
        let cmdline = fs::read_to_string(&self.proc_cmdline).unwrap_or_default();
        if let Some(running) = root_selector_from_cmdline(&cmdline).and_then(|s| s.device()) {
            if running == device {
                return Err(state_err(format!(
                    "entry '{entry}' names the running root '{}'; refusing to overwrite it",
                    device.display()
                )));
            }
        }
        Ok(device)
    }

    /// Remove the slot's kernel directory, so its entry cannot boot until
    /// [`install_slot_boot_files`](Self::install_slot_boot_files) runs.
    pub fn remove_slot_boot_files(&self, boot: &Boot, slot: Slot) -> Result<()> {
        let dir = self.boot_root.join(slot_boot_dir(&boot.prefix, slot));
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("removing slot {slot}'s kernel '{}'", dir.display()))?;
            sync_dir(&self.boot_root)?;
        }
        Ok(())
    }

    /// Install the slot's kernel (and initrd) in its ESP directory and point
    /// its entry at them; the entry also gets `panic=` so a panicking trial
    /// reboots into the default slot.
    pub fn install_slot_boot_files(
        &self,
        boot: &Boot,
        slot: Slot,
        kernel: &Path,
        initrd: Option<&Path>,
    ) -> Result<()> {
        let entry = self.entry_for(boot, slot)?;
        let content = self.entry_content(&entry)?;
        let dir_name = slot_boot_dir(&boot.prefix, slot);
        let dir = self.boot_root.join(&dir_name);
        fs::create_dir_all(&dir).with_context(|| format!("creating '{}'", dir.display()))?;
        copy_durable(kernel, &dir.join(KERNEL_FILE))?;
        if let Some(initrd) = initrd {
            copy_durable(initrd, &dir.join(INITRD_FILE))?;
        }
        let content = entry_with_boot_files(
            &content,
            &format!("/{dir_name}/{KERNEL_FILE}"),
            initrd.map(|_| format!("/{dir_name}/{INITRD_FILE}")).as_deref(),
        );
        let path = self.entries_dir.join(&entry);
        write_durable(&path, content.as_bytes())
            .with_context(|| format!("writing '{}'", path.display()))
    }

    /// The slot's entry file name, which must exist.
    fn entry_for(&self, boot: &Boot, slot: Slot) -> Result<String> {
        let entry = entry_name(&boot.prefix, slot);
        let path = self.entries_dir.join(&entry);
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
        Ok(entry)
    }

    fn entry_content(&self, entry: &str) -> Result<String> {
        let path = self.entries_dir.join(entry);
        fs::read_to_string(&path).with_context(|| format!("reading entry '{}'", path.display()))
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

    /// The active slot: the slot entry whose root= names the running root.
    fn active_slot(&self, prefix: &str) -> Result<Slot> {
        let cmdline = fs::read_to_string(&self.proc_cmdline).map_err(|err| {
            tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "unable to determine active slot: cannot read '{}': {err}",
                    self.proc_cmdline.display()
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
                    self.proc_cmdline.display()
                ),
            ));
        };
        self.slot_from_entry_roots(prefix, &root_selector)?.ok_or_else(|| {
            tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "unable to map active root selector '{root_selector}' to A/B entries under '{}' (prefix '{prefix}').\n\
Expected matching root=... options in '{}' and '{}'.",
                    self.entries_dir.display(),
                    entry_name(prefix, Slot::A),
                    entry_name(prefix, Slot::B),
                ),
            )
        })
    }

    fn slot_from_entry_roots(&self, prefix: &str, target: &RootSelector) -> Result<Option<Slot>> {
        let mut matched: Option<Slot> = None;
        for slot in [Slot::A, Slot::B] {
            let entry = entry_name(prefix, slot);
            if !self.entries_dir.join(&entry).is_file() {
                continue;
            }
            let Some(entry_root) = root_selector_from_entry_content(&self.entry_content(&entry)?)
            else {
                continue;
            };
            if !entry_root.names_same_root(target) {
                continue;
            }
            match matched {
                Some(existing) => {
                    return Err(tagged_err(
                        ErrorClass::StateOrDetection,
                        format!(
                            "ambiguous root selector '{target}' matches both '{}' and '{entry}' under '{}'",
                            entry_name(prefix, existing),
                            self.entries_dir.display()
                        ),
                    ))
                }
                None => matched = Some(slot),
            }
        }
        Ok(matched)
    }

    /// The slot entries' prefix: configured, else the default entry's, else
    /// the os-release ID when both entries carry it, else the one prefix that
    /// has both entries.
    fn entry_prefix(&self, default_entry: Option<&str>) -> Result<String> {
        if let Some(prefix) = &self.entry_prefix {
            return Ok(prefix.clone());
        }
        if let Some((prefix, _)) = default_entry.and_then(parse_entry_name) {
            return Ok(prefix);
        }
        let has_pair = |prefix: &str| {
            [Slot::A, Slot::B]
                .iter()
                .all(|slot| self.entries_dir.join(entry_name(prefix, *slot)).is_file())
        };
        if let Some(id) = os_release_id(&self.os_release)? {
            if has_pair(&id) {
                return Ok(id);
            }
        }
        let mut slots_by_prefix: BTreeMap<String, BTreeSet<Slot>> = BTreeMap::new();
        if let Ok(entries) = fs::read_dir(&self.entries_dir) {
            for entry in entries.flatten() {
                if let Some((prefix, slot)) = parse_entry_name(&entry.file_name().to_string_lossy()) {
                    slots_by_prefix.entry(prefix).or_default().insert(slot);
                }
            }
        }
        let mut paired = slots_by_prefix.into_iter().filter(|(_, slots)| slots.len() == 2);
        match (paired.next(), paired.next()) {
            (Some((prefix, _)), None) => Ok(prefix),
            (Some((first, _)), Some((second, _))) => Err(tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "both '{first}' and '{second}' have A/B entries in '{}'; set RECAB_ENTRY_PREFIX to the one this system uses",
                    self.entries_dir.display()
                ),
            )),
            _ => Err(tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "unable to determine A/B entry prefix in '{}'. Expected '<prefix>-a.conf' and '<prefix>-b.conf'.\n\
Name the entries after the os-release ID, or set RECAB_ENTRY_PREFIX.",
                    self.entries_dir.display()
                ),
            )),
        }
    }
}

fn entry_name(prefix: &str, slot: Slot) -> String {
    format!("{prefix}-{}.conf", slot.entry_suffix())
}

/// The slot's kernel directory on the ESP, relative to its root.
fn slot_boot_dir(prefix: &str, slot: Slot) -> String {
    format!("{prefix}-{}", slot.entry_suffix())
}

/// `<prefix>-a.conf` → (prefix, A); bootctl reports names without `.conf`.
fn parse_entry_name(entry: &str) -> Option<(String, Slot)> {
    let name = Path::new(entry.trim()).file_name().and_then(OsStr::to_str)?;
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".conf").unwrap_or(&lower);
    [Slot::A, Slot::B].into_iter().find_map(|slot| {
        stem.strip_suffix(&format!("-{}", slot.entry_suffix()))
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
    let key = |line: &str| line.split_whitespace().next().map(str::to_string);
    let mut lines: Vec<String> = vec![format!("default {entry}")];
    lines.extend(
        content
            .lines()
            .filter(|line| key(line).as_deref() != Some("default"))
            .map(str::to_string),
    );
    for (name, line) in [("timeout", "timeout 5"), ("editor", "editor 0")] {
        if !lines.iter().any(|l| key(l).as_deref() == Some(name)) {
            lines.push(line.to_string());
        }
    }
    lines.join("\n") + "\n"
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

/// The entry with its `linux`/`efi`/`initrd` lines replaced, and `panic=`
/// added to its options when they have none.
fn entry_with_boot_files(content: &str, kernel: &str, initrd: Option<&str>) -> String {
    let mut boot_lines = vec![format!("linux {kernel}")];
    if let Some(initrd) = initrd {
        boot_lines.push(format!("initrd {initrd}"));
    }
    let mut lines = Vec::new();
    let mut placed = false;
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("linux" | "efi" | "initrd") => {
                if !placed {
                    lines.append(&mut boot_lines);
                    placed = true;
                }
            }
            Some("options") if !parts.any(|token| token.starts_with("panic=")) => {
                lines.push(format!("{} panic={PANIC_REBOOT_SECS}", line.trim_end()));
            }
            _ => lines.push(line.to_string()),
        }
    }
    if !placed {
        lines.append(&mut boot_lines);
    }
    lines.join("\n") + "\n"
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
        // UUIDs compare case-insensitively; labels and device paths do not.
        for (prefix, make) in [
            ("PARTUUID=", Self::PartUuid as fn(String) -> Self),
            ("/dev/disk/by-partuuid/", Self::PartUuid),
            ("UUID=", Self::Uuid),
            ("/dev/disk/by-uuid/", Self::Uuid),
        ] {
            if token.len() > prefix.len() && token[..prefix.len()].eq_ignore_ascii_case(prefix) {
                return Some(make(token[prefix.len()..].to_ascii_lowercase()));
            }
        }
        for prefix in ["LABEL=", "/dev/disk/by-label/"] {
            if let Some(value) = token.strip_prefix(prefix) {
                return Some(Self::Label(value.to_string()));
            }
        }
        Some(Self::DevicePath(token.to_string()))
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
        self == other || matches!((self.device(), other.device()), (Some(a), Some(b)) if a == b)
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

/// The kernel uses the last `root=` on its command line.
fn root_selector_from_cmdline(cmdline: &str) -> Option<RootSelector> {
    cmdline
        .split_whitespace()
        .filter_map(|token| token.strip_prefix("root="))
        .next_back()
        .and_then(RootSelector::parse)
}

fn root_selector_from_entry_content(content: &str) -> Option<RootSelector> {
    let options: Vec<&str> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix("options"))
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .collect();
    root_selector_from_cmdline(&options.join(" "))
}
