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
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::refuse;
use crate::state::{Operation, PersistentState, Slot};
use crate::store::Store;
use crate::systemd_boot::{
    install_loader, layout_entry_name, layout_name, loader_with_default, new_entry, SystemdBoot, ENTRIES_DIR,
    INITRD_FILE, KERNEL_FILE, LOADER_CONF,
};
use crate::target::{partuuid, Target};
use recpart::policy::STATE_MOUNT;

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

/// Set up the system `target` (installed under `request.root`).
pub(crate) fn run(target: Target, request: &InitRequest) -> Result<()> {
    let esp = target.esp.clone();
    let prefix = target.entry_prefix()?;
    let name = target.os_name()?;

    // The entries, each with its slot (`None`: the mutable root) and title;
    // the first is the installed system's.
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
    check_mounts(&target, &request.layout, &entries[0].2)?;
    let installed = entries[0].0;

    install_loader(&target.root, &esp)?;

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
        Store::new(&target).save(&state, "init")?;
    }

    // A previous install may have left systemd-boot variables naming another
    // entry; they win over loader.conf. Point this machine's at the installed one.
    let backend = SystemdBoot::new(&Target::running()?);
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

/// The layout `init` writes must match what is mounted: the installed root
/// (slot A, or the mutable root) at the target's root, its ESP at `boot/`,
/// and for A/B the state partition at `var/`.
fn check_mounts(target: &Target, layout: &InitLayout, installed_partuuid: &str) -> Result<()> {
    let table = target.mount_table()?;
    let mounted = |point: &Path| table.iter().rev().find(|m| m.mount_point == point);
    let root = mounted(&target.root)
        .ok_or_else(|| refuse(format!("'{}' is not a mount point; mount the installed root there", target.root.display())))?;
    if partuuid(Path::new(&root.source))? != installed_partuuid {
        let which = if matches!(layout, InitLayout::Ab { .. }) { "--slot-a" } else { "--root-device" };
        return Err(refuse(format!(
            "{which} is not the partition mounted at '{}' ({})",
            target.root.display(),
            root.source
        )));
    }
    let mut needed = vec![target.esp.clone()];
    if matches!(layout, InitLayout::Ab { .. }) {
        needed.push(target.root.join(STATE_MOUNT.trim_start_matches('/')));
    }
    for point in needed {
        if mounted(&point).is_none() {
            return Err(refuse(format!("'{}' is not a mount point; mount the installed system's partition there", point.display())));
        }
    }
    Ok(())
}
