//! `recab init`: set up the boot layout of a freshly installed system, so
//! the layout recab later reads and rewrites is written by recab: the
//! target's systemd-boot on its ESP, the entries, the kernels and
//! loader.conf.
//!
//! A/B: see `installation`. Mutable: one entry for the one root.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::boot::Esp;
use crate::error::refuse;
use crate::installation::Installation;
use crate::target::{partuuid, Target};
use recpart::policy::STATE_MOUNT;

/// The root partitions `init` writes entries for.
#[derive(Debug, Clone)]
pub enum InitLayout {
    /// Root A's partition (holding the installed system) and root B's.
    Ab { slot_a: PathBuf, slot_b: PathBuf },
    /// The one root partition of a mutable install.
    Mutable { root_device: PathBuf },
}

/// The installed system `init` sets up.
#[derive(Debug, Clone)]
pub struct InitRequest {
    /// The installed system's root, with its ESP and its `/var` partition
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
    match &request.layout {
        InitLayout::Ab { slot_a, slot_b } => {
            crate::installation::run(target, request, slot_a, slot_b)
        }
        InitLayout::Mutable { root_device } => {
            let uuid = partuuid(root_device)?;
            check_mounts(&target, &request.layout, &uuid)?;
            let esp = Esp::new(&target)?;
            esp.install_loader()?;
            esp.write_slot(
                None,
                &uuid,
                &request.kernel,
                request.initrd.as_deref(),
                &request.options,
            )?;
            let id = format!("{}.conf", target.entry_prefix()?);
            esp.write_loader_default(&id)?;
            point_firmware(&id)
        }
    }
}

/// Point this machine's firmware at the installed system's default entry,
/// unless this machine runs an A/B installation of its own (installing from
/// an installed system onto another disk): a previous install may have left
/// systemd-boot variables naming another entry, and they win over loader.conf.
pub(crate) fn point_firmware(default_entry: &str) -> Result<()> {
    let running = Target::running()?;
    let host = Esp::new(&running)?;
    if Installation::read(&running)?.is_some() {
        eprintln!(
            "recab: note: this machine runs its own A/B installation, so its firmware variables were left as they are; \
the installed system boots '{default_entry}' from its own loader.conf"
        );
    } else if host.firmware_writable() {
        host.point_firmware_at(default_entry).with_context(|| {
            format!("pointing systemd-boot's firmware variables at '{default_entry}'")
        })?;
    } else {
        eprintln!(
            "recab: warning: EFI variables are not available here (no efivarfs), so systemd-boot's \
LoaderEntryDefault and LoaderEntryOneShot left by an earlier install could not be reset to '{default_entry}'"
        );
    }
    Ok(())
}

pub(crate) fn check_mounts(
    target: &Target,
    layout: &InitLayout,
    installed_partuuid: &str,
) -> Result<()> {
    let table = target.mount_table()?;
    let mounted = |point: &Path| table.iter().rev().find(|m| m.mount_point == point);
    let root = mounted(&target.root).ok_or_else(|| {
        refuse(format!(
            "'{}' is not a mount point; mount the installed root there",
            target.root.display()
        ))
    })?;
    if partuuid(Path::new(&root.source))? != installed_partuuid {
        let which = if matches!(layout, InitLayout::Ab { .. }) {
            "--slot-a"
        } else {
            "--root-device"
        };
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
            return Err(refuse(format!(
                "'{}' is not a mount point; mount the installed system's partition there",
                point.display()
            )));
        }
    }
    Ok(())
}
