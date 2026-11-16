//! The other root's partition: finding it, unlocking its block device,
//! mounting it and, when it cannot be used, creating its filesystem anew.

use anyhow::{Context, Result};
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::refuse;

/// Where the other root is mounted, read-write, on a running A/B system. The
/// initramfs mounts it here at every boot; `copy` mounts it here when it
/// is not.
pub const OTHER_MOUNT: &str = "/run/recab/other";

/// `BLKROSET`: set a block device's read-only flag.
const BLKROSET: libc::Ioctl = 0x125d;

/// The one block device with PARTUUID `uuid`.
pub(crate) fn device(uuid: &str) -> Result<PathBuf> {
    let output = Command::new("blkid")
        .args(["-c", "/dev/null", "-o", "device", "-t"])
        .arg(format!("PARTUUID={uuid}"))
        .output()
        .context("running blkid")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let devices = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    match devices.as_slice() {
        [one] => {
            let device = fs::canonicalize(one.trim())?;
            if !fs::metadata(&device)?.file_type().is_block_device() {
                return Err(refuse(format!(
                    "'{}' is not a block device",
                    device.display()
                )));
            }
            Ok(device)
        }
        [] => Err(refuse(format!("no partition has PARTUUID {uuid}"))),
        _ => Err(refuse(format!(
            "more than one partition has PARTUUID {uuid}; detach the cloned disk"
        ))),
    }
}

/// Where `device` is mounted, and whether that mount is read-only.
pub(crate) fn mounted(device: &Path) -> Result<Option<(PathBuf, bool)>> {
    let rdev = fs::metadata(device)?.rdev();
    let table = target_root::mount_table()?;
    Ok(table
        .iter()
        .rev()
        .find(|m| libc::makedev(m.dev.0, m.dev.1) == rdev)
        .map(|m| {
            (
                m.mount_point.clone(),
                m.options.split(',').any(|o| o == "ro"),
            )
        }))
}

/// Clear `device`'s read-only flag (the initramfs sets it on the booted root).
pub(crate) fn unlock(device: &Path) -> Result<()> {
    let file = fs::File::open(device).with_context(|| format!("opening '{}'", device.display()))?;
    let value: libc::c_int = 0;
    // SAFETY: BLKROSET reads one int from a pointer valid for the call.
    if unsafe { libc::ioctl(file.as_raw_fd(), BLKROSET, &value) } < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("making '{}' writable", device.display()));
    }
    Ok(())
}

/// Mount `device` (ext4) at `point`, read-write or read-only, creating
/// `point`.
pub(crate) fn mount(device: &Path, point: &Path, writable: bool) -> Result<()> {
    fs::create_dir_all(point).with_context(|| format!("creating '{}'", point.display()))?;
    let options = if writable {
        "rw,nosuid,nodev"
    } else {
        "ro,nosuid,nodev"
    };
    target_root::run(
        Command::new("mount")
            .args(["-t", "ext4", "-o", options])
            .arg(device)
            .arg(point),
    )?;
    Ok(())
}

/// Make the mount at `point` read-write.
pub(crate) fn remount_writable(point: &Path) -> Result<()> {
    target_root::run(Command::new("mount").args(["-o", "remount,rw"]).arg(point))?;
    Ok(())
}

pub(crate) fn unmount(point: &Path) -> Result<()> {
    target_root::run(Command::new("umount").arg(point))?;
    Ok(())
}

/// Create a new, empty ext4 filesystem on `device`, keeping its label. Its
/// PARTUUID, which names it in the boot entries, is the partition's and
/// stays.
pub(crate) fn make_filesystem(device: &Path) -> Result<()> {
    let label = target_root::blkid_tag(device, "LABEL").ok().flatten();
    let mut command = Command::new("mkfs.ext4");
    command.args(["-F", "-q"]);
    if let Some(label) = &label {
        command.args(["-L", label]);
    }
    target_root::run(command.arg(device))
        .with_context(|| format!("creating a filesystem on '{}'", device.display()))?;
    Ok(())
}

/// A temporary mount, unmounted (and its directory removed) when dropped.
pub(crate) struct Mounted {
    pub point: PathBuf,
}

impl Mounted {
    /// `device` mounted at `point` with `options`.
    pub fn new(device: &Path, point: &Path, options: &str) -> Result<Self> {
        fs::create_dir_all(point).with_context(|| format!("creating '{}'", point.display()))?;
        target_root::run(
            Command::new("mount")
                .args(["-t", "ext4", "-o", options])
                .arg(device)
                .arg(point),
        )?;
        Ok(Self {
            point: point.to_path_buf(),
        })
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        match unmount(&self.point) {
            Ok(()) => {
                let _ = fs::remove_dir(&self.point);
            }
            Err(err) => eprintln!("recab: warning: {err:#}"),
        }
    }
}
