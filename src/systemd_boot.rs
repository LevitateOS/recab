//! The installed ESP, which recab alone writes: systemd-boot's EFI binary
//! (copied from the target), loader.conf, the boot entries and each entry's
//! kernel directory, for the A/B and the one-root (mutable) layout; which
//! slot booted and which boots next; and changing that selection. The text
//! of entries is `entry`'s, the firmware variables `efivars`'.

use anyhow::{Context, Result};
use distro_profile::InitSystem;
use std::fs;
use std::path::{Path, PathBuf};

use crate::efivars::{EfiVars, Var};
use crate::entry::{
    cmdline_root, cmdline_root_text, entry_file, entry_name, layout_name, loader_default, loader_with_default,
    same_entry, slot_of_entry, Entry, ENTRIES_DIR, INITRD_FILE, KERNEL_FILE, LOADER_CONF, PANIC_KEY, PANIC_OPTION,
};
use crate::error::refuse;
use crate::init_system::crash_option;
use crate::state::{BootChange, Selection, Slot};
use crate::target::{partuuid, Kind, Target};
use recinit::live::FAIL_REBOOT_OPTION;

/// Where the target ships systemd-boot's EFI binaries, relative to its root.
const SYSTEMD_BOOT_SOURCE_DIR: &str = "usr/lib/systemd/boot/efi";
/// systemd-boot's EFI binary per firmware architecture suffix, and the
/// firmware's default loader name for it (`EFI/BOOT/BOOT<ARCH>.EFI`).
const SYSTEMD_BOOT_ARCHES: &[(&str, &str)] = &[("x64", "BOOTX64.EFI"), ("aa64", "BOOTAA64.EFI")];

/// The kernel options a trial boot's entry carries until it is committed: the
/// initramfs's fail-reboot word when the entry boots through one (a root that
/// cannot mount reboots instead of opening a shell), and the init's crash
/// option. A failure the kernel sees (no initramfs, a dying init) panics,
/// which `panic=` reboots.
fn trial_options(entry: &Entry, init: InitSystem) -> Vec<&'static str> {
    entry.has_initrd().then_some(FAIL_REBOOT_OPTION).into_iter().chain(crash_option(init)).collect()
}

/// Any option some trial carries: what a committed entry loses.
fn is_trial_option(option: &str) -> bool {
    option == FAIL_REBOOT_OPTION || InitSystem::ALL.iter().any(|init| crash_option(*init) == Some(option))
}

/// The entry carries a trial's options.
pub(crate) fn has_trial_options(entry: &Entry) -> bool {
    entry.options().iter().any(|o| is_trial_option(o))
}

/// The ESP and boot selection of the system the run looks at.
pub(crate) struct SystemdBoot {
    target: Target,
    vars: EfiVars,
}

/// The boot configuration as one recab run sees it.
pub(crate) struct Boot {
    pub prefix: String,
    /// The effective selection: entries systemd-boot would skip are dropped.
    pub selection: Selection,
    /// The `LoaderEntryDefault` and `LoaderEntryOneShot` variables as set,
    /// including a value `selection` dropped because its entry cannot boot
    /// now (it can once that slot is staged).
    raw_default: Option<String>,
    raw_oneshot: Option<String>,
}

/// What a failed [`SystemdBoot::select`] left changed.
pub(crate) struct SelectError {
    pub cause: anyhow::Error,
    /// The firmware variables it had changed (what boots next changed).
    pub firmware: Vec<&'static str>,
    /// It had rewritten the slot's entry options.
    pub entry: bool,
    /// loader.conf had changed and could not be put back.
    pub restore: Option<anyhow::Error>,
}

impl SystemdBoot {
    pub fn new(target: &Target) -> Self {
        Self { vars: EfiVars::new(target.firmware.clone()), target: target.clone() }
    }

    fn entries_dir(&self) -> PathBuf {
        self.target.esp.join(ENTRIES_DIR)
    }

    fn loader_conf(&self) -> PathBuf {
        self.target.esp.join(LOADER_CONF)
    }

    /// The entry `id` names (with or without `.conf`).
    fn entry_path(&self, id: &str) -> PathBuf {
        let file = if id.to_ascii_lowercase().ends_with(".conf") { id.to_string() } else { format!("{id}.conf") };
        self.entries_dir().join(file)
    }

    /// The layout's entry (`None`: the mutable root's), if it exists.
    pub fn entry(&self, prefix: &str, slot: Option<Slot>) -> Result<Option<Entry>> {
        let path = self.entries_dir().join(entry_file(prefix, slot));
        Ok(target_root::read_optional(&path)?.as_deref().map(Entry::parse))
    }

