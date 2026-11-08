//! The systemd-boot backend: which slot booted and which boots next (slot
//! entries, loader.conf and systemd-boot's EFI variables), changing that
//! selection by writing those variables, and each slot's entry and kernel
//! files on the ESP. It needs no `bootctl`, so it works where only
//! systemd-boot's EFI binary is installed.

use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{tagged_err, ErrorClass};
use distro_profile::os_release_value;
use crate::state::{BootChange, Selection};
use crate::store::{copy_durable, read_optional, sync_dir, write_durable};
use crate::Slot;

/// systemd-boot's vendor GUID, which names its EFI variables.
const LOADER_VENDOR_GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// Seconds after a kernel panic before the kernel reboots, on every slot
/// entry recab writes: a trial that panics reboots into the default slot.
pub(crate) const PANIC_OPTION: &str = "panic=10";

/// Every kernel option any init system uses as a trial option (see
/// `InitSystemExt::trial_options`): an entry that becomes the default loses
/// all of them.
const ALL_TRIAL_OPTIONS: &[&str] = &["rd.emergency=reboot", "systemd.crash_action=reboot"];

/// EFI variable attributes: non-volatile, boot-service and runtime access.
const EFI_VARIABLE_ATTRIBUTES: u32 = 0x7;
/// `FS_IMMUTABLE_FL`, which efivarfs sets on variables.
const FS_IMMUTABLE_FL: libc::c_int = 0x10;

const KERNEL_FILE: &str = "vmlinuz";
const INITRD_FILE: &str = "initramfs.img";

/// Where the backend reads and writes (from the run's [`Config`]).
pub(crate) struct SystemdBoot {
    boot_root: PathBuf,
    loader_conf: PathBuf,
    entries_dir: PathBuf,
    os_release: PathBuf,
    proc_cmdline: PathBuf,
    sys_firmware_efi: PathBuf,
    efivars: PathBuf,
    /// `efivars` is efivarfs; else a scratch tree of plain files.
    efivarfs: bool,
    /// The slot entries' name prefix, when not inferred.
    entry_prefix: Option<String>,
}

/// The boot configuration as one recab run sees it.
pub(crate) struct Boot {
    pub prefix: String,
    /// The effective selection: entries systemd-boot would skip are dropped.
    pub selection: Selection,
    /// The `LoaderEntryDefault` and `LoaderEntryOneShot` variables as set,
    /// including a value `selection` dropped because its entry cannot boot
    /// now (it can once that slot is staged).
    pub raw_default: Option<String>,
    pub raw_oneshot: Option<String>,
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
    pub fn new(config: &Config) -> Self {
        Self {
            boot_root: config.boot_root.clone(),
            loader_conf: config.boot_root.join("loader/loader.conf"),
            entries_dir: config.boot_root.join("loader/entries"),
            os_release: config.os_release.clone(),
            proc_cmdline: config.proc_cmdline.clone(),
            sys_firmware_efi: config.sys_firmware_efi.clone(),
            efivars: config.efivars.clone(),
            efivarfs: config.efivarfs,
            entry_prefix: config.entry_prefix.clone(),
        }
    }

    pub fn read(&self) -> Result<Boot> {
        // systemd-boot tries the LoaderEntryDefault firmware variable,
        // then loader.conf's `default`, skipping entries it cannot boot.
        let efivar_default = self.loader_efivar("LoaderEntryDefault")?;
        let conf_default = read_optional(&self.loader_conf)?.as_deref().and_then(loader_default);
        let prefix = self.entry_prefix(efivar_default.as_deref().or(conf_default.as_deref()))?;
        let active = self.active_slot(&prefix)?;
        let bootable_slot = |entry: Option<&str>| -> Result<Option<Slot>> {
            let Some(slot) = entry
                .and_then(parse_entry_name)
                .filter(|(entry_prefix, _)| *entry_prefix == prefix)
                .map(|(_, slot)| slot)
            else {
                return Ok(None);
            };
            Ok(self.missing_boot_file(&entry_name(&prefix, slot))?.is_none().then_some(slot))
        };
        let default = match bootable_slot(efivar_default.as_deref())? {
            Some(slot) => Some(slot),
            None => bootable_slot(conf_default.as_deref())?,
        };
        // systemd-boot removes the variable when it boots the entry, so it is
        // the record of whether a trial is still to come.
        let efivar_oneshot = self.loader_efivar("LoaderEntryOneShot")?;
        let oneshot = bootable_slot(efivar_oneshot.as_deref())?;
        Ok(Boot {
            prefix,
            selection: Selection { active, default, oneshot },
            raw_default: efivar_default,
            raw_oneshot: efivar_oneshot,
        })
    }

