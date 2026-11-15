//! The installed ESP, which recab alone writes: systemd-boot's EFI binary
//! (copied from the target), loader.conf, the boot entries and each entry's
//! kernel directory; which root is the boot default and which, if any, is
//! picked for the next boot only.
//!
//! systemd-boot boots, in order: `LoaderEntryOneShot` (once; it removes the
//! variable as it boots it), else `LoaderEntryDefault`, else loader.conf's
//! `default`. recab keeps the default variable and loader.conf naming the
//! same entry.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use crate::efivars::{EfiVars, Var};
use crate::entry::{
    cmdline_root, cmdline_root_text, entry_file, entry_name, layout_name, loader_default,
    loader_with_default, slot_of_entry, Entry, ENTRIES_DIR, INITRD_FILE, KERNEL_FILE, LOADER_CONF,
};
use crate::error::{classified, or_class, refuse, ErrorClass};
use crate::target::{partuuid, Kind, Target};
use crate::Slot;

/// Where the target ships systemd-boot's EFI binaries, relative to its root.
const SYSTEMD_BOOT_SOURCE_DIR: &str = "usr/lib/systemd/boot/efi";
/// systemd-boot's EFI binary per firmware architecture suffix, and the
/// firmware's default loader name for it (`EFI/BOOT/BOOT<ARCH>.EFI`).
const SYSTEMD_BOOT_ARCHES: &[(&str, &str)] = &[("x64", "BOOTX64.EFI"), ("aa64", "BOOTAA64.EFI")];

/// The ESP and boot selection of the system the run looks at.
pub(crate) struct Esp {
    target: Target,
    vars: EfiVars,
    prefix: String,
}

impl Esp {
    pub fn new(target: &Target) -> Result<Self> {
        Ok(Self {
            vars: EfiVars::new(target.firmware.clone()),
            prefix: target.entry_prefix()?,
            target: target.clone(),
        })
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.target.esp.join(relative.trim_start_matches('/'))
    }

    fn entry_path(&self, slot: Option<Slot>) -> std::path::PathBuf {
        self.path(ENTRIES_DIR).join(entry_file(&self.prefix, slot))
    }

    /// The entry id of `slot` (`<prefix>-a.conf`).
    pub fn id(&self, slot: Slot) -> String {
        entry_name(&self.prefix, slot)
    }

    /// The layout's entry (`None`: the mutable root's), if it exists.
    pub fn entry(&self, slot: Option<Slot>) -> Result<Option<Entry>> {
        Ok(target_root::read_optional(&self.entry_path(slot))?
            .as_deref()
            .map(Entry::parse))
    }

    /// The first file `slot`'s entry loads that the ESP lacks; the entry
    /// itself when it is missing.
    pub fn missing_boot_file(&self, slot: Slot) -> Result<Option<String>> {
        let Some(entry) = self.entry(Some(slot))? else {
            return Ok(Some(self.entry_path(Some(slot)).display().to_string()));
        };
        let files = entry.boot_files();
        if files.is_empty() {
            return Ok(Some("the entry loads no kernel".into()));
        }
        for file in files {
            if !target_root::exists(&self.path(&file))? {
                return Ok(Some(file));
            }
        }
        Ok(None)
    }

    /// The root systemd-boot boots by default: the default variable's entry
    /// when it can boot, else loader.conf's. `None` when that names no root
    /// of this layout.
    pub fn default_slot(&self) -> Result<Option<Slot>> {
        let conf = target_root::read_optional(&self.path(LOADER_CONF))?
            .as_deref()
            .and_then(loader_default);
        for id in [self.vars.get(Var::Default)?, conf].into_iter().flatten() {
            match slot_of_entry(&self.prefix, &id) {
                Some(slot) if self.missing_boot_file(slot)?.is_none() => return Ok(Some(slot)),
                Some(_) => {}
                // An entry of another system that can boot is the default.
                None if target_root::exists(&self.path(ENTRIES_DIR).join(entry_conf(&id)))? => {
                    return Ok(None);
                }
                None => {}
            }
        }
        Ok(None)
    }

    /// The root picked for the next boot only, if any.
    pub fn one_time_pick(&self) -> Result<Option<Slot>> {
        Ok(self
            .vars
            .get(Var::OneShot)?
            .as_deref()
            .and_then(|id| slot_of_entry(&self.prefix, id)))
    }

    /// Make `slot` the boot default: the firmware's default variable (when
    /// this machine's firmware variables can be changed), then loader.conf.
    pub fn set_default(&self, slot: Slot) -> Result<()> {
        let id = self.id(slot);
        if self.vars.can_change().is_ok() {
            self.vars.set(Var::Default, Some(&id))?;
        }
        self.write_loader_default(&id)
            .map_err(|err| or_class(err, ErrorClass::PartialUpdate))
    }

    /// Pick `slot` for the next boot only, or clear the pick (`None`).
    pub fn set_one_time_pick(&self, slot: Option<Slot>) -> Result<()> {
        self.vars.can_change()?;
        let id = slot.map(|slot| self.id(slot));
        self.vars.set(Var::OneShot, id.as_deref())
    }

    /// Point this machine's firmware at entry `id`: the default variable
    /// names it and no one-time pick is left.
    pub fn point_firmware_at(&self, id: &str) -> Result<()> {
        self.vars.set(Var::Default, Some(id))?;
        self.vars.set(Var::OneShot, None)
    }

    /// The firmware variables can be changed here.
    pub fn firmware_writable(&self) -> bool {
        self.vars.can_change().is_ok()
    }

