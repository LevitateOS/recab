//! The commands of a running A/B system. The booted root is locked
//! (read-only); the other root is unlocked (read-write, mounted at
//! [`OTHER_MOUNT`]). Which is which follows only from which root booted.
//! The boot default is the root that last finished starting.

use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::boot::Esp;
use crate::error::{classified, refuse, ErrorClass};
use crate::init_system::{Health, Startup};
use crate::installation::Installation;
use crate::inventory::Inventory;
use crate::other::OTHER_MOUNT;
use crate::sync::{SyncReport, Synchronization};
use crate::target::{require_root, Target};
use crate::Slot;

/// The schema of [`Status`].
pub const STATUS_SCHEMA_VERSION: u32 = 11;

/// The running system as an A/B installation.
pub(crate) struct Running {
    pub target: Target,
    pub record: Installation,
    pub esp: Esp,
    pub booted: Slot,
}

impl Running {
    pub fn read() -> Result<Self> {
        let target = Target::running()?;
        let record = Installation::require(&target)?;
        let esp = Esp::new(&target)?;
        let booted = record.slot_of(&esp.booted_root()?)?;
        Ok(Self {
            target,
            record,
            esp,
            booted,
        })
    }

    pub fn other(&self) -> Slot {
        self.booted.other()
    }

    /// Where the other root is mounted: [`OTHER_MOUNT`] when something is
    /// mounted there that is not the booted root.
    pub fn other_mount(&self) -> Result<Option<PathBuf>> {
        use std::os::unix::fs::MetadataExt;
        let point = Path::new(OTHER_MOUNT);
        let table = self.target.mount_table()?;
        let root = std::fs::metadata(&self.target.root)?.dev();
        Ok(table
            .iter()
            .rev()
            .find(|m| m.mount_point == point)
            .filter(|m| libc::makedev(m.dev.0, m.dev.1) != root)
            .map(|_| point.to_path_buf()))
    }
}

/// `recab status --json`.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub schema_version: u32,
    /// The booted root: locked (read-only).
    pub booted: Slot,
    /// The other root: unlocked (read-write); changes go there.
    pub other: Slot,
    /// Where the other root is mounted; `None` when it is not mounted.
    pub other_mount: Option<String>,
    /// The other root has a boot entry whose files are on the ESP, so it
    /// can be started. `false` after a copy that did not finish.
    pub other_bootable: bool,
    /// The root the boot loader starts by default: the one that last
    /// finished starting. `None` when the default names neither root.
    pub boot_default: Option<Slot>,
    /// The root picked for the next boot only, if any.
    pub next_boot_once: Option<Slot>,
}

pub fn status() -> Result<Status> {
    let running = Running::read()?;
    Ok(Status {
        schema_version: STATUS_SCHEMA_VERSION,
        booted: running.booted,
        other: running.other(),
        other_mount: running
            .other_mount()?
            .map(|point| point.display().to_string()),
        other_bootable: running.esp.bootable(running.other())?,
        boot_default: running.esp.default_slot()?,
        next_boot_once: running.esp.one_time_pick()?,
    })
}

/// What `recab copy` did.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize)]
pub struct Copied {
    pub from: Slot,
    pub to: Slot,
    /// The other root's filesystem was created anew first.
    pub recreated: bool,
    pub report: SyncReport,
}

fn say(quiet: bool, text: &str) {
    if !quiet {
        eprintln!("recab: {text}");
    }
}

