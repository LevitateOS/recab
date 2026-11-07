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

use crate::init_system::InitSystem;
use crate::store::write_durable;
use crate::systemd_boot::{Boot, SystemdBoot};
use crate::{tagged_err, ErrorClass, Slot, StageRequest};

/// Machine configuration every new system keeps from the running one:
/// identity, locale and time. Everything else under /etc comes from the new
/// image, except the accounts ([`ACCOUNT_FILES`]) and what the init system
/// keeps ([`InitSystem::carried`]).
const CARRIED_FILES: &[&str] = &[
    "etc/hostname",
    "etc/hosts",
    "etc/machine-id",
    "etc/subuid",
    "etc/subgid",
    "etc/localtime",
    "etc/locale.conf",
    "etc/vconsole.conf",
    "etc/adjtime",
];
/// Account databases, merged by name: the image's entries (new system users
/// and groups) plus the running system's, whose own entries win (passwords,
/// shells, group members).
const ACCOUNT_FILES: &[&str] = &["etc/passwd", "etc/group", "etc/shadow", "etc/gshadow"];
/// SSH host keys, so the machine keeps its SSH identity.
const SSH_DIR: &str = "etc/ssh";
const SSH_HOST_KEY_PREFIX: &str = "ssh_host_";

/// The filesystem stage makes on a slot.
const SLOT_FILESYSTEM: &str = "ext4";

/// The partition a stage will write, checked before anything is changed.
pub(crate) struct Target {
    device: PathBuf,
}

/// Check everything stage needs without changing anything: the inputs, and
/// a slot partition named by PARTUUID that is not mounted.
pub(crate) fn check(
    backend: &SystemdBoot,
    boot: &Boot,
    slot: Slot,
    request: &StageRequest,
) -> Result<Target> {
    check_inputs(request)?;
    let device = backend.slot_device(boot, slot)?;
    ensure_unmounted(&device)?;
    let partuuid = output(
        "blkid",
        &["-s", "PARTUUID", "-o", "value", path_str(&device)?],
    )?;
    if partuuid.is_empty() {
        bail!(
            "'{}' has no PARTUUID (stage needs a GPT partition)",
            device.display()
        );
    }
    Ok(Target { device })
}

/// Write the slot. Its kernel goes first, so the slot cannot be booted
/// until the kernel and entry are installed at the end.
pub(crate) fn write_slot(
    backend: &SystemdBoot,
    boot: &Boot,
    slot: Slot,
    request: &StageRequest,
    target: &Target,
    init: InitSystem,
) -> Result<()> {
    let device = &target.device;
    let running_root = Path::new("/");

    backend.remove_slot_boot_files(boot, slot)?;

    let label = output("blkid", &["-s", "LABEL", "-o", "value", path_str(device)?])
        .ok()
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| format!("ROOT_{slot}"));
    run(
        &format!("mkfs.{SLOT_FILESYSTEM}"),
        &["-F", "-q", "-L", &label, path_str(device)?],
    )?;

    let mount_point = PathBuf::from(format!("/run/recab/stage-{}", slot.entry_suffix()));
    fs::create_dir_all(&mount_point)
        .with_context(|| format!("creating '{}'", mount_point.display()))?;
    let mounted = Mounted::mount(device, &mount_point)?;
    let recstrap = std::env::var("RECAB_RECSTRAP_BIN").unwrap_or_else(|_| "recstrap".to_string());
    run(
        &recstrap,
        &[
            "--quiet",
            "--rootfs",
            path_str(&request.rootfs)?,
            path_str(&mount_point)?,
        ],
    )
    .context("extracting the rootfs image into the slot")?;
    carry_configuration(running_root, &mount_point, init)?;
    if init.trial_service_enabled(running_root) {
        init.enable_trial_service(&mount_point)
            .context("enabling the new system's packaged trial service")?;
    }
    let partuuid = output(
        "blkid",
        &["-s", "PARTUUID", "-o", "value", path_str(device)?],
    )?;
    write_fstab(
        &running_root.join("etc/fstab"),
        &mount_point.join("etc/fstab"),
        &partuuid,
    )?;
    sync_filesystem(&mount_point)?;
    mounted.unmount()?;

    backend.install_slot_boot_files(
        boot,
        slot,
        &request.kernel,
        request.initrd.as_deref(),
        init.trial_options(),
    )
}

fn check_inputs(request: &StageRequest) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    // The rootfs may be an image file or a partition holding the image.
    let rootfs_ok =
        fs::metadata(&request.rootfs).is_ok_and(|m| m.is_file() || m.file_type().is_block_device());
    if !rootfs_ok {
        bail!(
            "rootfs image '{}' is not a file or block device",
            request.rootfs.display()
        );
    }
    for (what, path) in [
        ("kernel", Some(&request.kernel)),
        ("initrd", request.initrd.as_ref()),
    ] {
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
        .any(|source| {
            source.starts_with('/') && fs::canonicalize(source).is_ok_and(|p| p == device)
        });
    if mounted {
        return Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "slot partition '{}' is mounted; unmount it before staging",
                device.display()
            ),
        ));
    }
    Ok(())
}