    /// loader.conf with `id` as its default.
    pub fn write_loader_default(&self, id: &str) -> Result<()> {
        let file = self.path(LOADER_CONF);
        let before = target_root::read_optional(&file)?;
        target_root::write_durable(
            &file,
            loader_with_default(before.as_deref(), id).as_bytes(),
            None,
        )
        .with_context(|| format!("writing '{}'", file.display()))
    }

    /// Write the kernel (and initramfs) into the layout's own directory on
    /// the ESP, then its entry: `title`, root by `uuid`, `options`. An entry
    /// names its files only once they are there.
    pub fn write_slot(
        &self,
        slot: Option<Slot>,
        uuid: &str,
        kernel: &Path,
        initrd: Option<&Path>,
        options: &str,
    ) -> Result<()> {
        let dir_name = layout_name(&self.prefix, slot);
        let dir = self.path(&dir_name);
        fs::create_dir_all(&dir).with_context(|| format!("creating '{}'", dir.display()))?;
        for (source, name) in [(initrd, INITRD_FILE), (Some(kernel), KERNEL_FILE)] {
            let Some(source) = source else { continue };
            let dest = dir.join(name);
            // A layout that already loads this very file keeps it as it is.
            if fs::canonicalize(source).ok() != fs::canonicalize(&dest).ok() {
                target_root::copy_durable(source, &dest)?;
            }
        }
        let title = match slot {
            Some(slot) => format!("{} (Slot {slot})", self.target.os_name()?),
            None => self.target.os_name()?,
        };
        let entry = Entry::new(
            &title,
            &dir_name,
            initrd.is_some(),
            uuid,
            slot.is_some(),
            options,
        );
        let path = self.entry_path(slot);
        target_root::write_durable(&path, entry.render().as_bytes(), Some(0o644))
            .with_context(|| format!("writing '{}'", path.display()))?;
        target_root::sync_dir(&dir)?;
        Ok(())
    }

    /// Give `to` the boot files `from` boots (its kernel, initramfs and
    /// options), with `to`'s own root.
    pub fn copy_slot(&self, from: Slot, to: Slot, to_uuid: &str) -> Result<()> {
        let entry = self
            .entry(Some(from))?
            .ok_or_else(|| refuse(format!("root {from} has no boot entry")))?;
        let (kernel, initrd) = entry.kernel_and_initrd();
        let kernel =
            kernel.ok_or_else(|| refuse(format!("root {from}'s entry names no kernel")))?;
        let kernel = self.path(&kernel);
        let initrd = initrd.map(|initrd| self.path(&initrd));
        self.write_slot(
            Some(to),
            to_uuid,
            &kernel,
            initrd.as_deref(),
            &entry.options(),
        )
    }

    /// Copy the target's own systemd-boot onto its ESP, under its own name
    /// (`EFI/systemd/`) and as the firmware's default loader (`EFI/BOOT/`).
    pub fn install_loader(&self) -> Result<()> {
        let source_dir = self.target.root.join(SYSTEMD_BOOT_SOURCE_DIR);
        let mut found = Vec::new();
        for (arch, fallback) in SYSTEMD_BOOT_ARCHES {
            let name = format!("systemd-boot{arch}.efi");
            if target_root::exists(&source_dir.join(&name))? {
                found.push((name, *fallback));
            }
        }
        let [(name, fallback)] = found.as_slice() else {
            return Err(refuse(format!(
                "'{}' must hold systemd-boot for exactly one architecture",
                source_dir.display()
            )));
        };
        let source = source_dir.join(name);
        for dest in [
            self.path("EFI/systemd").join(name),
            self.path("EFI/BOOT").join(fallback),
        ] {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating '{}'", parent.display()))?;
            }
            target_root::copy_durable(&source, &dest)?;
        }
        Ok(())
    }

    /// The ESP's PARTUUID; it must be a separately mounted vfat partition.
    pub fn partition(&self) -> Result<String> {
        let path = fs::canonicalize(&self.target.esp)?;
        let table = self.target.mount_table()?;
        let mount = target_root::mount_holding(&table, &path)?
            .ok_or_else(|| refuse("the ESP is not mounted"))?;
        if mount.mount_point != path || mount.fstype != "vfat" {
            return Err(refuse(format!(
                "'{}' must be a separately mounted vfat ESP",
                path.display()
            )));
        }
        partuuid(Path::new(&mount.source))
    }

    /// The PARTUUID of the booted root: the kernel command line's `root=`,
    /// or the partition mounted at an install target.
    pub fn booted_root(&self) -> Result<String> {
        match self.target.kind {
            Kind::Running => {
                let path = self.target.cmdline_path();
                let cmdline = fs::read_to_string(&path)
                    .map_err(|err| refuse(format!("cannot read '{}': {err}", path.display())))?;
                cmdline_root(&cmdline).ok_or_else(|| {
                    classified(
                        ErrorClass::StateOrDetection,
                        format!(
                            "the kernel command line names its root as '{}', not by root=PARTUUID= as recab's entries do",
                            cmdline_root_text(&cmdline).unwrap_or("nothing")
                        ),
                    )
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
                            "'{}' is not a mount point; mount the installed root there",
                            root.display()
                        ))
                    })?;
                partuuid(Path::new(&mount.source))
            }
        }
    }
}

/// An entry id as its file name.
fn entry_conf(id: &str) -> String {
    if id.to_ascii_lowercase().ends_with(".conf") {
        id.to_string()
    } else {
        format!("{id}.conf")
    }
}
