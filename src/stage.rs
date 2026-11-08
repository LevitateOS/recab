//! `recab stage`: write a prebuilt system into the inactive slot.
//!
//! The slot's partition is the one its boot entry's `root=` names (written
//! at install time). Staging makes the slot unbootable first (its kernel
//! goes), then formats and fills the partition, and installs the kernel and
//! entry last, so an interrupted stage never leaves a half-written slot
//! bootable.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::init_system::{self, InitSystemExt};
use crate::store::write_durable;
use crate::systemd_boot::{Boot, SystemdBoot};
use crate::{tagged_err, ErrorClass, Slot, StageRequest};
use distro_profile::Profile;
use target_root::ErofsImage;

/// Machine configuration every new system keeps from the running one:
/// identity, locale and time. Everything else under /etc comes from the new
/// image, except the accounts ([`ACCOUNT_FILES`]) and what the init system
/// keeps ([`init_system::carried`]).
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

/// The inputs a stage reads, checked before anything is changed: the
/// rootfs image, canonical.
pub(crate) struct Inputs {
    rootfs: ErofsImage,
}

/// The partition a stage will write, checked before anything is changed.
pub(crate) struct Target {
    device: PathBuf,
    /// Its PARTUUID, which the new system's fstab names its root by.
    partuuid: String,
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
    let rootfs = &request.rootfs;
    // The rootfs may be an image file or a partition holding the image.
    let image = target_root::erofs_image(rootfs)
        .map_err(|err| refuse(format!("rootfs image: {err}")))?
        .map_err(|why| {
            refuse(format!(
                "rootfs '{}' is not an EROFS image ({why}); pass the system's .erofs image, or the partition holding it",
                rootfs.display()
            ))
        })?;
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
    if let Some(slot_dir) = canonical(&slot_dir)? {
        for (what, path) in [
            ("rootfs", Some(rootfs)),
            ("kernel", Some(&request.kernel)),
            ("initrd", request.initrd.as_ref()),
        ] {
            let Some(path) = path else { continue };
            if canonical(path)?.is_some_and(|p| p.starts_with(&slot_dir)) {
                return Err(refuse(format!(
                    "{what} '{}' is inside slot {slot}'s kernel directory '{}', which stage removes before installing the new kernel; \
copy it elsewhere first (for example to /var/tmp) and pass that path",
                    path.display(),
                    slot_dir.display()
                )));
            }
        }
    }
    Ok(Inputs { rootfs: image })
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
    let table = target_root::mount_table()?;
    if let Some(point) = target_root::mounted_at(&table, &device)? {
        return Err(refuse(format!(
            "slot partition '{}' is mounted at '{}'; unmount it before staging",
            device.display(),
            point.display()
        )));
    }
    let rootfs = &inputs.rootfs;
    if rootfs.path == device || rootfs.rdev.is_some() && rootfs.rdev == Some(target_root::block_rdev(&device)?) {
        return Err(refuse(format!(
            "rootfs '{}' is slot {slot}'s partition, which stage formats; pass the EROFS image file or the partition holding it",
            rootfs.path.display()
        )));
    }
    if rootfs.rdev.is_some() {
        let point = match target_root::mounted_at(&table, &rootfs.path)? {
            Some(point) => Some(point),
            None => {
                let root_dev = fs::metadata("/").context("reading '/'")?.dev();
                (Some(root_dev) == rootfs.rdev).then(|| PathBuf::from("/"))
            }
        };
        if let Some(point) = point {
            return Err(refuse(format!(
                "rootfs partition '{}' is mounted at '{}'; unmount it, or pass the EROFS image file",
                rootfs.path.display(),
                point.display()
            )));
        }
    }
    let slot_bytes = target_root::block_device_bytes(&device)?;
    if rootfs.bytes > slot_bytes {
        return Err(refuse(format!(
            "rootfs image '{}' ({} bytes) does not fit in slot {slot}'s partition '{}' ({slot_bytes} bytes)",
            rootfs.path.display(),
            rootfs.bytes,
            device.display()
        )));
    }
    let partuuid = target_root::partuuid(&device)?.ok_or_else(|| {
        refuse(format!(
            "'{}' has no PARTUUID (stage needs a GPT partition)",
            device.display()
        ))
    })?;
    Ok(Target { device, partuuid })
}

