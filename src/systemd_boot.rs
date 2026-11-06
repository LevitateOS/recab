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

/// Seconds after a kernel panic before the kernel reboots, on every slot
/// entry recab writes: a trial that panics reboots into the default slot.
pub(crate) const PANIC_OPTION: &str = "panic=10";

/// Kernel options a slot's entry carries while it is scheduled as a trial
/// and before it is committed: the initramfs reboots instead of opening an
/// emergency shell, and a crashing PID 1 reboots instead of freezing. They
/// leave the entry when it becomes the default, so a committed slot that
/// fails gets a shell to repair it rather than a reboot loop.
pub(crate) const TRIAL_OPTIONS: &[&str] = &["rd.emergency=reboot", "systemd.crash_action=reboot"];

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
    /// The backend for a system whose ESP is mounted at `boot_root` (an
    /// install target); everything else is the running system's.
    pub fn at_boot_root(boot_root: &Path) -> Result<Self> {
        let mut backend = Self::from_env()?;
        backend.loader_conf = boot_root.join("loader/loader.conf");
        backend.entries_dir = boot_root.join("loader/entries");
        backend.boot_root = boot_root.to_path_buf();
        Ok(backend)
    }

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
        // systemd-boot tries the firmware variable bootctl set-default writes,
        // then loader.conf's `default`, skipping entries it cannot boot.
        let efivar_default = self.loader_efivar("LoaderEntryDefault");
        let conf_default = read_optional(&self.loader_conf)?.as_deref().and_then(loader_default);
        let prefix = self.entry_prefix(efivar_default.as_deref().or(conf_default.as_deref()))?;
        let active = self.active_slot(&prefix)?;
        let bootable_slot = |entry: Option<&str>| {
            entry
                .and_then(parse_entry_name)
                .filter(|(entry_prefix, _)| *entry_prefix == prefix)
                .map(|(_, slot)| slot)
                .filter(|slot| self.missing_boot_file(&entry_name(&prefix, *slot)).is_none())
        };
        let default = bootable_slot(efivar_default.as_deref())
            .or_else(|| bootable_slot(conf_default.as_deref()));
        // systemd-boot removes the variable when it boots the entry, so it is
        // the record of whether a trial is still to come.
        let oneshot = bootable_slot(self.loader_efivar("LoaderEntryOneShot").as_deref());
        Ok(Boot {
            prefix,
            selection: Selection { active, default, oneshot },
        })
    }

    /// Whether this boot is a one-shot trial, from the firmware alone: the
    /// entry systemd-boot booted is not the default. It needs neither the
    /// ESP nor /var mounted, so it still answers when a trial's startup broke.
    /// `None` when the variables do not name two slot entries of one prefix.
    pub fn firmware_trial(&self) -> Option<bool> {
        let (sel_prefix, selected) = parse_entry_name(&self.loader_efivar("LoaderEntrySelected")?)?;
        let default = self.loader_efivar("LoaderEntryDefault").or_else(|| {
            read_optional(&self.loader_conf).ok().flatten().as_deref().and_then(loader_default)
        })?;
        let (def_prefix, default) = parse_entry_name(&default)?;
        (sel_prefix == def_prefix).then_some(selected != default)
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

    /// Write `change`. A one-shot entry first gets the trial options. For a
    /// new default, a pending one-shot is cleared first (so the riskier boot
    /// is the one dropped), the entry loses the trial options, then
    /// loader.conf and the firmware variable name it.
    pub fn apply(&self, boot: &Boot, change: BootChange) -> Result<(), ApplyError> {
        match change {
            BootChange::None => Ok(()),
            BootChange::OneShot(slot) => {
                let entry = self.entry_for(boot, slot).map_err(ApplyError::Untouched)?;
                self.set_trial_options(&entry, true).map_err(ApplyError::Untouched)?;
                self.bootctl(&["set-oneshot", &entry]).map_err(ApplyError::Untouched)
            }
            BootChange::Default(slot) => {
                let entry = self.entry_for(boot, slot).map_err(ApplyError::Untouched)?;
                let mut cleared_oneshot = false;
                if self.loader_efivar("LoaderEntryOneShot").is_some() {
                    self.bootctl(&["set-oneshot", ""]).map_err(ApplyError::Untouched)?;
                    cleared_oneshot = true;
                }
                // From here a failure leaves the one-shot cleared and the old
                // default in place: the safe next boot, but a changed one.
                let failed = |cause: anyhow::Error| {
                    if cleared_oneshot {
                        ApplyError::FirmwareChanged(cause.context(
                            "the pending one-shot boot was cleared; the default is unchanged",
                        ))
                    } else {
                        ApplyError::Untouched(cause)
                    }
                };
                self.set_trial_options(&entry, false).map_err(failed)?;
                let before = read_optional(&self.loader_conf).map_err(failed)?;
                let content = loader_with_default(before.as_deref(), &entry);
                write_durable(&self.loader_conf, content.as_bytes())
                    .with_context(|| format!("writing '{}'", self.loader_conf.display()))
                    .map_err(failed)?;
                if let Err(cause) = self.bootctl(&["set-default", &entry]) {
                    let restore = match &before {
                        Some(content) => write_durable(&self.loader_conf, content.as_bytes()),
                        None => fs::remove_file(&self.loader_conf).map_err(Into::into),
                    };
                    return Err(match restore {
                        Ok(()) => failed(cause),
                        Err(restore) => ApplyError::RestoreFailed { cause, restore },
                    });
                }
                Ok(())
            }
        }
    }

    /// Refuse a slot whose entry names files the ESP lacks: systemd-boot
    /// would skip it and boot another entry.
    pub fn ensure_bootable(&self, boot: &Boot, slot: Slot) -> Result<()> {
        let entry = self.entry_for(boot, slot)?;
        if let Some(missing) = self.missing_boot_file(&entry) {
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

    /// The first file the entry loads that the ESP lacks; the entry itself
    /// when it is missing or unreadable.
    fn missing_boot_file(&self, entry: &str) -> Option<String> {
        let Ok(content) = self.entry_content(entry) else {
            return Some(entry.to_string());
        };
        entry_boot_files(&content)
            .into_iter()
            .find(|file| !self.boot_root.join(file.trim_start_matches('/')).is_file())
    }

    /// Add or remove [`TRIAL_OPTIONS`] on the entry's `options`.
    fn set_trial_options(&self, entry: &str, on: bool) -> Result<()> {
        let content = self.entry_content(entry)?;
        let updated = entry_with_options(&content, |options| {
            options.retain(|o| !TRIAL_OPTIONS.contains(&o.as_str()));
            if on {
                options.extend(TRIAL_OPTIONS.iter().map(|o| o.to_string()));
            }
        });
        if updated == content {
            return Ok(());
        }
        let path = self.entries_dir.join(entry);
        write_durable(&path, updated.as_bytes()).with_context(|| format!("writing '{}'", path.display()))
    }

    /// The block device the slot's entry names with `root=`. The entries are
    /// the record of which partition is which slot; the running root is never
    /// returned for the other slot.
    pub fn slot_device(&self, boot: &Boot, slot: Slot) -> Result<PathBuf> {
        let entry = self.entry_for(boot, slot)?;
        let state_err = |message: String| tagged_err(ErrorClass::StateOrDetection, message);
        let selector = root_selector_from_entry_content(&self.entry_content(&entry)?)
            .ok_or_else(|| state_err(format!("entry '{entry}' has no root= option naming its partition")))?;
        // Formatting gives the filesystem a new UUID and label stays only by
        // care; the partition's PARTUUID is what survives it.
        if !matches!(selector, RootSelector::PartUuid(_)) {
            return Err(state_err(format!(
                "entry '{entry}' names its root as '{selector}'; stage needs root=PARTUUID=, which formatting does not change"
            )));
        }
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
    /// its entry at them. The entry gets `panic=` and, the slot being
    /// uncommitted, the trial options.
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
        let content = entry_with_options(&content, |options| {
            if !options.iter().any(|o| o.starts_with("panic=")) {
                options.push(PANIC_OPTION.to_string());
            }
            options.retain(|o| !TRIAL_OPTIONS.contains(&o.as_str()));
            options.extend(TRIAL_OPTIONS.iter().map(|o| o.to_string()));
        });
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

    /// The running system exposes UEFI variables.
    pub fn efivars_available(&self) -> bool {
        self.sys_firmware_efi.is_dir() && self.efivars.is_dir()
    }

    pub(crate) fn bootctl(&self, args: &[&str]) -> Result<()> {
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

/// loader.conf with `entry` as its only `default`. A file recab creates
/// (`content` is `None`) also gets a menu timeout and the editor disabled.
pub(crate) fn loader_with_default(content: Option<&str>, entry: &str) -> String {
    let key = |line: &str| line.split_whitespace().next().map(str::to_string);
    let mut lines: Vec<String> = vec![format!("default {entry}")];
    match content {
        Some(content) => lines.extend(
            content
                .lines()
                .filter(|line| key(line).as_deref() != Some("default"))
                .map(str::to_string),
        ),
        None => lines.extend(["timeout 5".to_string(), "editor 0".to_string()]),
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

/// The entry with its `linux`/`efi`/`initrd` lines replaced.
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
            _ => lines.push(line.to_string()),
        }
    }
    if !placed {
        lines.append(&mut boot_lines);
    }
    lines.join("\n") + "\n"
}

/// The entry with its `options` edited as one list (systemd-boot joins all
/// `options` lines); the result has a single `options` line where the first
/// one was, or at the end.
fn entry_with_options(content: &str, edit: impl FnOnce(&mut Vec<String>)) -> String {
    let mut options: Vec<String> = Vec::new();
    let mut lines: Vec<Option<String>> = Vec::new();
    let mut slot = None;
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("options") {
            options.extend(parts.map(str::to_string));
            if slot.is_none() {
                slot = Some(lines.len());
                lines.push(None);
            }
        } else {
            lines.push(Some(line.to_string()));
        }
    }
    edit(&mut options);
    let options_line = format!("options {}", options.join(" "));
    match slot {
        Some(index) => lines[index] = Some(options_line),
        None => lines.push(Some(options_line)),
    }
    lines.into_iter().flatten().collect::<Vec<_>>().join("\n") + "\n"
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