    /// Whether this boot is a one-shot trial, from the firmware alone: the
    /// entry systemd-boot booted is not the default. It needs neither the
    /// ESP nor /var mounted, so it still answers when a trial's startup broke.
    /// `None` when the variables do not name two slot entries of one prefix.
    pub fn firmware_trial(&self) -> Result<Option<bool>> {
        let Some((sel_prefix, selected)) =
            self.loader_efivar("LoaderEntrySelected")?.as_deref().and_then(parse_entry_name)
        else {
            return Ok(None);
        };
        let default = match self.loader_efivar("LoaderEntryDefault")? {
            Some(default) => Some(default),
            None => read_optional(&self.loader_conf)?.as_deref().and_then(loader_default),
        };
        let Some((def_prefix, default)) = default.as_deref().and_then(parse_entry_name) else {
            return Ok(None);
        };
        Ok((sel_prefix == def_prefix).then_some(selected != default))
    }

    /// Point firmware variables that disagree with the effective selection
    /// back at it, before `slot` is made bootable (`stage`) or scheduled
    /// (`set-next`): a variable naming an entry systemd-boot skips today
    /// would take effect once that entry can boot. A `LoaderEntryDefault`
    /// naming any entry but `keep`'s (the effective default, the known-good
    /// slot) is set to `keep`'s entry; a `LoaderEntryOneShot` naming `slot`'s
    /// entry while no one-shot is pending is removed. systemd-boot boots what
    /// it booted before; only the stale values are gone.
    pub fn reset_stale_selection(&self, boot: &Boot, keep: Slot, slot: Slot) -> Result<()> {
        let names = |raw: &Option<String>, slot: Slot| {
            raw.as_deref()
                .and_then(parse_entry_name)
                .is_some_and(|(prefix, named)| prefix == boot.prefix && named == slot)
        };
        let refuse = |variable: &str, value: &str, fix: String, err: anyhow::Error| {
            tagged_err(
                ErrorClass::StateOrDetection,
                format!(
                    "systemd-boot's {variable} names '{value}', a stale value that would boot slot {slot} or another entry instead of slot {keep}, and {fix} failed: {err:#}.\n\
Slot {slot} was not changed. Make '{}' writable (efivarfs mounted read-write) and retry.",
                    self.efivars.display()
                ),
            )
        };
        if let Some(value) = &boot.raw_default {
            if !names(&boot.raw_default, keep) {
                let entry = entry_name(&boot.prefix, keep);
                self.set_loader_efivar("LoaderEntryDefault", Some(&entry)).map_err(|err| {
                    refuse("LoaderEntryDefault", value, format!("pointing it at slot {keep}'s '{entry}'"), err)
                })?;
            }
        }
        if let Some(value) = &boot.raw_oneshot {
            if boot.selection.oneshot.is_none() && names(&boot.raw_oneshot, slot) {
                self.set_loader_efivar("LoaderEntryOneShot", None)
                    .map_err(|err| refuse("LoaderEntryOneShot", value, "removing it".to_string(), err))?;
            }
        }
        Ok(())
    }