/// Make the other root a copy of the booted root, and its boot files the
/// booted root's. Works when the other root is stale, damaged or cannot be
/// mounted or read: then its filesystem is created anew and filled.
///
/// Everything is read and checked before anything is written. Then the
/// other root's boot entry is removed, the copy is made, and the entry is
/// written back as the last step: a copy that did not finish cannot be
/// started. An error about the booted root, or a copy that does not fit,
/// leaves the other root as it was.
pub fn copy(quiet: bool) -> Result<Copied> {
    require_root("copy")?;
    let running = Running::read()?;
    let _lock = running.target.lock()?;
    let (from, to) = (running.booted, running.other());
    let to_uuid = running.record.uuid(to).to_string();
    let from_uuid = running.record.uuid(from).to_string();
    let device = crate::other::device(&to_uuid)?;
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&device)?.rdev() == std::fs::metadata(&running.target.root)?.dev() {
            return Err(refuse("the other root's partition is the booted root"));
        }
    }
    let point = Path::new(OTHER_MOUNT);
    let mounted = crate::other::mounted(&device)?;
    if let Some((at, _)) = &mounted {
        if at != point {
            return Err(refuse(format!(
                "root {to} is mounted at '{}'; unmount it there",
                at.display()
            )));
        }
    }
    crate::other::unlock(&device)?;

    // Read and check, writing nothing. A root this run mounts is mounted
    // read-only until its entry is gone.
    let mounted_here = mounted.is_none() && crate::other::mount(&device, point, false).is_ok();
    let readable = mounted.is_some() || mounted_here;
    let mut read_only = mounted.as_ref().is_some_and(|(_, ro)| *ro) || mounted_here;
    let checked = (|| -> Result<_> {
        let roots: &[&Path] = if readable {
            &[&running.target.root, point]
        } else {
            &[&running.target.root]
        };
        let boundaries = crate::filesystem::boundaries(roots)?;
        say(quiet, &format!("reading the booted root {from}"));
        let desired = Inventory::capture(&running.target.root, &boundaries)?;
        let prepared = Synchronization::prepare(
            &running.target.root,
            desired,
            boundaries.clone(),
            to,
            &to_uuid,
            &from_uuid,
        )?;
        // Only the other root's own state calls for a new filesystem.
        let planned = match readable
            .then(|| Synchronization::observe(point, &boundaries))
            .transpose()
        {
            Ok(Some(observed)) => Some(Synchronization::plan(&prepared, observed)?),
            Ok(None) => None,
            Err(err) => {
                say(quiet, &format!("root {to} cannot be read ({err:#})"));
                None
            }
        };
        Ok((prepared, planned))
    })();
    let (prepared, planned) = match checked {
        Ok(checked) => checked,
        Err(err) => {
            if mounted_here {
                let _ = crate::other::unmount(point);
            }
            return Err(err);
        }
    };

    running.esp.remove_entry(to)?;
    let mut recreated = false;
    let sync = match planned {
        Some(sync) => sync,
        None => {
            say(quiet, &format!("creating root {to}'s filesystem anew"));
            if readable {
                crate::other::unmount(point)?;
            }
            crate::other::make_filesystem(&device)?;
            crate::other::mount(&device, point, true)?;
            read_only = false;
            recreated = true;
            let observed = Synchronization::observe(point, prepared.boundaries())?;
            Synchronization::plan(&prepared, observed)?
        }
    };
    if read_only {
        crate::other::remount_writable(point)?;
    }
    say(
        quiet,
        &format!("copying root {from} to root {to} at {OTHER_MOUNT}"),
    );
    let report = sync.apply()?;
    sync.verify(point)?;
    drop(sync);
    say(quiet, &format!("writing root {to}'s boot files on the ESP"));
    running.esp.copy_slot(from, to, &to_uuid).map_err(|err| {
        classified(
            ErrorClass::PartialUpdate,
            format!("root {to} holds the copy, but its boot entry could not be written; run 'recab copy' again: {err:#}"),
        )
    })?;
    Ok(Copied {
        from,
        to,
        recreated,
        report,
    })
}

/// Pick the other root for the next boot only (`cancel`: clear the pick).
/// Returns the other root.
pub fn boot_other(cancel: bool) -> Result<Slot> {
    require_root("boot-other")?;
    let running = Running::read()?;
    let _lock = running.target.lock()?;
    let other = running.other();
    if cancel {
        running.esp.set_one_time_pick(None)?;
        return Ok(other);
    }
    if running.esp.entry(Some(other))?.is_none() {
        return Err(refuse(format!(
            "root {other} has no boot entry because a copy into it did not finish; run 'recab copy' to finish it"
        )));
    }
    if let Some(missing) = running.esp.missing_boot_file(other)? {
        return Err(refuse(format!(
            "root {other}'s entry loads '{missing}', which is missing; run 'recab copy' to give it the booted root's boot files"
        )));
    }
    running.esp.set_one_time_pick(Some(other))?;
    Ok(other)
}

/// What `recab started` found and did.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize)]
pub struct Started {
    pub booted: Slot,
    pub health: Health,
    /// The booted root became the boot default.
    pub made_default: bool,
    /// Startup did not finish on a root that is not the boot default, so
    /// the machine is restarting into the default.
    pub restarting: bool,
}

/// Run once per boot after startup: wait (at most `timeout`) for startup to
/// finish.
/// - Finished (with or without a failed service): when the booted root is
///   not the boot default, make it the default.
/// - Ended in the init's emergency state or did not finish in time: when the
///   booted root is not the boot default, restart, so the default starts.
///   When it is the default, change nothing: there is nothing to go back to.
/// - Otherwise (shutting down) change nothing.
pub fn started(timeout: Duration) -> Result<Started> {
    require_root("started")?;
    let running = Running::read()?;
    let health = crate::init_system::health(&running.target, timeout)?;
    let mut made_default = false;
    let mut restarting = false;
    match health.startup {
        Startup::Running | Startup::Failed => {
            let _lock = running.target.lock()?;
            if running.esp.default_slot()? != Some(running.booted) {
                running.esp.set_default(running.booted)?;
                made_default = true;
            }
        }
        Startup::Emergency | Startup::TimedOut => {
            if running.esp.default_slot()? != Some(running.booted) {
                eprintln!(
                    "recab: root {} {}; it is not the boot default, so the machine restarts into the default",
                    running.booted,
                    health.describe(timeout)
                );
                crate::init_system::reboot(running.target.init()?)?;
                restarting = true;
            }
        }
        Startup::Inconclusive => {}
    }
    Ok(Started {
        booted: running.booted,
        health,
        made_default,
        restarting,
    })
}
