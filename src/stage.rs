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

/// Where an EROFS image's superblock starts, and its magic number there.
const EROFS_SUPERBLOCK_OFFSET: u64 = 1024;
const EROFS_SUPER_MAGIC: u32 = 0xE0F5_E1E2;
/// Block sizes (as a bit shift) an EROFS superblock can declare.
const EROFS_BLOCK_SIZE_BITS: std::ops::RangeInclusive<u8> = 9..=16;

/// The inputs a stage reads, checked before anything is changed.
pub(crate) struct Inputs {
    /// The rootfs image, canonical.
    rootfs: PathBuf,
    /// Its size: the file's length, or the image size a partition's EROFS
    /// superblock declares.
    rootfs_bytes: u64,
    /// The device number, when the rootfs is a block device.
    rootfs_rdev: Option<u64>,
}

/// The partition a stage will write, checked before anything is changed.
pub(crate) struct Target {
    device: PathBuf,
}

fn refuse(message: String) -> anyhow::Error {
    tagged_err(ErrorClass::StateOrDetection, message)
}

/// Check the inputs without changing anything: an EROFS rootfs image (a
/// file or a partition), a kernel and initrd, and none of them inside the
/// slot's kernel directory, which stage removes first.
pub(crate) fn check_inputs(
    backend: &SystemdBoot,
    boot: &Boot,
    slot: Slot,
    request: &StageRequest,
) -> Result<Inputs> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let rootfs = &request.rootfs;
    // The rootfs may be an image file or a partition holding the image.
    let meta = fs::metadata(rootfs)
        .ok()
        .filter(|m| m.is_file() || m.file_type().is_block_device())
        .ok_or_else(|| {
            refuse(format!(
                "rootfs image '{}' is not a file or block device",
                rootfs.display()
            ))
        })?;
    let Some(declared_bytes) = erofs_image_bytes(rootfs)? else {
        return Err(refuse(format!(
            "rootfs '{}' is not an EROFS image (no EROFS superblock at byte {EROFS_SUPERBLOCK_OFFSET}); \
pass the system's .erofs image, or the partition holding it",
            rootfs.display()
        )));
    };
    for (what, path) in [
        ("kernel", Some(&request.kernel)),
        ("initrd", request.initrd.as_ref()),
    ] {
        if let Some(path) = path {
            if !path.is_file() {
                return Err(refuse(format!("{what} '{}' is not a file", path.display())));
            }
        }
    }
    // Stage removes this directory before it installs the kernel and initrd.
    let slot_dir = backend.slot_boot_files_dir(boot, slot);
    if let Ok(slot_dir) = fs::canonicalize(&slot_dir) {
        for (what, path) in [
            ("rootfs", Some(rootfs)),
            ("kernel", Some(&request.kernel)),
            ("initrd", request.initrd.as_ref()),
        ] {
            let Some(path) = path else { continue };
            if fs::canonicalize(path).is_ok_and(|p| p.starts_with(&slot_dir)) {
                return Err(refuse(format!(
                    "{what} '{}' is inside slot {slot}'s kernel directory '{}', which stage removes before installing the new kernel; \
copy it elsewhere first (for example to /var/tmp) and pass that path",
                    path.display(),
                    slot_dir.display()
                )));
            }
        }
    }
    let block_device = meta.file_type().is_block_device();
    Ok(Inputs {
        rootfs: fs::canonicalize(rootfs)
            .with_context(|| format!("resolving '{}'", rootfs.display()))?,
        rootfs_bytes: if block_device { declared_bytes } else { meta.len() },
        rootfs_rdev: block_device.then(|| meta.rdev()),
    })
}

