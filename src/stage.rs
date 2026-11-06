//! `recab stage`: write a prebuilt system into the inactive slot.
//!
//! The slot's partition is the one its boot entry's `root=` names (written
//! at install time). Staging makes the slot unbootable first (its kernel
//! goes), then formats and fills the partition, and installs the kernel and
//! entry last, so an interrupted stage never leaves a half-written slot
//! bootable.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::store::write_durable;
use crate::systemd_boot::{Boot, SystemdBoot};
use crate::{tagged_err, unit, ErrorClass, Slot, StageRequest};

/// Machine configuration a new system keeps from the running one: identity,
/// accounts, locale and time, enabled units, network connections. Everything
/// else under /etc comes from the new image.
const CARRIED_FILES: &[&str] = &[
    "etc/hostname",
    "etc/machine-id",
    "etc/passwd",
    "etc/shadow",
    "etc/group",
    "etc/gshadow",
    "etc/subuid",
    "etc/subgid",
    "etc/localtime",
    "etc/locale.conf",
    "etc/vconsole.conf",
    "etc/adjtime",
];
const CARRIED_DIRS: &[&str] = &["etc/systemd/system", "etc/NetworkManager/system-connections"];
/// SSH host keys, so the machine keeps its SSH identity.
const SSH_DIR: &str = "etc/ssh";
const SSH_HOST_KEY_PREFIX: &str = "ssh_host_";

/// The filesystem stage makes on a slot.
const SLOT_FILESYSTEM: &str = "ext4";

pub(crate) fn write_slot(
    backend: &SystemdBoot,
    boot: &Boot,
    slot: Slot,
    request: &StageRequest,
) -> Result<()> {
    check_inputs(request)?;
    let device = backend.slot_device(boot, slot)?;
    ensure_unmounted(&device)?;
    let running_root = Path::new("/");

    backend.remove_slot_boot_files(boot, slot)?;

    let label = output("blkid", &["-s", "LABEL", "-o", "value", path_str(&device)?])
        .ok()
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| format!("ROOT_{slot}"));
    run(&format!("mkfs.{SLOT_FILESYSTEM}"), &["-F", "-q", "-L", &label, path_str(&device)?])?;

    let mount_point = PathBuf::from(format!("/run/recab/stage-{}", slot.entry_suffix()));
    fs::create_dir_all(&mount_point)
        .with_context(|| format!("creating '{}'", mount_point.display()))?;
    let mounted = Mounted::mount(&device, &mount_point)?;
    let recstrap = std::env::var("RECAB_RECSTRAP_BIN").unwrap_or_else(|_| "recstrap".to_string());
    run(
        &recstrap,
        &["--quiet", "--rootfs", path_str(&request.rootfs)?, path_str(&mount_point)?],
    )
    .context("extracting the rootfs image into the slot")?;
    carry_configuration(running_root, &mount_point)?;
    if unit::installed(running_root) {
        // The new system gets this recab's unit, not a stale copy.
        unit::refresh(&mount_point)?;
    }
    let partuuid = output("blkid", &["-s", "PARTUUID", "-o", "value", path_str(&device)?])?;
    if partuuid.is_empty() {
        bail!("'{}' has no PARTUUID (stage needs a GPT partition)", device.display());
    }
    write_fstab(
        &running_root.join("etc/fstab"),
        &mount_point.join("etc/fstab"),
        &partuuid,
    )?;
    run("sync", &["-f", path_str(&mount_point)?])?;
    mounted.unmount()?;

    backend.install_slot_boot_files(boot, slot, &request.kernel, request.initrd.as_deref())
}

fn check_inputs(request: &StageRequest) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    // The rootfs may be an image file or a partition holding the image.
    let rootfs_ok = fs::metadata(&request.rootfs)
        .is_ok_and(|m| m.is_file() || m.file_type().is_block_device());
    if !rootfs_ok {
        bail!("rootfs image '{}' is not a file or block device", request.rootfs.display());
    }
    for (what, path) in [("kernel", Some(&request.kernel)), ("initrd", request.initrd.as_ref())] {
        if let Some(path) = path {
            if !path.is_file() {
                bail!("{what} '{}' is not a file", path.display());
            }
        }
    }
    Ok(())
}

