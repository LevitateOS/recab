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
    cmdline_root, cmdline_root_text, entry_file, entry_name, layout_name, loader_default,
    loader_with_default, same_entry, slot_of_entry, Entry, ENTRIES_DIR, INITRD_FILE, KERNEL_FILE,
    LOADER_CONF,
};
use crate::error::refuse;
use crate::state::{BootChange, PersistentState, Selection, Slot};
use crate::target::{partuuid, Kind, Target};
use crate::trial_options::{make_default, make_trial};

/// Where the target ships systemd-boot's EFI binaries, relative to its root.
const SYSTEMD_BOOT_SOURCE_DIR: &str = "usr/lib/systemd/boot/efi";
/// systemd-boot's EFI binary per firmware architecture suffix, and the
/// firmware's default loader name for it (`EFI/BOOT/BOOT<ARCH>.EFI`).
const SYSTEMD_BOOT_ARCHES: &[(&str, &str)] = &[("x64", "BOOTX64.EFI"), ("aa64", "BOOTAA64.EFI")];

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
    pub slot_partuuids: [Option<String>; 2],
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
        Self {
            vars: EfiVars::new(target.firmware.clone()),
            target: target.clone(),
        }
    }

    fn entries_dir(&self) -> PathBuf {
        self.target.esp.join(ENTRIES_DIR)
    }

    fn loader_conf(&self) -> PathBuf {
        self.target.esp.join(LOADER_CONF)
    }

    /// The entry `id` names (with or without `.conf`).
    fn entry_path(&self, id: &str) -> PathBuf {
        let file = if id.to_ascii_lowercase().ends_with(".conf") {
            id.to_string()
        } else {
            format!("{id}.conf")
        };
        self.entries_dir().join(file)
    }

    /// The layout's entry (`None`: the mutable root's), if it exists.
    pub fn entry(&self, prefix: &str, slot: Option<Slot>) -> Result<Option<Entry>> {
        let path = self.entries_dir().join(entry_file(prefix, slot));
        Ok(target_root::read_optional(&path)?
            .as_deref()
            .map(Entry::parse))
    }

    /// The first file the entry `id` loads that the ESP lacks; the entry
    /// itself when it is missing.
    fn missing_boot_file(&self, id: &str) -> Result<Option<String>> {
        let path = self.entry_path(id);
        let Some(text) = target_root::read_optional(&path)? else {
            return Ok(Some(path.display().to_string()));
        };
        let files = Entry::parse(&text).boot_files();
        if files.is_empty() {
            return Ok(Some("entry has no boot loader target".into()));
        }
        for file in files {
            if !target_root::exists(&self.target.esp.join(file.trim_start_matches('/')))? {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }

    /// Every file the slot's entry loads is on the ESP.
    pub fn bootable(&self, boot: &Boot, slot: Slot) -> Result<bool> {
        Ok(self
            .missing_boot_file(&entry_name(&boot.prefix, slot))?
            .is_none())
    }

    /// This is an A/B system: both slots' entries exist.
    pub fn is_ab(&self) -> Result<bool> {
        let prefix = self.target.entry_prefix()?;
        if let Some(state) = crate::store::Store::new(&self.target).load()? {
            if state.slots.len() == 2 {
                return Ok(true);
            }
        }
        Ok(self.entry(&prefix, Some(Slot::A))?.is_some()
            && self.entry(&prefix, Some(Slot::B))?.is_some())
    }

    pub fn read(&self, state: &PersistentState) -> Result<Boot> {
        let prefix = self.target.entry_prefix()?;
        let mut slot_partuuids = [None, None];
        for slot in Slot::ALL {
            let entry_root = self
                .entry(&prefix, Some(slot))?
                .and_then(|entry| entry.root_partuuid());
            let recorded = state
                .slot(slot)
                .map(|record| record.partuuid.to_ascii_lowercase());
            if recorded
                .as_ref()
                .zip(entry_root.as_ref())
                .is_some_and(|(recorded, entry)| recorded != entry)
            {
                return Err(refuse(format!(
                    "slot {slot}'s entry conflicts with its recorded partition"
                )));
            }
            slot_partuuids[slot as usize] = recorded.or(entry_root);
        }
        let root = self.running_root()?;
        let matches = Slot::ALL
            .into_iter()
            .filter(|slot| slot_partuuids[*slot as usize].as_deref() == Some(root.as_str()))
            .collect::<Vec<_>>();
        let active = match matches.as_slice() {
            [slot] => *slot,
            _ => {
                return Err(refuse(
                    "running root does not identify exactly one recorded slot",
                ))
            }
        };
        let active_generation = match self.target.kind {
            Kind::Running => fs::read_to_string(self.target.cmdline_path())?
                .split_whitespace()
                .filter_map(|word| word.strip_prefix("recab.generation="))
                .next_back()
                .map(str::to_string),
            Kind::Installed => crate::filesystem::Root::open(&self.target.root)?
                .optional_text(Path::new(crate::generation::MARKER))?
                .map(|text| serde_json::from_str::<serde_json::Value>(&text))
                .transpose()?
                .and_then(|marker| marker["generation"].as_str().map(str::to_string)),
        };
        let raw_default = self.vars.get(Var::Default)?;
        let raw_oneshot = self.vars.get(Var::OneShot)?;
        let conf_default = target_root::read_optional(&self.loader_conf())?
            .as_deref()
            .and_then(loader_default);
        // systemd-boot boots the variable's entry, else loader.conf's,
        // skipping an entry it cannot boot.
        let mut effective = None;
        for id in [&raw_default, &conf_default].into_iter().flatten() {
            if self.missing_boot_file(id)?.is_none() {
                effective = Some(id.clone());
                break;
            }
        }
        let default = effective
            .as_deref()
            .and_then(|id| slot_of_entry(&prefix, id));
        let foreign_default = effective.is_some() && default.is_none();
        let oneshot = match raw_oneshot
            .as_deref()
            .and_then(|id| slot_of_entry(&prefix, id))
        {
            Some(slot)
                if self
                    .missing_boot_file(&entry_name(&prefix, slot))?
                    .is_none() =>
            {
                Some(slot)
            }
            _ => None,
        };
        let mut bootable = [false; 2];
        for slot in Slot::ALL {
            bootable[slot as usize] = self
                .missing_boot_file(&entry_name(&prefix, slot))?
                .is_none();
        }
        Ok(Boot {
            selection: Selection {
                active,
                active_generation,
                default,
                oneshot,
                foreign_default,
                bootable,
            },
            slot_partuuids,
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
        let Some(selected) = self
            .vars
            .get(Var::Selected)?
            .and_then(|id| slot_of_entry(&prefix, &id))
        else {
            return Ok(None);
        };
        let default = match self.vars.get(Var::Default)? {
            Some(id) => Some(id),
            None => target_root::read_optional(&self.loader_conf())?
                .as_deref()
                .and_then(loader_default),
        };
        Ok(default
            .and_then(|id| slot_of_entry(&prefix, &id))
            .map(|default| selected != default))
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
                $e.map_err(|cause| SelectError {
                    cause,
                    firmware: firmware.clone(),
                    entry: entry_changed,
                    restore: None,
                })?
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
                    if boot
                        .raw_default
                        .as_deref()
                        .is_some_and(|raw| !same_entry(raw, &default_id))
                    {
                        step!(self.vars.set(Var::Default, Some(&default_id)));
                        firmware.push(Var::Default.name());
                    }
                }
                let init = step!(init());
                step!(self.rewrite_entry(&boot.prefix, slot, |entry| make_trial(entry, init)));
                entry_changed = true;
                step!(self.vars.set(Var::OneShot, Some(&id)));
                Ok(())
            }
            BootChange::Default(_) => {
                if boot.raw_oneshot.is_some() {
                    step!(self.vars.set(Var::OneShot, None));
                    firmware.push(Var::OneShot.name());
                }
                step!(self.rewrite_entry(&boot.prefix, slot, make_default));
                entry_changed = true;
                let loader_conf = self.loader_conf();
                let before =
                    step!(target_root::read_optional(&loader_conf).map_err(anyhow::Error::from));
                step!(target_root::write_durable(
                    &loader_conf,
                    loader_with_default(before.as_deref(), &id).as_bytes(),
                    None
                )
                .with_context(|| format!("writing '{}'", loader_conf.display())));
                if let Err(cause) = self.vars.set(Var::Default, Some(&id)) {
                    let restore = match &before {
                        Some(text) => {
                            target_root::write_durable(&loader_conf, text.as_bytes(), None)
                                .map_err(anyhow::Error::from)
                        }
                        None => fs::remove_file(&loader_conf).map_err(anyhow::Error::from),
                    };
                    return Err(SelectError {
                        cause,
                        firmware,
                        entry: entry_changed,
                        restore: restore.err(),
                    });
                }
                Ok(())
            }
        }
    }

    /// Rewrite the slot's entry with `edit`, when it changes anything.
    fn rewrite_entry(&self, prefix: &str, slot: Slot, edit: impl FnOnce(&mut Entry)) -> Result<()> {
        let path = self.entries_dir().join(entry_name(prefix, slot));
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading entry '{}'", path.display()))?;
        let mut entry = Entry::parse(&text);
        edit(&mut entry);
        if entry.render() == text {
            return Ok(());
        }
        target_root::write_durable(&path, entry.render().as_bytes(), None)
            .with_context(|| format!("writing '{}'", path.display()))
    }

    /// The PARTUUID the slot's entry names its root by: the entries are the
    /// record of which partition is which slot.
    pub fn slot_partuuid(&self, boot: &Boot, slot: Slot) -> Result<String> {
        boot.slot_partuuids[slot as usize]
            .clone()
            .ok_or_else(|| refuse(format!("slot {slot} has no recorded partition identity")))
    }

    pub fn verify_generation(
        &self,
        boot: &Boot,
        slot: Slot,
        generation: &crate::generation::Generation,
    ) -> Result<()> {
        let entry = self
            .entry(&boot.prefix, Some(slot))?
            .ok_or_else(|| refuse("candidate entry is missing"))?;
        let entry_generation = entry
            .options()
            .into_iter()
            .find_map(|option| option.strip_prefix("recab.generation=").map(str::to_string));
        if entry_generation.as_deref() != Some(generation.id.as_str()) {
            return Err(refuse("boot entry does not match the recorded generation"));
        }
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        let (kernel, initrd) = entry.artifacts()?;
        let kernel = root.entry(Path::new(&kernel), false)?;
        if crate::generation::hash_reader(&mut kernel.open()?)? != generation.kernel_digest {
            return Err(refuse(
                "kernel content does not match the recorded generation",
            ));
        }
        let initrd = initrd
            .map(|path| -> Result<String> {
                let entry = root.entry(Path::new(&path), false)?;
                crate::generation::hash_reader(&mut entry.open()?)
            })
            .transpose()?;
        if initrd != generation.initrd_digest {
            return Err(refuse(
                "initramfs content does not match the recorded generation",
            ));
        }
        Ok(())
    }

    /// The slot's kernel directory on the ESP.
    pub fn kernel_dir(&self, boot: &Boot, slot: Slot) -> PathBuf {
        self.target.esp.join(layout_name(&boot.prefix, Some(slot)))
    }

    /// Start writing the inactive `slot`: point firmware variables that would
    /// boot it once it has a kernel at the running slot (a default naming
    /// any other entry, a one-shot naming it while no trial is pending: they
    /// change nothing systemd-boot boots now, since it skips the slot), then
    /// remove its entry and transitional kernel inputs. Only finalization may
    /// publish the new entry.
    pub fn begin_slot_write<'a>(&'a self, boot: &'a Boot, slot: Slot) -> Result<SlotWrite<'a>> {
        self.vars.can_change()?;
        let active = entry_name(&boot.prefix, boot.selection.active);
        let unchanged = |var: Var| {
            format!(
                "could not point {} at '{active}'; slot {slot} was not changed",
                var.name()
            )
        };
        if boot
            .raw_default
            .as_deref()
            .is_some_and(|raw| !same_entry(raw, &active))
        {
            self.vars
                .set(Var::Default, Some(&active))
                .with_context(|| unchanged(Var::Default))?;
        }
        let names_slot = boot
            .raw_oneshot
            .as_deref()
            .is_some_and(|raw| slot_of_entry(&boot.prefix, raw) == Some(slot));
        if names_slot && boot.selection.oneshot.is_none() {
            self.vars
                .set(Var::OneShot, None)
                .with_context(|| unchanged(Var::OneShot))?;
        }
        self.invalidate_entry(boot, slot)?;
        let dir = self.kernel_dir(boot, slot);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("removing slot {slot}'s kernel '{}'", dir.display()))?;
            target_root::sync_dir(&self.target.esp)?;
        }
        Ok(SlotWrite {
            backend: self,
            boot,
            slot,
        })
    }

    pub fn invalidate_entry(&self, boot: &Boot, slot: Slot) -> Result<()> {
        if slot == boot.selection.active {
            return Err(refuse("cannot invalidate the running slot"));
        }
        if boot.selection.default != Some(boot.selection.active) || boot.selection.oneshot.is_some()
        {
            return Err(refuse(
                "cancel the trial and select the committed active slot before invalidation",
            ));
        }
        self.vars.can_change()?;
        let active = entry_name(&boot.prefix, boot.selection.active);
        if boot.raw_default.as_deref() != Some(active.as_str()) {
            self.vars.set(Var::Default, Some(&active))?;
        }
        if boot.raw_oneshot.is_some() {
            self.vars.set(Var::OneShot, None)?;
        }
        let path = self.entries_dir().join(entry_name(&boot.prefix, slot));
        match fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        target_root::sync_dir(&self.entries_dir())?;
        Ok(())
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
        fs::create_dir_all(&entries_dir)
            .with_context(|| format!("creating '{}'", entries_dir.display()))?;
        for (slot, title, partuuid) in entries {
            let mut entry = Entry::new(
                title,
                &layout_name(&prefix, *slot),
                initrd.is_some(),
                partuuid,
                options,
            );
            if let Some(slot) = slot {
                entry.make_immutable();
                entry.set_slot(*slot);
            }
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
        target_root::write_durable(
            &loader_conf,
            loader_with_default(before.as_deref(), &id).as_bytes(),
            None,
        )
        .with_context(|| format!("writing '{}'", loader_conf.display()))?;
        Ok(id)
    }

    pub(crate) fn withdraw_installed(&self) -> Result<()> {
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        if root.metadata(Path::new(ENTRIES_DIR))?.is_none() {
            return Ok(());
        }
        for slot in Slot::ALL {
            let entry = root.entry(
                Path::new(&format!(
                    "{ENTRIES_DIR}/{}",
                    entry_name(&self.target.entry_prefix()?, slot)
                )),
                false,
            )?;
            if entry.metadata()?.is_some() {
                fs::remove_file(entry.path())?;
            }
        }
        target_root::sync_dir(&root.entry(Path::new(ENTRIES_DIR), false)?.path())?;
        Ok(())
    }
    pub(crate) fn install_bootloader(&self) -> Result<()> {
        install_loader(&self.target.root, &self.target.esp)
    }
    pub(crate) fn publish_installed(
        &self,
        slot: Slot,
        uuid: &str,
        g: &crate::Generation,
        seal: &crate::BootSeal,
        init: InitSystem,
        options: &str,
    ) -> Result<()> {
        self.verify_manifest(g, seal)?;
        let (kernel, initrd) = object_paths(g)?;
        let mut entry = Entry::new(
            &format!("{} (Slot {slot})", self.target.os_name()?),
            "unused",
            true,
            uuid,
            &immutable_options(options),
        );
        entry.set_artifacts(&kernel, &initrd);
        entry.make_immutable();
        entry.set_slot(slot);
        entry.set_generation(&g.id);
        if slot == Slot::A {
            make_default(&mut entry);
        } else {
            make_trial(&mut entry, init);
        }
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        let path = root.entry(
            Path::new(&format!(
                "{ENTRIES_DIR}/{}",
                entry_name(&self.target.entry_prefix()?, slot)
            )),
            true,
        )?;
        let text = entry.render();
        if root
            .optional_text(Path::new(&format!(
                "{ENTRIES_DIR}/{}",
                entry_name(&self.target.entry_prefix()?, slot)
            )))?
            .as_deref()
            != Some(&text)
        {
            target_root::write_durable(&path.path(), text.as_bytes(), Some(0o644))?;
        }
        Ok(())
    }
    pub(crate) fn installed_default(&self) -> Result<String> {
        let id = entry_name(&self.target.entry_prefix()?, Slot::A);
        let file = self.loader_conf();
        let before = target_root::read_optional(&file)?;
        target_root::write_durable(
            &file,
            loader_with_default(before.as_deref(), &id).as_bytes(),
            None,
        )?;
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
    pub fn running_root(&self) -> Result<String> {
        match self.target.kind {
            Kind::Running => {
                let path = self.target.cmdline_path();
                let cmdline = fs::read_to_string(&path).map_err(|err| {
                    refuse(format!(
                        "unable to determine the running slot: cannot read '{}': {err}",
                        path.display()
                    ))
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
                let mount = table
                    .iter()
                    .rev()
                    .find(|m| m.mount_point == *root)
                    .ok_or_else(|| {
                        refuse(format!(
                            "'{}' is not a mount point; mount the installed system's root there",
                            root.display()
                        ))
                    })?;
                partuuid(Path::new(&mount.source))
            }
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
    /// Save transitional stage inputs without publishing an entry. The shared
    /// finalizer validates the root and installs generation objects before boot.
    pub fn finish(self, kernel: &Path, initrd: Option<&Path>) -> Result<()> {
        install_kernel(
            &self.backend.kernel_dir(self.boot, self.slot),
            kernel,
            initrd,
        )
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
        _ => {
            return Err(refuse(format!(
                "'{}' has systemd-boot for more than one architecture",
                source_dir.display()
            )))
        }
    };
    let source = source_dir.join(&name);
    for dest in [
        esp.join("EFI/systemd").join(&name),
        esp.join("EFI/BOOT").join(fallback),
    ] {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating '{}'", parent.display()))?;
        }
        target_root::copy_durable(&source, &dest)?;
    }
    Ok(())
}

fn immutable_options(options: &str) -> String {
    options
        .split_whitespace()
        .filter(|o| {
            !matches!(*o, "ro" | "rw" | "rootwait")
                && ![
                    "root=",
                    "rootflags=",
                    "rootfstype=",
                    "init=",
                    "rdinit=",
                    "resume=",
                    "recab.",
                    "systemd.volatile",
                    "systemd.mount-extra",
                ]
                .iter()
                .any(|prefix| o.starts_with(prefix))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Content-addressed boot payloads shared by generations and slots on the ESP.
pub(crate) fn object_paths(g: &crate::Generation) -> Result<(String, String)> {
    Ok((
        format!("recab/objects/{}.linux", g.kernel_digest),
        format!(
            "recab/objects/{}.initrd",
            g.initrd_digest
                .as_ref()
                .ok_or_else(|| refuse("finalized generation needs an initramfs"))?
        ),
    ))
}
fn manifest(g: &crate::Generation, seal: &crate::BootSeal) -> serde_json::Value {
    serde_json::json!({"version":1,"generation":g.id,"kernel":g.kernel_digest,"initrd":g.initrd_digest,"kernel_release":seal.kernel_release,"architecture":seal.architecture,"protocol":seal.protocol})
}
impl SystemdBoot {
    fn remove_temporary_boot_files(&self) -> Result<()> {
        // The global recab lock is held. Interrupted atomic copies never had
        // a bootable name, so they cannot be referenced by an entry.
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        for dir in ["recab/objects", "recab/generations"] {
            if root.metadata(Path::new(dir))?.is_none() {
                continue;
            }
            for name in root.names(Path::new(dir))? {
                let Some(name) = name.to_str() else { continue };
                let Some((object, pid)) =
                    name.strip_prefix('.').and_then(|n| n.split_once(".tmp-"))
                else {
                    continue;
                };
                let Some((digest, extension)) = object.split_once('.') else {
                    continue;
                };
                if digest.len() != 64
                    || !digest.bytes().all(|b| b.is_ascii_hexdigit())
                    || pid.is_empty()
                    || !pid.bytes().all(|b| b.is_ascii_digit())
                    || !matches!(extension, "linux" | "initrd" | "json")
                {
                    continue;
                }
                let entry = root.entry(&Path::new(dir).join(name), false)?;
                if !entry.metadata()?.is_some_and(|m| m.is_file()) {
                    return Err(refuse("unexpected temporary boot object type"));
                }
                fs::remove_file(entry.path())?;
            }
            target_root::sync_dir(&root.entry(Path::new(dir), false)?.path())?;
        }
        Ok(())
    }
    pub(crate) fn partition(&self) -> Result<String> {
        let path = fs::canonicalize(&self.target.esp)?;
        let table = target_root::mount_table_at(Path::new("/proc/self/mountinfo"))?;
        let mount = target_root::mount_holding(&table, &path)?
            .ok_or_else(|| refuse("ESP is not mounted"))?;
        if mount.mount_point != path || mount.fstype != "vfat" || mount.root != Path::new("/") {
            return Err(refuse(
                "finalization requires a separately mounted vfat ESP",
            ));
        }
        partuuid(Path::new(&mount.source))
    }
    pub(crate) fn inputs(&self, boot: &Boot, slot: Slot) -> Result<(PathBuf, PathBuf)> {
        let entry = self
            .entry(&boot.prefix, Some(slot))?
            .context("source boot entry is missing")?;
        let (kernel, initrd) = entry.artifacts()?;
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        let kernel = root.entry(Path::new(&kernel), false)?;
        let initrd = root.entry(
            Path::new(&initrd.context("source initramfs missing")?),
            false,
        )?;
        // Resolve only after validating all parent components inside the ESP.
        Ok((
            fs::canonicalize(kernel.path())?,
            fs::canonicalize(initrd.path())?,
        ))
    }
    pub(crate) fn object_inputs(&self, g: &crate::Generation) -> Result<(PathBuf, PathBuf)> {
        let (kernel, initrd) = object_paths(g)?;
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        Ok((
            fs::canonicalize(root.entry(Path::new(&kernel), false)?.path())?,
            fs::canonicalize(root.entry(Path::new(&initrd), false)?.path())?,
        ))
    }
    pub(crate) fn preflight_objects(&self, checked: &crate::candidate::Checked) -> Result<()> {
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        self.remove_temporary_boot_files()?;
        let mut need = 64 * 1024u64;
        for (digest, kind, source) in [
            (&checked.kernel_digest, "linux", &checked.kernel),
            (&checked.initrd_digest, "initrd", &checked.initrd),
        ] {
            let path = format!("recab/objects/{digest}.{kind}");
            if root.metadata(Path::new(&path))?.is_some() {
                if crate::generation::hash_reader(
                    &mut root.entry(Path::new(&path), false)?.open()?,
                )? != *digest
                {
                    return Err(refuse("immutable boot object is corrupt; refusing to overwrite a possibly referenced payload"));
                }
            } else {
                need = need.saturating_add(fs::metadata(source)?.len());
            }
        }
        let file = fs::File::open(&self.target.esp)?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        use std::os::fd::AsRawFd;
        if unsafe { libc::fstatvfs(file.as_raw_fd(), &mut stat) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if need > stat.f_bavail.saturating_mul(stat.f_frsize) {
            return Err(refuse(
                "ESP has insufficient space for candidate boot artifacts",
            ));
        }
        Ok(())
    }
    pub(crate) fn install_objects(
        &self,
        checked: &crate::candidate::Checked,
        g: &crate::Generation,
        seal: &crate::BootSeal,
    ) -> Result<(u64, u64, u64)> {
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        let mut bytes = 0;
        let mut reused = 0;
        for (digest, kind, source) in [
            (&checked.kernel_digest, "linux", &checked.kernel),
            (&checked.initrd_digest, "initrd", &checked.initrd),
        ] {
            let path = format!("recab/objects/{digest}.{kind}");
            let entry = root.entry(Path::new(&path), true)?;
            if entry.metadata()?.is_some() {
                if crate::generation::hash_reader(&mut entry.open()?)? != *digest {
                    return Err(refuse("immutable boot object changed"));
                }
                reused += 1;
                continue;
            }
            target_root::copy_durable(source, &entry.path())?;
            if crate::generation::hash_reader(&mut entry.open()?)? != *digest {
                return Err(refuse("boot payload changed while being installed"));
            }
            bytes += entry.metadata()?.unwrap().len();
        }
        let entry = root.entry(Path::new(&format!("recab/generations/{}.json", g.id)), true)?;
        let expected = manifest(g, seal);
        let data = serde_json::to_vec(&expected)?;
        let mut metadata_bytes = 0;
        let current = if let Some(meta) = entry.metadata()? {
            if meta.len() > 64 * 1024 {
                return Err(refuse("oversized generation manifest"));
            }
            Some(serde_json::from_reader::<_, serde_json::Value>(
                entry.open()?,
            )?)
        } else {
            None
        };
        if current.as_ref().is_some_and(|value| value != &expected) {
            return Err(refuse(
                "generation boot manifest conflicts with immutable identity",
            ));
        }
        if current.is_none() {
            target_root::write_durable(&entry.path(), &data, Some(0o644))?;
            metadata_bytes = data.len() as u64;
        }
        Ok((bytes, reused, metadata_bytes))
    }
    pub(crate) fn publish(
        &self,
        boot: &Boot,
        slot: Slot,
        g: &crate::Generation,
        seal: &crate::BootSeal,
        init: InitSystem,
    ) -> Result<u64> {
        if slot == boot.selection.active {
            return Err(refuse("cannot publish over the active entry"));
        }
        let (kernel, initrd) = object_paths(g)?;
        let source = self
            .entry(&boot.prefix, Some(boot.selection.active))?
            .context("active entry missing")?;
        let extra = immutable_options(&source.options().join(" "));
        let mut entry = Entry::new(
            &format!("{} (Slot {slot})", self.target.os_name()?),
            "unused",
            true,
            &self.slot_partuuid(boot, slot)?,
            &extra,
        );
        entry.set_artifacts(&kernel, &initrd);
        entry.make_immutable();
        entry.set_slot(slot);
        entry.set_generation(&g.id);
        make_trial(&mut entry, init);
        self.verify_manifest(g, seal)?;
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        let dest = root.entry(
            Path::new(&format!("{ENTRIES_DIR}/{}", entry_name(&boot.prefix, slot))),
            true,
        )?;
        let text = entry.render();
        if dest.metadata()?.is_some() && dest.open()?.metadata()?.len() == text.len() as u64 {
            use std::io::Read;
            let mut existing = String::new();
            dest.open()?.take(64 * 1024).read_to_string(&mut existing)?;
            if existing == text {
                return Ok(0);
            }
        }
        target_root::write_durable(&dest.path(), text.as_bytes(), Some(0o644))?;
        Ok(text.len() as u64)
    }
    pub(crate) fn verify_manifest(
        &self,
        g: &crate::Generation,
        seal: &crate::BootSeal,
    ) -> Result<()> {
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        let file = root
            .entry(
                Path::new(&format!("recab/generations/{}.json", g.id)),
                false,
            )?
            .open()?;
        if file.metadata()?.len() > 64 * 1024 {
            return Err(refuse("oversized generation boot manifest"));
        }
        if serde_json::from_reader::<_, serde_json::Value>(file)? != manifest(g, seal) {
            return Err(refuse("generation boot manifest does not match slot seal"));
        }
        Ok(())
    }
    pub(crate) fn garbage_collect(&self, state: &PersistentState) -> Result<()> {
        use std::collections::BTreeSet;
        let root = crate::filesystem::Root::open(&self.target.esp)?;
        let mut keep_objects = BTreeSet::new();
        let mut keep_generations = BTreeSet::new();
        // Foreign entries may legitimately reference recab's content-addressed files.
        for name in root.names(Path::new(ENTRIES_DIR))? {
            if !name
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".conf")
            {
                continue;
            }
            let mut input = root
                .entry(&Path::new(ENTRIES_DIR).join(name), false)?
                .open()?;
            use std::io::Read;
            if input.metadata()?.len() > 64 * 1024 {
                return Err(refuse("oversized boot entry prevents safe collection"));
            }
            let mut text = String::new();
            input.read_to_string(&mut text)?;
            for path in Entry::parse(&text).boot_files() {
                let path = path.trim_start_matches('/');
                if !Path::new(path)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                {
                    return Err(refuse("unsafe boot reference prevents collection"));
                }
                keep_objects.insert(path.to_ascii_lowercase());
            }
        }
        for slot in &state.slots {
            if let Some(g) = &slot.generation {
                keep_generations.insert(format!("{}.json", g.id));
                if slot.seal.is_some() {
                    let (k, i) = object_paths(g)?;
                    keep_objects.extend([k, i]);
                }
            }
        }
        for (dir, is_object) in [("recab/objects", true), ("recab/generations", false)] {
            if root.metadata(Path::new(dir))?.is_none() {
                continue;
            }
            for name in root.names(Path::new(dir))? {
                let Some(name) = name.to_str() else { continue };
                let Some((id, extension)) = name.split_once('.') else {
                    continue;
                };
                if id.len() != 64
                    || !id.bytes().all(|b| b.is_ascii_hexdigit())
                    || !(if is_object {
                        matches!(extension, "linux" | "initrd")
                    } else {
                        extension == "json"
                    })
                {
                    continue;
                }
                let relative = format!("{dir}/{name}");
                if (is_object && keep_objects.contains(&relative.to_ascii_lowercase()))
                    || (!is_object && keep_generations.contains(name))
                {
                    continue;
                }
                let entry = root.entry(Path::new(&relative), false)?;
                if !entry.metadata()?.is_some_and(|m| m.is_file()) {
                    return Err(refuse("unexpected object type prevents collection"));
                }
                fs::remove_file(entry.path())?;
            }
            target_root::sync_dir(&root.entry(Path::new(dir), false)?.path())?;
        }
        Ok(())
    }
}