/// Write the slot. Its kernel goes first, so the slot cannot be booted
/// until the kernel and entry are installed at the end.
pub(crate) fn write_slot(
    config: &Config,
    backend: &SystemdBoot,
    boot: &Boot,
    slot: Slot,
    request: &StageRequest,
    target: &Target,
    profile: &Profile,
) -> Result<()> {
    let init = profile.init;
    let device = &target.device;
    let running_root = Path::new("/");

    backend.remove_slot_boot_files(boot, slot)?;

    let label = target_root::blkid_tag(device, "LABEL")?.unwrap_or_else(|| format!("ROOT_{slot}"));
    target_root::run(
        Command::new(format!("mkfs.{SLOT_FILESYSTEM}"))
            .args(["-F", "-q", "-L", &label])
            .arg(device),
    )?;

    let mount_point = config.stage_dir.join(format!("stage-{}", slot.entry_suffix()));
    fs::create_dir_all(&mount_point)
        .with_context(|| format!("creating '{}'", mount_point.display()))?;
    let mounted = Mounted::mount(device, &mount_point)?;
    target_root::run(
        Command::new(&config.recstrap)
            .args(["--quiet", "--rootfs"])
            .arg(&request.rootfs)
            .arg(&mount_point),
    )
    .context("extracting the rootfs image into the slot")?;
    carry_configuration(running_root, &mount_point, profile)?;
    if init.trial_service_enabled(running_root)? {
        init.enable_trial_service(&mount_point)
            .context("enabling the new system's packaged trial service")?;
    }
    write_fstab(
        &running_root.join("etc/fstab"),
        &mount_point.join("etc/fstab"),
        &target.partuuid,
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

/// The canonical form of `path`; `None` when it does not exist.
fn canonical(path: &Path) -> Result<Option<PathBuf>> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(Some(path)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("resolving '{}'", path.display())),
    }
}

/// Copy the machine configuration from the running root into the new one.
fn carry_configuration(from: &Path, to: &Path, profile: &Profile) -> Result<()> {
    let carried = init_system::carried(profile);
    for rel in CARRIED_FILES.iter().chain(&carried.files) {
        let source = from.join(rel);
        if target_root::exists(&source)? {
            copy_tree(&source, &to.join(rel))?;
        }
    }
    for rel in ACCOUNT_FILES {
        merge_accounts(&from.join(rel), &to.join(rel))?;
    }
    for rel in &carried.dirs {
        let source = from.join(rel);
        if source.is_dir() {
            copy_tree(&source, &to.join(rel))?;
        }
    }
    for name in target_root::dir_names(&from.join(SSH_DIR))? {
        if name.to_string_lossy().starts_with(SSH_HOST_KEY_PREFIX) {
            copy_tree(&from.join(SSH_DIR).join(&name), &to.join(SSH_DIR).join(&name))?;
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
    let existing = match fs::symlink_metadata(dest) {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(context),
    };
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
    let Some(running_content) = target_root::read_optional(running)? else {
        return Ok(());
    };
    let Some(image_content) = target_root::read_optional(image)? else {
        return copy_tree(running, image);
    };
    write_preserving_mode(
        image,
        target_root::merge_accounts(&image_content, &running_content).as_bytes(),
    )
}

/// Replace `path`'s content durably, keeping its owner and permissions.
fn write_preserving_mode(path: &Path, content: &[u8]) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).with_context(|| format!("reading '{}'", path.display()))?;
    target_root::write_durable(path, content, Some(meta.mode() & 0o7777))
        .with_context(|| format!("writing '{}'", path.display()))?;
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
        target_root::run(Command::new("mount").arg(device).arg(point))?;
        Ok(Self {
            point: point.to_path_buf(),
            active: true,
        })
    }

    fn unmount(mut self) -> Result<()> {
        self.active = false;
        target_root::run(Command::new("umount").arg(&self.point))?;
        Ok(())
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if self.active {
            if let Err(err) = target_root::run(Command::new("umount").arg(&self.point)) {
                eprintln!("recab: warning: {err:#}");
            }
        }
    }
}