/// Check the slot's partition without changing anything: named by
/// PARTUUID, not the running root, not mounted, not the rootfs itself, and
/// large enough for the rootfs image. The rootfs partition, if it is one,
/// must not be mounted either.
pub(crate) fn check_target(
    backend: &SystemdBoot,
    boot: &Boot,
    slot: Slot,
    inputs: &Inputs,
) -> Result<Target> {
    use std::os::unix::fs::MetadataExt;
    let device = backend.slot_device(boot, slot)?;
    if let Some(point) = mounted_at(&device)? {
        return Err(refuse(format!(
            "slot partition '{}' is mounted at '{point}'; unmount it before staging",
            device.display()
        )));
    }
    let device_rdev = fs::metadata(&device).ok().map(|m| m.rdev());
    if inputs.rootfs == device || (inputs.rootfs_rdev.is_some() && inputs.rootfs_rdev == device_rdev) {
        return Err(refuse(format!(
            "rootfs '{}' is slot {slot}'s partition, which stage formats; pass the EROFS image file or the partition holding it",
            inputs.rootfs.display()
        )));
    }
    if inputs.rootfs_rdev.is_some() {
        let point = mounted_at(&inputs.rootfs)?.or_else(|| {
            let root_dev = fs::metadata("/").ok().map(|m| m.dev());
            (root_dev == inputs.rootfs_rdev).then(|| "/".to_string())
        });
        if let Some(point) = point {
            return Err(refuse(format!(
                "rootfs partition '{}' is mounted at '{point}'; unmount it, or pass the EROFS image file",
                inputs.rootfs.display()
            )));
        }
    }
    let slot_bytes = block_device_bytes(&device)?;
    if inputs.rootfs_bytes > slot_bytes {
        return Err(refuse(format!(
            "rootfs image '{}' ({} bytes) does not fit in slot {slot}'s partition '{}' ({slot_bytes} bytes)",
            inputs.rootfs.display(),
            inputs.rootfs_bytes,
            device.display()
        )));
    }
    let partuuid = output(
        "blkid",
        &["-s", "PARTUUID", "-o", "value", path_str(&device)?],
    )?;
    if partuuid.is_empty() {
        return Err(refuse(format!(
            "'{}' has no PARTUUID (stage needs a GPT partition)",
            device.display()
        )));
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

/// The image size an EROFS superblock at the start of `path` declares
/// (block count times block size), or `None` when there is none.
fn erofs_image_bytes(path: &Path) -> Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom};
    let context = || format!("reading rootfs '{}'", path.display());
    let mut file = fs::File::open(path).with_context(context)?;
    file.seek(SeekFrom::Start(EROFS_SUPERBLOCK_OFFSET)).with_context(context)?;
    // magic (u32), checksum (u32), feature_compat (u32), blkszbits (u8),
    // sb_extslots (u8), root nid (u16), inos (u64), build time (u64, u32),
    // blocks (u32): all little-endian.
    let mut superblock = [0u8; 40];
    match file.read_exact(&mut superblock) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).with_context(context),
    }
    let u32_at = |at: usize| u32::from_le_bytes(superblock[at..at + 4].try_into().unwrap());
    let block_bits = superblock[12];
    if u32_at(0) != EROFS_SUPER_MAGIC || !EROFS_BLOCK_SIZE_BITS.contains(&block_bits) {
        return Ok(None);
    }
    Ok(Some(u64::from(u32_at(36)) << block_bits))
}

/// A block device's size: from sysfs, else by seeking to its end.
fn block_device_bytes(device: &Path) -> Result<u64> {
    use std::io::{Seek, SeekFrom};
    let sectors = device
        .file_name()
        .and_then(|name| fs::read_to_string(Path::new("/sys/class/block").join(name).join("size")).ok())
        .and_then(|size| size.trim().parse::<u64>().ok());
    if let Some(sectors) = sectors {
        return Ok(sectors * 512);
    }
    let context = || format!("reading the size of '{}'", device.display());
    fs::File::open(device)
        .with_context(context)?
        .seek(SeekFrom::End(0))
        .with_context(context)
}

/// Where the block device `device` (canonical) is mounted, if anywhere.
fn mounted_at(device: &Path) -> Result<Option<String>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let rdev = fs::metadata(device)
        .ok()
        .filter(|m| m.file_type().is_block_device())
        .map(|m| m.rdev());
    let mounts = fs::read_to_string("/proc/self/mounts").context("reading /proc/self/mounts")?;
    Ok(mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let (source, point) = (fields.next()?, fields.next()?);
        if !source.starts_with('/') {
            return None;
        }
        let same = fs::canonicalize(source).is_ok_and(|p| p == device)
            || rdev.is_some_and(|rdev| {
                fs::metadata(source)
                    .is_ok_and(|m| m.file_type().is_block_device() && m.rdev() == rdev)
            });
        // /proc/self/mounts escapes spaces as \040.
        same.then(|| point.replace("\\040", " "))
    }))
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
