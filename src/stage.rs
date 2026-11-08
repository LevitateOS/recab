//! `recab stage`: write a prebuilt system into the inactive slot.
//!
//! The slot's partition is the one its boot entry's `root=` names (written
//! at install time). Staging makes the slot unbootable first (its kernel
//! goes), then formats and fills the partition, and installs the kernel and
//! entry last, so an interrupted stage never leaves a half-written slot
//! bootable.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::carried::{self, Carry};
use crate::systemd_boot::{Boot, SystemdBoot};
use crate::state::Slot;
use crate::error::refuse;
use distro_profile::{Flavour, Profile};
use recinit::live::{INITRAMFS_INSTALLED_ISO_PATH, KERNEL_ISO_PATH, ROOTFS_ISO_PATH};
use recpart::types::Role;
use target_root::ErofsImage;

/// The prebuilt system `stage` writes: files, or a release medium holding them.
#[derive(Debug, Clone)]
pub enum StageSource {
    Files(StageRequest),
    /// A release medium (its device, or where it is mounted): the rootfs,
    /// kernel and, when it carries one, the installed initramfs at the live
    /// protocol's paths.
    Media(PathBuf),
}

/// The prebuilt system `stage` writes, as files.
#[derive(Debug, Clone)]
pub struct StageRequest {
    /// EROFS rootfs image (a file or a partition), extracted with recstrap.
    pub rootfs: PathBuf,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
}

/// The slot's role in the disk layout, which gives its filesystem and label.
fn slot_role(slot: Slot) -> Role {
    match slot {
        Slot::A => Role::RootA,
        Slot::B => Role::RootB,
    }
}

/// A release medium's system: the rootfs, kernel and, when the medium carries
/// one, the installed initramfs, at the live protocol's paths. A block device
/// is mounted read-only under the stage directory, and stays mounted while
/// the returned guard lives; a directory is taken as the mounted medium.
pub(crate) fn open_media(config: &Config, media: &Path) -> Result<(Option<Mounted>, StageRequest)> {
    use std::os::unix::fs::FileTypeExt;
    let meta = fs::metadata(media).map_err(|err| refuse(format!("release medium '{}': {err}", media.display())))?;
    let (mounted, root) = if meta.is_dir() {
        (None, media.to_path_buf())
    } else if meta.file_type().is_block_device() {
        let point = config.stage_dir.join("media");
        fs::create_dir_all(&point).with_context(|| format!("creating '{}'", point.display()))?;
        (Some(Mounted::mount_ro(media, &point)?), point)
    } else {
        return Err(refuse(format!(
            "release medium '{}' is neither a block device nor a directory",
            media.display()
        )));
    };
    let initrd = root.join(INITRAMFS_INSTALLED_ISO_PATH);
    let request = StageRequest {
        rootfs: root.join(ROOTFS_ISO_PATH),
        kernel: root.join(KERNEL_ISO_PATH),
        initrd: target_root::exists(&initrd)?.then_some(initrd),
    };
    Ok((mounted, request))
}

/// The inputs a stage reads, checked before anything is changed: the
/// rootfs image, canonical.
pub(crate) struct Inputs {
    rootfs: ErofsImage,
}

/// Refuse, before anything is changed, an image of another flavour than the
/// running system's: the base system staged onto a desktop install would
/// turn it back into the base system, and the reverse. `change_flavour`
/// stages it anyway. Each flavour is its os-release's `REC_FLAVOUR`; the
/// image's is read from the image, mounted read-only under the stage
/// directory.
pub(crate) fn check_flavour(config: &Config, inputs: &Inputs, change_flavour: bool) -> Result<()> {
    if change_flavour {
        return Ok(());
    }
    let running = fs::read_to_string(&config.os_release)
        .with_context(|| format!("reading '{}'", config.os_release.display()))?;
    let running = Flavour::from_os_release(&running)
        .map_err(|err| refuse(format!("the running system's {}: {err}", config.os_release.display())))?;
    let image = &inputs.rootfs.path;
    let point = config.stage_dir.join("image");
    fs::create_dir_all(&point).with_context(|| format!("creating '{}'", point.display()))?;
    let mounted = Mounted::mount_image_ro(image, inputs.rootfs.rdev.is_some(), &point)?;
    let text = target_root::os_release(&point);
    mounted.unmount()?;
    let text = text?.ok_or_else(|| refuse(format!("rootfs image '{}' has no os-release", image.display())))?;
    let staged = Flavour::from_os_release(&text)
        .map_err(|err| refuse(format!("rootfs image '{}': {err}", image.display())))?;
    if staged != running {
        return Err(refuse(format!(
            "rootfs image '{}' is the {staged} system and this machine runs the {running} system: staging it \
would turn this machine into the {staged} system. Stage a {running} image (the rootfs of a {running} ISO), \
or pass --change-flavour to change this machine to the {staged} system",
            image.display()
        )));
    }
    Ok(())
}

