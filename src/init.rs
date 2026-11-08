//! `recab init`: set up the boot layout of a freshly installed system, so
//! the layout recab later reads and rewrites is written by recab: the
//! target's systemd-boot on its ESP, the entries, the kernel, loader.conf and,
//! for A/B, recab's state. The trial service comes enabled with recab's
//! package.
//!
//! A/B: slot A holds the installed system; slot B is empty until `recab
//! stage`. Both entries are written now, because their `root=PARTUUID=` is the
//! record of which partition is which slot. Mutable: one entry for the one
//! root, with the same options.

use anyhow::{Context, Result};
use distro_profile::os_release_value;
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::state::{Operation, PersistentState};
use crate::store::Store;
use crate::systemd_boot::{
    install_loader, layout_entry_name, layout_name, loader_with_default, new_entry, os_prefix, SystemdBoot,
    ENTRIES_DIR, INITRD_FILE, KERNEL_FILE, LOADER_CONF,
};
use crate::state::Slot;
use crate::error::refuse;
use std::path::PathBuf;

/// The root partitions `init` writes entries for.
#[derive(Debug, Clone)]
pub enum InitLayout {
    /// Slot A's partition (holding the installed system) and slot B's.
    Ab { slot_a: PathBuf, slot_b: PathBuf },
    /// The one root partition of a mutable install.
    Mutable { root_device: PathBuf },
}

/// The installed system `init` sets up.
#[derive(Debug, Clone)]
pub struct InitRequest {
    /// The installed system's root, with its ESP and its state partition
    /// mounted where the layout puts them (`boot/`, `var/`).
    pub root: PathBuf,
    pub layout: InitLayout,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    /// Extra kernel options for the entries (consoles, for example).
    pub options: String,
}

/// Set up the system under `request.root`; `config` is that system's
/// ([`Config::install_target`]).
pub(crate) fn run(config: Config, request: &InitRequest) -> Result<()> {
    let root = &request.root;
    let esp = config.boot_root.clone();
    let (id, name) = os_identity(root)?;
    let prefix = os_prefix(&id);

    // The entries, each with its slot (`None`: the mutable root) and title.
    let entries: Vec<(Option<Slot>, String, String)> = match &request.layout {
        InitLayout::Ab { slot_a, slot_b } => {
            let (a, b) = (partuuid(slot_a)?, partuuid(slot_b)?);
            if a == b {
                return Err(refuse("slot A and slot B are the same partition"));
            }
            vec![
                (Some(Slot::A), format!("{name} (Slot A)"), a),
                (Some(Slot::B), format!("{name} (Slot B)"), b),
            ]
        }
        InitLayout::Mutable { root_device } => vec![(None, name.clone(), partuuid(root_device)?)],
    };
    let installed = entries[0].0;

    install_loader(root, &esp)?;

    // The installed system's kernel; slot B's directory stays empty, so its
    // entry cannot boot until `recab stage` fills it.
    let dir = esp.join(layout_name(&prefix, installed));
    fs::create_dir_all(&dir).with_context(|| format!("creating '{}'", dir.display()))?;
    target_root::copy_durable(&request.kernel, &dir.join(KERNEL_FILE))?;
    if let Some(initrd) = &request.initrd {
        target_root::copy_durable(initrd, &dir.join(INITRD_FILE))?;
    }

    let entries_dir = esp.join(ENTRIES_DIR);
    fs::create_dir_all(&entries_dir).with_context(|| format!("creating '{}'", entries_dir.display()))?;
    for (slot, title, partuuid) in &entries {
        let entry = new_entry(title, &prefix, *slot, request.initrd.is_some(), partuuid, &request.options);
        let path = entries_dir.join(layout_entry_name(&prefix, *slot));
        target_root::write_durable(&path, entry.as_bytes(), None).with_context(|| format!("writing '{}'", path.display()))?;
    }

    let default_entry = layout_entry_name(&prefix, installed);
    let loader_conf = esp.join(LOADER_CONF);
    let before = target_root::read_optional(&loader_conf)?;
    target_root::write_durable(&loader_conf, loader_with_default(before.as_deref(), &default_entry).as_bytes(), None)
        .with_context(|| format!("writing '{}'", loader_conf.display()))?;

    if let InitLayout::Ab { .. } = request.layout {
        let state = PersistentState {
            known_good_slot: Some(Slot::A),
            last_operation: Some(Operation::Init(Slot::A).label()),
            ..PersistentState::default()
        };
        Store::new(&config).save(&state, "init")?;
    }

    // A previous install may have left systemd-boot variables naming another
    // entry; they win over loader.conf. Point them at the installed one.
    let backend = SystemdBoot::new(&config);
    if backend.efivars_available() {
        backend
            .set_loader_efivar("LoaderEntryDefault", Some(&default_entry))
            .and_then(|()| backend.set_loader_efivar("LoaderEntryOneShot", None))
            .with_context(|| format!("pointing systemd-boot's firmware variables at '{default_entry}'"))?;
    } else {
        eprintln!(
            "recab: warning: EFI variables are not available here (no efivarfs), so systemd-boot's \
LoaderEntryDefault and LoaderEntryOneShot left by an earlier install could not be reset to '{default_entry}'; \
until they are, they may select another entry. 'recab stage' resets them before it writes slot B."
        );
    }
    Ok(())
}

/// The target's os-release `ID` (the entry prefix) and `NAME` (entry titles).
fn os_identity(root: &Path) -> Result<(String, String)> {
    let content = target_root::os_release(root)?
        .with_context(|| format!("'{}' has no os-release", root.display()))?;
    let id = os_release_value(&content, "ID")
        .with_context(|| format!("'{}'s os-release has no ID", root.display()))?;
    let name = os_release_value(&content, "NAME").unwrap_or_else(|| id.clone());
    Ok((id, name))
}

fn partuuid(device: &Path) -> Result<String> {
    target_root::partuuid(device)?.ok_or_else(|| {
        refuse(format!("'{}' has no PARTUUID; the root partitions must be GPT partitions", device.display()),
        )
    })
}
