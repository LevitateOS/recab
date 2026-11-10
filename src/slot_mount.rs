//! Writable access to a positively identified, unmounted inactive ext4 partition.
use crate::error::refuse;
use crate::stage::Mounted;
use crate::system::System;
use crate::Slot;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

pub(crate) struct SlotMount {
    mounted: Option<Mounted>,
    pub point: PathBuf,
    device: File,
    was_readonly: bool,
}
impl SlotMount {
    pub fn open(sys: &System, slot: Slot, id: &str) -> Result<Self> {
        if slot == sys.boot.selection.active {
            return Err(refuse("cannot mount the active slot writable"));
        }
        let uuid = sys.backend.slot_partuuid(&sys.boot, slot)?;
        let output = std::process::Command::new("blkid")
            .args([
                "-c",
                "/dev/null",
                "-t",
                &format!("PARTUUID={uuid}"),
                "-o",
                "device",
            ])
            .output()?;
        let paths = std::str::from_utf8(&output.stdout)?
            .lines()
            .collect::<Vec<_>>();
        if paths.len() != 1 || !output.status.success() {
            return Err(refuse(
                "inactive PARTUUID must identify exactly one block device",
            ));
        }
        let device = std::path::PathBuf::from(paths[0]);
        let device = fs::canonicalize(device)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&device)?;
        let meta = file.metadata()?;
        if !meta.file_type().is_block_device() || crate::target::partuuid(&device)? != uuid {
            return Err(refuse("inactive partition identity changed"));
        }
        if meta.rdev() == fs::metadata(&sys.target.root)?.dev() {
            return Err(refuse("inactive partition is the active filesystem"));
        }
        // Check kernel device numbers, including bind mounts and aliases whose
        // original source path is no longer visible. Never trust only a pathname.
        let table = target_root::mount_table_at(std::path::Path::new("/proc/self/mountinfo"))?;
        if table
            .iter()
            .any(|m| libc::makedev(m.dev.0, m.dev.1) == meta.rdev())
        {
            return Err(refuse(
                "inactive partition is already mounted; unmount it before preparation",
            ));
        }
        fs::create_dir_all(&sys.target.stage_dir)?;
        let directory = crate::filesystem::Root::open(&sys.target.stage_dir)?;
        let name = format!("prepare-{id}");
        let point = sys.target.stage_dir.join(&name);
        let entry = directory.entry(std::path::Path::new(&name), false)?;
        if entry.metadata()?.is_none() {
            fs::create_dir(entry.path())?;
        }
        if !entry.metadata()?.unwrap().is_dir() || !fs::read_dir(entry.path())?.next().is_none() {
            return Err(refuse(
                "preparation mount point must be a real empty directory",
            ));
        }
        if table.iter().any(|m| m.mount_point == point) {
            return Err(refuse("preparation mount point is already mounted"));
        }
        fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o700))?;
        let mut readonly: libc::c_int = 0;
        if unsafe { libc::ioctl(file.as_raw_fd(), 0x125e, &mut readonly) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("reading inactive device protection");
        }
        let mut result = Self {
            mounted: None,
            point,
            device: file,
            was_readonly: readonly != 0,
        };
        let pinned = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            result.device.as_raw_fd()
        ));
        result.mounted = Some(Mounted::new(
            &sys.target,
            &["-t", "ext4", "-o", "ro,noload,nosuid,nodev,noexec"],
            &pinned,
            &result.point,
        )?);
        if fs::metadata(&result.point)?.dev() != meta.rdev() {
            return Err(refuse(
                "mounted inactive filesystem identity does not match",
            ));
        }
        Ok(result)
    }
    pub fn writable(&mut self, sys: &System) -> Result<()> {
        if let Some(mounted) = self.mounted.take() {
            mounted.unmount()?;
        }
        if self.was_readonly {
            self.set_readonly(false)?;
        }
        let pinned = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.device.as_raw_fd()
        ));
        self.mounted = Some(Mounted::new(
            &sys.target,
            &["-t", "ext4", "-o", "rw,nosuid,nodev,noexec"],
            &pinned,
            &self.point,
        )?);
        Ok(())
    }
    fn set_readonly(&self, readonly: bool) -> Result<()> {
        let value: libc::c_int = readonly.into();
        if unsafe { libc::ioctl(self.device.as_raw_fd(), 0x125d, &value) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("restoring inactive device protection");
        }
        Ok(())
    }
    pub fn seal(mut self) -> Result<()> {
        if let Some(mounted) = self.mounted.take() {
            mounted.unmount()?;
        }
        self.was_readonly = true;
        self.set_readonly(true)
    }
    pub fn close(mut self) -> Result<()> {
        if let Some(mounted) = self.mounted.take() {
            mounted.unmount()?;
        }
        if self.was_readonly {
            self.set_readonly(true)?;
        }
        Ok(())
    }
}
impl Drop for SlotMount {
    fn drop(&mut self) {
        drop(self.mounted.take());
        if self.was_readonly {
            if let Err(error) = self.set_readonly(true) {
                eprintln!("recab: warning: {error:#}");
            }
        }
    }
}

/// Destructive legacy staging uses the same checked inactive-device boundary.
/// Its guard restores protection even when extraction or validation fails.
pub(crate) struct StageDevice {
    device: File,
    was_readonly: bool,
}
impl StageDevice {
    pub fn open(
        target: &crate::target::Target,
        path: &std::path::Path,
        uuid: &str,
    ) -> Result<Self> {
        let device = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(fs::canonicalize(path)?)?;
        let meta = device.metadata()?;
        let pinned = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            device.as_raw_fd()
        ));
        if !meta.file_type().is_block_device()
            || crate::target::partuuid(&pinned)? != uuid
            || meta.rdev() == fs::metadata(&target.root)?.dev()
        {
            return Err(refuse(
                "staging device is not the checked inactive partition",
            ));
        }
        if target_root::mount_table_at(std::path::Path::new("/proc/self/mountinfo"))?
            .iter()
            .any(|m| libc::makedev(m.dev.0, m.dev.1) == meta.rdev())
        {
            return Err(refuse("staging partition is already mounted"));
        }
        let mut readonly: libc::c_int = 0;
        if unsafe { libc::ioctl(device.as_raw_fd(), 0x125e, &mut readonly) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let guard = Self {
            device,
            was_readonly: readonly != 0,
        };
        if guard.was_readonly {
            let value: libc::c_int = 0;
            if unsafe { libc::ioctl(guard.device.as_raw_fd(), 0x125d, &value) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(guard)
    }
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.device.as_raw_fd()
        ))
    }
}
impl Drop for StageDevice {
    fn drop(&mut self) {
        if self.was_readonly {
            let value: libc::c_int = 1;
            if unsafe { libc::ioctl(self.device.as_raw_fd(), 0x125d, &value) } < 0 {
                eprintln!(
                    "recab: warning: restoring staged device protection failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}