/// The partition a stage will write, checked before anything is changed.
pub(crate) struct Target {
    device: PathBuf,
    /// Its PARTUUID, which the new system's fstab names its root by.
    partuuid: String,
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
            if !target_root::metadata(path)?.is_some_and(|meta| meta.is_file()) {
                return Err(refuse(format!("{what} '{}' is not a file", path.display())));
            }
        }
    }
    // Stage removes this directory before it installs the kernel and initrd.
    let slot_dir = backend.slot_boot_files_dir(boot, slot);
    if let Some(slot_dir) = target_root::canonical(&slot_dir)? {
        for (what, path) in [
            ("rootfs", Some(rootfs)),
            ("kernel", Some(&request.kernel)),
            ("initrd", request.initrd.as_ref()),
        ] {
            let Some(path) = path else { continue };
            if target_root::canonical(path)?.is_some_and(|p| p.starts_with(&slot_dir)) {
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

    let role = slot_role(slot);
    let label = role.label().expect("slot roles have labels");
    target_root::run(
        Command::new(format!("mkfs.{}", role.filesystem().as_str()))
            .args(["-F", "-q", "-L", label])
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
    // The new image's list: an image can add or drop what it carries.
    let list = carried::read(&mount_point)?;
    // What the image ships in the directories it carries, before the carry
    // changes them: the next update tells the machine's changes from it.
    write_baseline(&mount_point, &list)?;
    carry_configuration(running_root, &mount_point, &list)?;
    write_fstab(
        &running_root.join("etc/fstab"),
        &mount_point.join("etc/fstab"),
        &target.partuuid,
        role.filesystem().as_str(),
    )?;
    sync_filesystem(&mount_point)?;
    mounted.unmount()?;

    backend.install_slot_boot_files(
        boot,
        slot,
        &request.kernel,
        request.initrd.as_deref(),
        init,
    )
}


/// Carry the machine state the new image lists from the running root into
/// the new one. A source that cannot be read fails the stage.
fn carry_configuration(from: &Path, to: &Path, list: &[Carry]) -> Result<()> {
    // A running slot staged before baselines existed has none: its
    // directories are carried whole, once.
    let baseline = read_baseline(from)?;
    for entry in list {
        let rel = entry.path();
        let source = from.join(rel);
        match entry {
            Carry::File(_) | Carry::Dir(_) => {
                let meta = target_root::metadata(&source)?;
                if matches!(entry, Carry::Dir(_)) && meta.as_ref().is_some_and(|m| !m.is_dir()) {
                    bail!("'{}' is listed as a directory to carry but is not one", source.display());
                }
                // A directory the running slot's baseline covers carries
                // only the machine's changes to it.
                if matches!(entry, Carry::Dir(_)) && baseline.get(rel).is_some_and(|s| s == "d") {
                    copy_changed(from, to, rel, &baseline)?;
                    remove_whiteouts(from, to, rel, &baseline)?;
                } else if meta.is_some() {
                    copy_tree(&source, &to.join(rel))?;
                }
            }
            Carry::Prefix(_) => {
                let (dir, prefix) = rel.rsplit_once('/').unwrap_or(("", rel));
                for name in target_root::dir_names(&from.join(dir))? {
                    if name.to_string_lossy().starts_with(prefix) {
                        copy_tree(&from.join(dir).join(&name), &to.join(dir).join(&name))?;
                    }
                }
            }
            Carry::Accounts(_) => merge_accounts(&source, &to.join(rel))?,
        }
    }
    Ok(())
}

/// Where `stage` records what the image shipped in the carried directories,
/// relative to the slot's root. One line per entry, tab separated: `d<TAB>path`
/// for a directory, `l<TAB>path<TAB>target` for a symlink and
/// `f<TAB>path<TAB>sha256` for a file.
const BASELINE: &str = "usr/lib/recab/baseline";

/// What an entry is, as the baseline records it: `d`, `l<TAB>target` or
/// `f<TAB>sha256`.
fn signature(path: &Path) -> Result<String> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("reading '{}'", path.display()))?;
    Ok(if meta.is_dir() {
        "d".to_string()
    } else if meta.file_type().is_symlink() {
        format!("l\t{}", fs::read_link(path)?.display())
    } else {
        let content = fs::read(path).with_context(|| format!("reading '{}'", path.display()))?;
        format!("f\t{:x}", Sha256::digest(content))
    })
}

/// Every entry under `root/rel` (itself included) as (path, signature).
fn walk(root: &Path, rel: &str, out: &mut Vec<(String, String)>) -> Result<()> {
    let path = root.join(rel);
    let sig = signature(&path)?;
    let is_dir = sig == "d";
    out.push((rel.to_string(), sig));
    if is_dir {
        let mut names = target_root::dir_names(&path)?;
        names.sort();
        for name in names {
            walk(root, &format!("{rel}/{}", name.to_string_lossy()), out)?;
        }
    }
    Ok(())
}

/// Record the tree of every carried directory under `root`, as the image
/// shipped it.
fn write_baseline(root: &Path, list: &[Carry]) -> Result<()> {
    let mut entries = Vec::new();
    for entry in list {
        if let Carry::Dir(rel) = entry {
            if target_root::metadata(&root.join(rel))?.is_some_and(|m| m.is_dir()) {
                walk(root, rel, &mut entries)?;
            }
        }
    }
    let text: String = entries
        .iter()
        .map(|(path, sig)| match sig.split_once('\t') {
            Some((kind, value)) => format!("{kind}\t{path}\t{value}\n"),
            None => format!("{sig}\t{path}\n"),
        })
        .collect();
    Ok(target_root::write_durable(&root.join(BASELINE), text.as_bytes(), None)?)
}

/// The baseline of the slot under `root` as path -> signature; empty when it
/// has none.
fn read_baseline(root: &Path) -> Result<BTreeMap<String, String>> {
    let path = root.join(BASELINE);
    let Some(text) = target_root::read_optional(&path)? else { return Ok(BTreeMap::new()) };
    text.lines()
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            match (fields.next(), fields.next(), fields.next()) {
                (Some("d"), Some(path), None) => Ok((path.to_string(), "d".to_string())),
                (Some(kind @ ("l" | "f")), Some(path), Some(value)) => {
                    Ok((path.to_string(), format!("{kind}\t{value}")))
                }
                _ => bail!("'{}': '{line}' is not a baseline entry", path.display()),
            }
        })
        .collect()
}