    /// The first file the entry `id` loads that the ESP lacks; the entry
    /// itself when it is missing.
    fn missing_boot_file(&self, id: &str) -> Result<Option<String>> {
        let path = self.entry_path(id);
        let Some(text) = target_root::read_optional(&path)? else {
            return Ok(Some(path.display().to_string()));
        };
        for file in Entry::parse(&text).boot_files() {
            if !target_root::exists(&self.target.esp.join(file.trim_start_matches('/')))? {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }

    /// Every file the slot's entry loads is on the ESP.
    pub fn bootable(&self, boot: &Boot, slot: Slot) -> Result<bool> {
        Ok(self.missing_boot_file(&entry_name(&boot.prefix, slot))?.is_none())
    }

    /// This is an A/B system: both slots' entries exist.
    pub fn is_ab(&self) -> Result<bool> {
        let prefix = self.target.entry_prefix()?;
        Ok(self.entry(&prefix, Some(Slot::A))?.is_some() && self.entry(&prefix, Some(Slot::B))?.is_some())
    }

    pub fn read(&self) -> Result<Boot> {
        let prefix = self.target.entry_prefix()?;
        let active = self.active_slot(&prefix)?;
        let raw_default = self.vars.get(Var::Default)?;
        let raw_oneshot = self.vars.get(Var::OneShot)?;
        let conf_default = target_root::read_optional(&self.loader_conf())?.as_deref().and_then(loader_default);
        // systemd-boot boots the variable's entry, else loader.conf's,
        // skipping an entry it cannot boot.
        let mut effective = None;
        for id in [&raw_default, &conf_default].into_iter().flatten() {
            if self.missing_boot_file(id)?.is_none() {
                effective = Some(id.clone());
                break;
            }
        }
        let default = effective.as_deref().and_then(|id| slot_of_entry(&prefix, id));
        let foreign_default = effective.is_some() && default.is_none();
        let oneshot = match raw_oneshot.as_deref().and_then(|id| slot_of_entry(&prefix, id)) {
            Some(slot) if self.missing_boot_file(&entry_name(&prefix, slot))?.is_none() => Some(slot),
            _ => None,
        };
        let mut bootable = [false; 2];
        for slot in Slot::ALL {
            bootable[slot as usize] = self.missing_boot_file(&entry_name(&prefix, slot))?.is_none();
        }
        Ok(Boot {
            selection: Selection { active, default, oneshot, foreign_default, bootable },
            prefix,
            raw_default,
            raw_oneshot,
        })
    }

    /// Whether this boot is a one-shot trial, from the firmware alone: the
    /// slot systemd-boot booted is not the default slot. It reads the
    /// default variable, and loader.conf only when the variable is unset
    /// (`init` and every new default set it). `None` when the variables do
    /// not name two slots.
    pub fn firmware_trial(&self) -> Result<Option<bool>> {
        let prefix = self.target.entry_prefix()?;
        let Some(selected) = self.vars.get(Var::Selected)?.and_then(|id| slot_of_entry(&prefix, &id)) else {
            return Ok(None);
        };
        let default = match self.vars.get(Var::Default)? {
            Some(id) => Some(id),
            None => target_root::read_optional(&self.loader_conf())?.as_deref().and_then(loader_default),
        };
        Ok(default.and_then(|id| slot_of_entry(&prefix, &id)).map(|default| selected != default))
    }

    /// Write `change` (`plan` checked the slot can boot), checking first
    /// that the firmware can change. A one-shot points a stale
    /// default variable at the default `plan` checked, then gives the entry
    /// the trial options for `init()` and sets the one-shot. A new default
    /// clears a pending one-shot first (so the riskier boot is the one
    /// dropped), strips the trial options, then writes loader.conf and the
    /// default variable (putting loader.conf back if the variable fails).
    pub fn select(
        &self,
        boot: &Boot,
        change: BootChange,
        init: impl FnOnce() -> Result<InitSystem>,
    ) -> Result<(), SelectError> {
        let mut firmware = Vec::new();
        let mut entry_changed = false;
        macro_rules! step {
            ($e:expr) => {
                $e.map_err(|cause| SelectError { cause, firmware: firmware.clone(), entry: entry_changed, restore: None })?
            };
        }
        let slot = match change {
            BootChange::OneShot(slot) | BootChange::Default(slot) => slot,
        };
        step!(self.vars.can_change());
        let id = entry_name(&boot.prefix, slot);
        match change {
            BootChange::OneShot(_) => {
                // A failed trial falls back to the default variable: make it
                // the default `plan` checked, not a stale value it skipped.
                if let Some(default) = boot.selection.default {
                    let default_id = entry_name(&boot.prefix, default);
                    if boot.raw_default.as_deref().is_some_and(|raw| !same_entry(raw, &default_id)) {
                        step!(self.vars.set(Var::Default, Some(&default_id)));
                        firmware.push(Var::Default.name());
                    }
                }
                let init = step!(init());
                step!(self.set_trial_options(&boot.prefix, slot, Some(init)));
                entry_changed = true;
                step!(self.vars.set(Var::OneShot, Some(&id)));
                Ok(())
            }
            BootChange::Default(_) => {
                if boot.raw_oneshot.is_some() {
                    step!(self.vars.set(Var::OneShot, None));
                    firmware.push(Var::OneShot.name());
                }
                step!(self.set_trial_options(&boot.prefix, slot, None));
                entry_changed = true;
                let loader_conf = self.loader_conf();
                let before = step!(target_root::read_optional(&loader_conf).map_err(anyhow::Error::from));
                step!(target_root::write_durable(&loader_conf, loader_with_default(before.as_deref(), &id).as_bytes(), None)
                    .with_context(|| format!("writing '{}'", loader_conf.display())));
                if let Err(cause) = self.vars.set(Var::Default, Some(&id)) {
                    let restore = match &before {
                        Some(text) => target_root::write_durable(&loader_conf, text.as_bytes(), None).map_err(anyhow::Error::from),
                        None => fs::remove_file(&loader_conf).map_err(anyhow::Error::from),
                    };
                    return Err(SelectError { cause, firmware, entry: entry_changed, restore: restore.err() });
                }
                Ok(())
            }
        }
    }

    /// Give the slot's entry a trial's options for `init`, or none (a default).
    fn set_trial_options(&self, prefix: &str, slot: Slot, init: Option<InitSystem>) -> Result<()> {
        let path = self.entries_dir().join(entry_name(prefix, slot));
        let text = fs::read_to_string(&path).with_context(|| format!("reading entry '{}'", path.display()))?;
        let mut entry = Entry::parse(&text);
        let wanted = init.map(|init| trial_options(&entry, init)).unwrap_or_default();
        let mut options = entry.options();
        options.retain(|o| !is_trial_option(o));
        options.extend(wanted.iter().map(|o| o.to_string()));
        entry.set_options(options);
        if entry.render() == text {
            return Ok(());
        }
        target_root::write_durable(&path, entry.render().as_bytes(), None)
            .with_context(|| format!("writing '{}'", path.display()))
    }

    /// The partition the slot's entry names by `root=PARTUUID=`, and that
    /// PARTUUID. The entries are the record of which partition is which
    /// slot; the running root is never returned for the other slot.
    pub fn slot_device(&self, boot: &Boot, slot: Slot) -> Result<(PathBuf, String)> {
        let id = entry_name(&boot.prefix, slot);
        let entry = self.entry(&boot.prefix, Some(slot))?.ok_or_else(|| self.missing_entry(&id))?;
        let partuuid = entry.root_partuuid().ok_or_else(|| {
            refuse(format!(
                "entry '{id}' names its root as '{}', not by root=PARTUUID=, which formatting does not change",
                entry.root_text().unwrap_or_default()
            ))
        })?;
        if partuuid == self.running_root()? {
            return Err(refuse(format!("entry '{id}' names the running root (PARTUUID={partuuid}); refusing to overwrite it")));
        }
        let device = target_root::device_with_tag("PARTUUID", &partuuid)?
            .ok_or_else(|| refuse(format!("entry '{id}' names root PARTUUID={partuuid}, which is not present")))?;
        let device = target_root::canonical(&device)?.unwrap_or(device);
        Ok((device, partuuid))
    }

    /// The slot's kernel directory on the ESP.
    pub fn kernel_dir(&self, boot: &Boot, slot: Slot) -> PathBuf {
        self.target.esp.join(layout_name(&boot.prefix, Some(slot)))
    }

    fn missing_entry(&self, id: &str) -> anyhow::Error {
        refuse(format!(
            "required systemd-boot entry missing: '{}'",
            self.entries_dir().join(id).display()
        ))
    }

    /// Start writing the inactive `slot`: point firmware variables that would
    /// boot it once it has a kernel at the running slot (a default naming
    /// any other entry, a one-shot naming it while no trial is pending: they
    /// change nothing systemd-boot boots now, since it skips the slot), then
    /// remove its kernel, so it cannot boot until [`SlotWrite::finish`].
    pub fn begin_slot_write<'a>(&'a self, boot: &'a Boot, slot: Slot) -> Result<SlotWrite<'a>> {
        self.vars.can_change()?;
        let active = entry_name(&boot.prefix, boot.selection.active);
        let unchanged = |var: Var| format!("could not point {} at '{active}'; slot {slot} was not changed", var.name());
        if boot.raw_default.as_deref().is_some_and(|raw| !same_entry(raw, &active)) {
            self.vars.set(Var::Default, Some(&active)).with_context(|| unchanged(Var::Default))?;
        }
        let names_slot = boot.raw_oneshot.as_deref().is_some_and(|raw| slot_of_entry(&boot.prefix, raw) == Some(slot));
        if names_slot && boot.selection.oneshot.is_none() {
            self.vars.set(Var::OneShot, None).with_context(|| unchanged(Var::OneShot))?;
        }
        let dir = self.kernel_dir(boot, slot);
        if dir.exists() {
            fs::remove_dir_all(&dir).with_context(|| format!("removing slot {slot}'s kernel '{}'", dir.display()))?;
            target_root::sync_dir(&self.target.esp)?;
        }
        Ok(SlotWrite { backend: self, boot, slot })
    }

    /// Set up the boot layout of a freshly installed system: the target's
    /// systemd-boot, the installed root's kernel (`installed`'s directory,
    /// replaced whole), every entry (slot, title, PARTUUID), and loader.conf
    /// defaulting to the installed entry. Returns that entry's id.
    pub fn install_layout(
        &self,
        entries: &[(Option<Slot>, String, String)],
        installed: Option<Slot>,
        kernel: &Path,
        initrd: Option<&Path>,
        options: &str,
    ) -> Result<String> {
        let prefix = self.target.entry_prefix()?;
        install_loader(&self.target.root, &self.target.esp)?;
        let entries_dir = self.entries_dir();
        fs::create_dir_all(&entries_dir).with_context(|| format!("creating '{}'", entries_dir.display()))?;
        for (slot, title, partuuid) in entries {
            let entry = Entry::new(title, &layout_name(&prefix, *slot), initrd.is_some(), partuuid, options);
            let path = entries_dir.join(entry_file(&prefix, *slot));
            target_root::write_durable(&path, entry.render().as_bytes(), None)
                .with_context(|| format!("writing '{}'", path.display()))?;
        }
        let dir = self.target.esp.join(layout_name(&prefix, installed));
        if dir.exists() {
            fs::remove_dir_all(&dir).with_context(|| format!("removing '{}'", dir.display()))?;
        }
        install_kernel(&dir, kernel, initrd)?;
        let id = entry_file(&prefix, installed);
        let loader_conf = self.loader_conf();
        let before = target_root::read_optional(&loader_conf)?;
        target_root::write_durable(&loader_conf, loader_with_default(before.as_deref(), &id).as_bytes(), None)
            .with_context(|| format!("writing '{}'", loader_conf.display()))?;
        Ok(id)
    }

    /// Point this machine's firmware at entry `id`: the default variable
    /// names it and no one-shot is left.
    pub fn point_firmware_at(&self, id: &str) -> Result<()> {
        self.vars.set(Var::Default, Some(id))?;
        self.vars.set(Var::OneShot, None)
    }

    /// The firmware variables can be changed here (else why not, exit 20).
    pub fn can_change(&self) -> Result<()> {
        self.vars.can_change()
    }

    /// The firmware variables can be changed here.
    pub fn firmware_writable(&self) -> bool {
        self.can_change().is_ok()
    }

    /// The PARTUUID of the root of the system recab reads: the kernel command
    /// line's `root=`, or the partition mounted at an install target.
    fn running_root(&self) -> Result<String> {
        match self.target.kind {
            Kind::Running => {
                let path = self.target.cmdline_path();
                let cmdline = fs::read_to_string(&path).map_err(|err| {
                    refuse(format!("unable to determine the running slot: cannot read '{}': {err}", path.display()))
                })?;
                cmdline_root(&cmdline).ok_or_else(|| {
                    refuse(format!(
                        "unable to determine the running slot: the kernel command line names its root as '{}', not by root=PARTUUID= as recab's entries do",
                        cmdline_root_text(&cmdline).unwrap_or("nothing")
                    ))
                })
            }
            Kind::Installed => {
                let root = &self.target.root;
                let table = self.target.mount_table()?;
                let mount = table.iter().rev().find(|m| m.mount_point == *root).ok_or_else(|| {
                    refuse(format!("'{}' is not a mount point; mount the installed system's root there", root.display()))
                })?;
                partuuid(Path::new(&mount.source))
            }
        }
    }

    /// The active slot: the slot entry whose root is the running root. The
    /// entries' `root=`, written at install time, are the record of which
    /// partition is which slot.
    fn active_slot(&self, prefix: &str) -> Result<Slot> {
        let root = self.running_root()?;
        let mut matched = Vec::new();
        for slot in Slot::ALL {
            if self.entry(prefix, Some(slot))?.and_then(|e| e.root_partuuid()).as_deref() == Some(root.as_str()) {
                matched.push(slot);
            }
        }
        match matched.as_slice() {
            [slot] => Ok(*slot),
            [] => Err(refuse(format!(
                "unable to map the running root (PARTUUID={root}) to the A/B entries '{}' and '{}' under '{}'",
                entry_name(prefix, Slot::A),
                entry_name(prefix, Slot::B),
                self.entries_dir().display()
            ))),
            _ => Err(refuse(format!("both slot entries name the running root (PARTUUID={root})"))),
        }
    }
}

/// A slot being written: it has no kernel until [`SlotWrite::finish`].
pub(crate) struct SlotWrite<'a> {
    backend: &'a SystemdBoot,
    boot: &'a Boot,
    slot: Slot,
}

impl SlotWrite<'_> {
    /// Install the slot's kernel (and initrd) and point its entry at them,
    /// with `panic=` and, the slot being uncommitted, the trial options for
    /// `init`. The entry is written first and the kernel last, so the slot
    /// can boot only once everything is in place.
    pub fn finish(self, kernel: &Path, initrd: Option<&Path>, init: InitSystem) -> Result<()> {
        let (backend, prefix, slot) = (self.backend, &self.boot.prefix, self.slot);
        let id = entry_name(prefix, slot);
        let mut entry = backend.entry(prefix, Some(slot))?.ok_or_else(|| backend.missing_entry(&id))?;
        entry.set_boot_files(&layout_name(prefix, Some(slot)), initrd.is_some());
        let mut options = entry.options();
        options.retain(|o| !is_trial_option(o));
        if !options.iter().any(|o| o.starts_with(PANIC_KEY)) {
            options.push(PANIC_OPTION.to_string());
        }
        options.extend(trial_options(&entry, init).iter().map(|o| o.to_string()));
        entry.set_options(options);
        let path = backend.entries_dir().join(&id);
        target_root::write_durable(&path, entry.render().as_bytes(), None)
            .with_context(|| format!("writing '{}'", path.display()))?;
        install_kernel(&backend.kernel_dir(self.boot, slot), kernel, initrd)
    }
}

