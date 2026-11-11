//! `recab init`: set up the boot layout of a freshly installed system, so
//! the layout recab later reads and rewrites is written by recab: the
//! target's systemd-boot on its ESP, the entries, the kernel, loader.conf and,
//! for A/B, recab's state. The trial service comes enabled with recab's
//! package.
//!
//! A/B initialization delegates to the recoverable installation transaction:
//! both personalized roots are equivalent and sealed before entries publish.
//! Mutable: one entry for the one root, with the same options.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::error::refuse;
use crate::store::Store;
use crate::systemd_boot::SystemdBoot;
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
    if let InitLayout::Ab { slot_a, slot_b } = &request.layout {
        return crate::installation::run(target, request, slot_a, slot_b)
            .map_err(|e| crate::error::or_class(e, crate::error::ErrorClass::StateOrDetection));
    }
    if Store::new(&target).load()?.is_some() {
        return Err(refuse(
            "recab state already exists; init cannot reset an installed generation ledger",
        ));
    }
    let InitLayout::Mutable { root_device } = &request.layout else {
        unreachable!()
    };
    let uuid = partuuid(root_device)?;
    check_mounts(&target, &request.layout, &uuid)?;
    let entries = vec![(None, target.os_name()?, uuid)];
    let entry = SystemdBoot::new(&target).install_layout(
        &entries,
        None,
        &request.kernel,
        request.initrd.as_deref(),
        &request.options,
    )?;
    point_firmware(&entry)
}

pub(crate) fn point_firmware(default_entry: &str) -> Result<()> {
    // The boot selection before recab's state. A previous install may have
    // left systemd-boot variables naming another entry; they win over
    // loader.conf. This machine's firmware boots the target only when this
    // machine is not itself a recab install (installing from the live
    // medium, not from an installed system onto another disk).
    let host = SystemdBoot::new(&Target::running()?);
    if host.is_ab()? {
        eprintln!(
            "recab: note: this machine runs its own A/B install, so its firmware variables were left as they are; \
the installed system boots '{default_entry}' from its own loader.conf"
        );
    } else if host.firmware_writable() {
        host.point_firmware_at(default_entry).with_context(|| {
            format!("pointing systemd-boot's firmware variables at '{default_entry}'")
        })?;
    } else {
        eprintln!(
            "recab: warning: EFI variables are not available here (no efivarfs), so systemd-boot's \
LoaderEntryDefault and LoaderEntryOneShot left by an earlier install could not be reset to '{default_entry}'; \
reset them from compatible recovery media before booting this installation"
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