/// Copy the machine's entries of `rel` that the baseline lacks or records
/// differently over the new image's; the rest stays as the image ships it.
fn copy_changed(from: &Path, to: &Path, rel: &str, baseline: &BTreeMap<String, String>) -> Result<()> {
    for name in target_root::dir_names(&from.join(rel))? {
        let child = format!("{rel}/{}", name.to_string_lossy());
        let sig = signature(&from.join(&child))?;
        if sig == "d" && baseline.get(&child).is_some_and(|s| s == "d") {
            copy_changed(from, to, &child, baseline)?;
        } else if baseline.get(&child) != Some(&sig) {
            copy_tree(&from.join(&child), &to.join(&child))?;
        }
    }
    Ok(())
}

/// Delete from the new slot what the baseline has under `rel` and the
/// machine removed.
fn remove_whiteouts(from: &Path, to: &Path, rel: &str, baseline: &BTreeMap<String, String>) -> Result<()> {
    let prefix = format!("{rel}/");
    for path in baseline.keys().filter(|p| p.starts_with(&prefix)) {
        if target_root::metadata(&from.join(path))?.is_some() {
            continue;
        }
        let gone = to.join(path);
        match target_root::metadata(&gone)? {
            Some(m) if m.is_dir() => fs::remove_dir_all(&gone),
            Some(_) => fs::remove_file(&gone),
            None => Ok(()),
        }
        .with_context(|| format!("removing '{}'", gone.display()))?;
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
    let existing = target_root::metadata(dest).with_context(context)?;
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
fn write_fstab(running: &Path, dest: &Path, partuuid: &str, filesystem: &str) -> Result<()> {
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
                *fstype = filesystem;
            }
            fields.join("\t")
        })
        .collect();
    if !found_root {
        bail!("'{}' has no entry for /", running.display());
    }
    target_root::write_durable(dest, (lines.join("\n") + "\n").as_bytes(), None)
        .with_context(|| format!("writing '{}'", dest.display()))
}

/// A filesystem mounted for the stage, unmounted when dropped.
pub(crate) struct Mounted {
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

    /// An EROFS image, read-only: a partition holding it, or an image file
    /// through a loop device.
    fn mount_image_ro(image: &Path, block_device: bool, point: &Path) -> Result<Self> {
        target_root::run(
            Command::new("mount")
                .args(["-t", "erofs", "-o", if block_device { "ro" } else { "ro,loop" }])
                .arg(image)
                .arg(point),
        )?;
        Ok(Self {
            point: point.to_path_buf(),
            active: true,
        })
    }

    fn mount_ro(device: &Path, point: &Path) -> Result<Self> {
        target_root::run(Command::new("mount").args(["-o", "ro"]).arg(device).arg(point))?;
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