    /// Changing the boot selection needs a UEFI system with efivarfs, where
    /// the variables systemd-boot obeys are written.
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
        Ok(())
    }

    /// Write `change`. A one-shot entry first gets `trial_options`. For a
    /// new default, a pending one-shot is cleared first (so the riskier boot
    /// is the one dropped), the entry loses the trial options, then
    /// loader.conf and the firmware variable name it.
    pub fn apply(&self, boot: &Boot, change: BootChange, trial_options: &[&str]) -> Result<(), ApplyError> {
        match change {
            BootChange::None => Ok(()),
            BootChange::OneShot(slot) => {
                let entry = self.entry_for(boot, slot).map_err(ApplyError::Untouched)?;
                self.set_trial_options(&entry, trial_options).map_err(ApplyError::Untouched)?;
                self.set_loader_efivar("LoaderEntryOneShot", Some(&entry)).map_err(ApplyError::Untouched)
            }
            BootChange::Default(slot) => {
                let entry = self.entry_for(boot, slot).map_err(ApplyError::Untouched)?;
                let mut cleared_oneshot = false;
                if self.loader_efivar("LoaderEntryOneShot").map_err(ApplyError::Untouched)?.is_some() {
                    self.set_loader_efivar("LoaderEntryOneShot", None).map_err(ApplyError::Untouched)?;
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
                self.set_trial_options(&entry, &[]).map_err(failed)?;
                let before = read_optional(&self.loader_conf).map_err(failed)?;
                let content = loader_with_default(before.as_deref(), &entry);
                write_durable(&self.loader_conf, content.as_bytes())
                    .with_context(|| format!("writing '{}'", self.loader_conf.display()))
                    .map_err(failed)?;
                if let Err(cause) = self.set_loader_efivar("LoaderEntryDefault", Some(&entry)) {
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
        if let Some(missing) = self.missing_boot_file(&entry)? {
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
    /// when it is missing.
    fn missing_boot_file(&self, entry: &str) -> Result<Option<String>> {
        let Some(content) = read_optional(&self.entries_dir.join(entry))? else {
            return Ok(Some(entry.to_string()));
        };
        for file in entry_boot_files(&content) {
            if !target_root::exists(&self.boot_root.join(file.trim_start_matches('/')))? {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }

    /// Make `trial_options` the entry's only trial options (none: a default).
    fn set_trial_options(&self, entry: &str, trial_options: &[&str]) -> Result<()> {
        let content = self.entry_content(entry)?;
        let updated = entry_with_options(&content, |options| {
            options.retain(|o| !ALL_TRIAL_OPTIONS.contains(&o.as_str()));
            options.extend(trial_options.iter().map(|o| o.to_string()));
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
            .device()?
            .ok_or_else(|| state_err(format!("entry '{entry}' names root '{selector}', which is not present")))?;
        let cmdline = fs::read_to_string(&self.proc_cmdline)
            .with_context(|| format!("reading '{}'", self.proc_cmdline.display()))?;
        let running = match root_selector_from_cmdline(&cmdline) {
            Some(selector) => selector.device()?,
            None => None,
        };
        if let Some(running) = running {
            if running == device {
                return Err(state_err(format!(
                    "entry '{entry}' names the running root '{}'; refusing to overwrite it",
                    device.display()
                )));
            }
        }
        Ok(device)
    }

    /// The slot's kernel directory on the ESP, which
    /// [`remove_slot_boot_files`](Self::remove_slot_boot_files) removes.
    pub fn slot_boot_files_dir(&self, boot: &Boot, slot: Slot) -> PathBuf {
        self.boot_root.join(slot_boot_dir(&boot.prefix, slot))
    }

    /// Remove the slot's kernel directory, so its entry cannot boot until
    /// [`install_slot_boot_files`](Self::install_slot_boot_files) runs.
    pub fn remove_slot_boot_files(&self, boot: &Boot, slot: Slot) -> Result<()> {
        let dir = self.slot_boot_files_dir(boot, slot);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("removing slot {slot}'s kernel '{}'", dir.display()))?;
            sync_dir(&self.boot_root)?;
        }
        Ok(())
    }

    /// Install the slot's kernel (and initrd) in its ESP directory and point
    /// its entry at them. The entry gets `panic=` and, the slot being
    /// uncommitted, `trial_options`.
    pub fn install_slot_boot_files(
        &self,
        boot: &Boot,
        slot: Slot,
        kernel: &Path,
        initrd: Option<&Path>,
        trial_options: &[&str],
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
            options.retain(|o| !ALL_TRIAL_OPTIONS.contains(&o.as_str()));
            options.extend(trial_options.iter().map(|o| o.to_string()));
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

    /// Set (or, with `None`, remove) a systemd-boot EFI variable holding an
    /// entry name, as bootctl does: efivarfs marks variables immutable, so
    /// that flag is cleared first, and a variable is replaced by writing
    /// attributes and value in one write.
    pub(crate) fn set_loader_efivar(&self, name: &str, value: Option<&str>) -> Result<()> {
        let path = self.efivars.join(format!("{name}-{LOADER_VENDOR_GUID}"));
        let context = || format!("writing the EFI variable '{}'", path.display());
        if path.exists() {
            clear_immutable(&path).with_context(context)?;
        }
        let Some(value) = value else {
            return match fs::remove_file(&path) {
                Err(err) if err.kind() != ErrorKind::NotFound => Err(anyhow!(err)).with_context(context),
                _ => Ok(()),
            };
        };
        let mut bytes = EFI_VARIABLE_ATTRIBUTES.to_le_bytes().to_vec();
        for unit in value.encode_utf16().chain([0]) {
            bytes.extend(unit.to_le_bytes());
        }
        if !self.efivarfs {
            return fs::write(&path, &bytes).with_context(context);
        }
        use std::io::Write;
        // efivarfs replaces a variable with each complete write; it is never
        // truncated.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(context)?;
        let written = file.write(&bytes).with_context(context)?;
        if written != bytes.len() {
            return Err(anyhow!("short write ({written} of {} bytes)", bytes.len())).with_context(context);
        }
        Ok(())
    }

    /// A systemd-boot EFI variable holding an entry name, if set.
    fn loader_efivar(&self, name: &str) -> Result<Option<String>> {
        let path = self.efivars.join(format!("{name}-{LOADER_VENDOR_GUID}"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(anyhow!(err).context(format!("reading the EFI variable '{}'", path.display()))),
        };
        let unreadable = || anyhow!("the EFI variable '{}' is not a UTF-16 string", path.display());
        // 4 bytes of attributes, then a NUL-terminated UTF-16LE string.
        let units: Vec<u16> = bytes
            .get(4..)
            .ok_or_else(unreadable)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|unit| *unit != 0)
            .collect();
        let value = String::from_utf16(&units).map_err(|_| unreadable())?;
        Ok((!value.is_empty()).then_some(value))
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
            if !target_root::exists(&self.entries_dir.join(&entry))? {
                continue;
            }
            let Some(entry_root) = root_selector_from_entry_content(&self.entry_content(&entry)?)
            else {
                continue;
            };
            if !entry_root.names_same_root(target)? {
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
        let id = read_optional(&self.os_release)?
            .and_then(|text| os_release_value(&text, "ID"))
            .map(|id| id.to_ascii_lowercase());
        if let Some(id) = id {
            if has_pair(&id) {
                return Ok(id);
            }
        }
        let mut slots_by_prefix: BTreeMap<String, BTreeSet<Slot>> = BTreeMap::new();
        for name in target_root::dir_names(&self.entries_dir)? {
            if let Some((prefix, slot)) = parse_entry_name(&name.to_string_lossy()) {
                slots_by_prefix.entry(prefix).or_default().insert(slot);
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
    fn device(&self) -> Result<Option<PathBuf>> {
        let link = match self {
            Self::PartUuid(v) => PathBuf::from(format!("/dev/disk/by-partuuid/{v}")),
            Self::Uuid(v) => PathBuf::from(format!("/dev/disk/by-uuid/{v}")),
            Self::Label(v) => PathBuf::from(format!("/dev/disk/by-label/{v}")),
            Self::DevicePath(v) => PathBuf::from(v),
        };
        match fs::canonicalize(&link) {
            Ok(device) => Ok(Some(device)),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(anyhow!(err).context(format!("resolving '{}'", link.display()))),
        }
    }

    /// Same selector, or two selectors naming the same device (a cmdline
    /// UUID= and an entry PARTUUID= for one partition).
    fn names_same_root(&self, other: &Self) -> Result<bool> {
        Ok(self == other || matches!((self.device()?, other.device()?), (Some(a), Some(b)) if a == b))
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

/// Clear `FS_IMMUTABLE_FL`; filesystems without file flags have nothing to clear.
fn clear_immutable(path: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = fs::File::open(path)?;
    let mut flags: libc::c_int = 0;
    // SAFETY: FS_IOC_GETFLAGS/SETFLAGS read and write one int on an open fd.
    unsafe {
        if libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) != 0 {
            let err = std::io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::ENOTTY | libc::EINVAL | libc::EOPNOTSUPP) => Ok(()),
                _ => Err(err),
            };
        }
        if flags & FS_IMMUTABLE_FL != 0 {
            flags &= !FS_IMMUTABLE_FL;
            if libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &flags) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}
