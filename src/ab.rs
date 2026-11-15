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
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub schema_version: u32,
    /// The booted root: locked (read-only).
    pub booted: Slot,
    /// The other root: unlocked (read-write); changes go there.
    pub other: Slot,
    /// Where the other root is mounted; `None` when it is not mounted.
    pub other_mount: Option<String>,
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
        boot_default: running.esp.default_slot()?,
        next_boot_once: running.esp.one_time_pick()?,
    })
}

/// What `recab copy` did.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
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
/// booted root's. Works when the other root is stale, damaged or holds no
/// usable filesystem: then its filesystem is created anew and filled.
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
    say(quiet, &format!("reading the booted root {from}"));
    let desired = Inventory::capture(&running.target.root)?;

    crate::other::unlock(&device)?;
    let point = Path::new(OTHER_MOUNT);
    let mut recreated = false;
    match crate::other::mounted(&device)? {
        Some((at, _)) if at != point => {
            return Err(refuse(format!(
                "root {to} is mounted at '{}'; unmount it there",
                at.display()
            )));
        }
        Some((_, true)) => crate::other::remount_writable(point)?,
        Some((_, false)) => {}
        None => {
            if crate::other::mount(&device, point).is_err() {
                say(
                    quiet,
                    &format!("root {to} cannot be mounted; creating its filesystem anew"),
                );
                crate::other::make_filesystem(&device)?;
                crate::other::mount(&device, point)?;
                recreated = true;
            }
        }
    }
    let attempt = |desired: Inventory| -> Result<SyncReport> {
        let sync = Synchronization::check(
            &running.target.root,
            desired,
            point,
            to,
            &to_uuid,
            &from_uuid,
        )?;
        let report = sync.apply()?;
        sync.verify(point)?;
        Ok(report)
    };
    say(
        quiet,
        &format!("copying root {from} to root {to} at {OTHER_MOUNT}"),
    );
    let report = match attempt(desired.clone()) {
        Ok(report) => report,
        Err(err) if !recreated => {
            say(
                quiet,
                &format!("copying into root {to} failed ({err:#}); creating its filesystem anew"),
            );
            crate::other::unmount(point)?;
            crate::other::make_filesystem(&device)?;
            crate::other::mount(&device, point)?;
            recreated = true;
            attempt(desired)?
        }
        Err(err) => return Err(err),
    };
    say(quiet, &format!("writing root {to}'s boot files on the ESP"));
    running.esp.copy_slot(from, to, &to_uuid).map_err(|err| {
        classified(
            ErrorClass::PartialUpdate,
            format!("root {to} holds the copy, but its boot files could not be written: {err:#}"),
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
    if let Some(missing) = running.esp.missing_boot_file(other)? {
        return Err(refuse(format!(
            "root {other}'s entry loads '{missing}', which is missing; run 'recab copy' to give it the booted root's boot files"
        )));
    }
    running.esp.set_one_time_pick(Some(other))?;
    Ok(other)
}

/// What `recab started` found and did.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
pub struct Started {
    pub booted: Slot,
    pub health: Health,
    /// The booted root became the boot default.
    pub made_default: bool,
}

/// Run once per boot after startup: wait (at most `timeout`) for startup to
/// finish; when it finished and the booted root is not
/// the boot default, make it the default. Otherwise change nothing.
pub fn started(timeout: Duration) -> Result<Started> {
    require_root("started")?;
    let running = Running::read()?;
    let health = crate::init_system::health(&running.target, timeout)?;
    let mut made_default = false;
    // Finished starting, with or without a failed service: the machine came
    // up, so this root is the one to start next time. A root that ends in
    // the emergency state or never finishes leaves the default alone.
    if matches!(health.startup, Startup::Running | Startup::Failed) {
        let _lock = running.target.lock()?;
        if running.esp.default_slot()? != Some(running.booted) {
            running.esp.set_default(running.booted)?;
            made_default = true;
        }
    }
    Ok(Started {
        booted: running.booted,
        health,
        made_default,
    })
}