fn ensure_unmounted(device: &Path) -> Result<()> {
    let mounts = fs::read_to_string("/proc/self/mounts").context("reading /proc/self/mounts")?;
    let mounted = mounts
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .any(|source| source.starts_with('/') && fs::canonicalize(source).is_ok_and(|p| p == device));
    if mounted {
        return Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!("slot partition '{}' is mounted; unmount it before staging", device.display()),
        ));
    }
    Ok(())
}

/// Copy the machine configuration from the running root into the new one.
fn carry_configuration(from: &Path, to: &Path) -> Result<()> {
    for rel in CARRIED_FILES {
        let source = from.join(rel);
        if source.symlink_metadata().is_ok() {
            copy_preserving(&source, &to.join(rel))?;
        }
    }
    for rel in CARRIED_DIRS {
        let source = from.join(rel);
        if !source.is_dir() {
            continue;
        }
        let dest = to.join(rel);
        match dest.symlink_metadata() {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&dest),
            Ok(_) => fs::remove_file(&dest),
            Err(_) => Ok(()),
        }
        .with_context(|| format!("replacing '{}'", dest.display()))?;
        copy_preserving(&source, &dest)?;
    }
    if let Ok(entries) = fs::read_dir(from.join(SSH_DIR)) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(SSH_HOST_KEY_PREFIX) {
                copy_preserving(&entry.path(), &to.join(SSH_DIR).join(entry.file_name()))?;
            }
        }
    }
    Ok(())
}

/// The running fstab with the root line pointed at the new slot's partition.
fn write_fstab(running: &Path, dest: &Path, partuuid: &str) -> Result<()> {
    let content = fs::read_to_string(running)
        .with_context(|| format!("reading '{}'", running.display()))?;
    let mut found_root = false;
    let lines: Vec<String> = content
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            if line.trim_start().starts_with('#') || fields.get(1) != Some(&"/") {
                return line.to_string();
            }
            found_root = true;
            let spec = format!("PARTUUID={partuuid}");
            fields[0] = &spec;
            if let Some(fstype) = fields.get_mut(2) {
                *fstype = SLOT_FILESYSTEM;
            }
            fields.join("\t")
        })
        .collect();
    if !found_root {
        bail!("'{}' has no entry for /", running.display());
    }
    write_durable(dest, (lines.join("\n") + "\n").as_bytes())
        .with_context(|| format!("writing '{}'", dest.display()))
}

/// A filesystem mounted for the stage, unmounted when dropped.
struct Mounted {
    point: PathBuf,
    active: bool,
}

impl Mounted {
    fn mount(device: &Path, point: &Path) -> Result<Self> {
        run("mount", &[path_str(device)?, path_str(point)?])?;
        Ok(Self { point: point.to_path_buf(), active: true })
    }

    fn unmount(mut self) -> Result<()> {
        self.active = false;
        run("umount", &[path_str(&self.point)?])
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if self.active {
            if let Err(err) = run("umount", &[&self.point.to_string_lossy()]) {
                eprintln!("recab: warning: {err:#}");
            }
        }
    }
}

fn copy_preserving(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating '{}'", parent.display()))?;
    }
    run(
        "cp",
        &["-a", "--remove-destination", "--no-target-directory", path_str(source)?, path_str(dest)?],
    )
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path '{}' is not UTF-8", path.display()))
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running '{program}'"))?;
    if !out.status.success() {
        bail!(
            "{program} {args:?} failed (exit {}): {}",
            out.status.code().unwrap_or(1),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn output(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running '{program}'"))?;
    if !out.status.success() {
        bail!("{program} {args:?} failed (exit {})", out.status.code().unwrap_or(1));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