/// Copy a kernel (and initrd) into `dir` on the ESP, the kernel last: an
/// entry naming them cannot boot until it is there.
fn install_kernel(dir: &Path, kernel: &Path, initrd: Option<&Path>) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating '{}'", dir.display()))?;
    if let Some(initrd) = initrd {
        target_root::copy_durable(initrd, &dir.join(INITRD_FILE))?;
    }
    target_root::copy_durable(kernel, &dir.join(KERNEL_FILE))?;
    Ok(())
}

/// Copy the target's own systemd-boot onto its ESP, under its own name
/// (`EFI/systemd/`) and as the firmware's default loader (`EFI/BOOT/`). The
/// target ships one binary, for its architecture.
fn install_loader(root: &Path, esp: &Path) -> Result<()> {
    let source_dir = root.join(SYSTEMD_BOOT_SOURCE_DIR);
    let mut found = Vec::new();
    for (arch, fallback) in SYSTEMD_BOOT_ARCHES {
        let name = format!("systemd-boot{arch}.efi");
        if target_root::exists(&source_dir.join(&name))? {
            found.push((name, *fallback));
        }
    }
    let (name, fallback) = match found.as_slice() {
        [one] => one.clone(),
        [] => {
            return Err(refuse(format!(
                "'{}' has no systemd-boot EFI binary; extract a complete system first",
                source_dir.display()
            )))
        }
        _ => return Err(refuse(format!("'{}' has systemd-boot for more than one architecture", source_dir.display()))),
    };
    let source = source_dir.join(&name);
    for dest in [esp.join("EFI/systemd").join(&name), esp.join("EFI/BOOT").join(fallback)] {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating '{}'", parent.display()))?;
        }
        target_root::copy_durable(&source, &dest)?;
    }
    Ok(())
}