/// Copy the machine configuration from the running root into the new one.
fn carry_configuration(from: &Path, to: &Path, init: InitSystem) -> Result<()> {
    let carried = init.carried();
    for rel in CARRIED_FILES.iter().chain(carried.files) {
        let source = from.join(rel);
        if source.symlink_metadata().is_ok() {
            copy_tree(&source, &to.join(rel))?;
        }
    }
    for rel in ACCOUNT_FILES {
        merge_accounts(&from.join(rel), &to.join(rel))?;
    }
    for rel in carried.dirs {
        let source = from.join(rel);
        if source.is_dir() {
            copy_tree(&source, &to.join(rel))?;
        }
    }
    if let Ok(entries) = fs::read_dir(from.join(SSH_DIR)) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(SSH_HOST_KEY_PREFIX)
            {
                copy_tree(&entry.path(), &to.join(SSH_DIR).join(entry.file_name()))?;
            }
        }
    }
    Ok(())
}

/// Copy `source` to `dest` keeping symlinks, permissions and owners. A
/// directory's entries are copied into `dest`, replacing same-named ones
/// and keeping the others (as `cp -a source/. dest`); anything else
/// replaces `dest`. Done here rather than with `cp`, whose options differ
/// between GNU coreutils and busybox.
fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    use std::os::unix::fs::{lchown, MetadataExt};
    let meta =
        fs::symlink_metadata(source).with_context(|| format!("reading '{}'", source.display()))?;
    let context = || format!("copying '{}' to '{}'", source.display(), dest.display());
    let existing = fs::symlink_metadata(dest).ok();
    if meta.is_dir() {
        if existing.as_ref().is_some_and(|e| !e.is_dir()) {
            fs::remove_file(dest).with_context(context)?;
        }
        fs::create_dir_all(dest).with_context(context)?;
        for entry in fs::read_dir(source).with_context(context)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else {
        match existing {
            Some(e) if e.is_dir() => fs::remove_dir_all(dest).with_context(context)?,
            Some(_) => fs::remove_file(dest).with_context(context)?,
            None => {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).with_context(context)?;
                }
            }
        }
        if meta.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(source)?, dest).with_context(context)?;
        } else {
            fs::copy(source, dest).with_context(context)?;
        }
    }
    lchown(dest, Some(meta.uid()), Some(meta.gid())).with_context(context)?;
    if !meta.file_type().is_symlink() {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dest, fs::Permissions::from_mode(meta.mode() & 0o7777))
            .with_context(context)?;
    }
    Ok(())
}

/// Flush the filesystem holding `path` to disk.
fn sync_filesystem(path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let dir = fs::File::open(path).with_context(|| format!("opening '{}'", path.display()))?;
    // SAFETY: syncfs on a file descriptor this function owns.
    if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("syncing '{}'", path.display()));
    }
    Ok(())
}

/// Merge a colon-separated account file by its first field: the image's
/// lines in their order with the running system's line for the same name in
/// their place, then the running system's other lines. Keeps the image
/// file's owner and mode (shadow files are not world-readable).
fn merge_accounts(running: &Path, image: &Path) -> Result<()> {
    let Ok(running_content) = fs::read_to_string(running) else {
        return Ok(());
    };
    let Ok(image_content) = fs::read_to_string(image) else {
        return copy_tree(running, image);
    };
    let name = |line: &str| line.split(':').next().unwrap_or("").to_string();
    let running_lines: Vec<&str> = running_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let mut used = std::collections::HashSet::new();
    let mut merged: Vec<String> = Vec::new();
    for line in image_content.lines().filter(|l| !l.trim().is_empty()) {
        let key = name(line);
        match running_lines.iter().find(|r| name(r) == key) {
            Some(own) => {
                used.insert(key);
                merged.push(own.to_string());
            }
            None => merged.push(line.to_string()),
        }
    }
    merged.extend(
        running_lines
            .iter()
            .filter(|line| !used.contains(&name(line)))
            .map(|line| line.to_string()),
    );
    write_preserving_mode(image, (merged.join("\n") + "\n").as_bytes())
}

/// Replace `path`'s content durably, keeping its owner and permissions.
fn write_preserving_mode(path: &Path, content: &[u8]) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = fs::metadata(path).with_context(|| format!("reading '{}'", path.display()))?;
    write_durable(path, content).with_context(|| format!("writing '{}'", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    std::os::unix::fs::chown(path, Some(meta.uid()), Some(meta.gid()))
        .with_context(|| format!("restoring the owner of '{}'", path.display()))
}

/// The running fstab with the root line pointed at the new slot's partition.
fn write_fstab(running: &Path, dest: &Path, partuuid: &str) -> Result<()> {
    let content =
        fs::read_to_string(running).with_context(|| format!("reading '{}'", running.display()))?;
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
        Ok(Self {
            point: point.to_path_buf(),
            active: true,
        })
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
        bail!(
            "{program} {args:?} failed (exit {})",
            out.status.code().unwrap_or(1)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
