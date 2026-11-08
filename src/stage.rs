//! `recab stage`: write a prebuilt system into the inactive slot. Every
//! check runs first ([`check`]) and yields a [`StagePlan`]; only [`write`]
//! changes anything. It makes the slot unbootable (its kernel goes), formats
//! and fills the partition, carries the machine's state, and installs the
//! kernel and entry last, so an interrupted stage never leaves a half-written
//! slot bootable.

use anyhow::{Context, Result};
use distro_profile::{os_release_fact, os_release_value, Flavour, InitSystem};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::carry::{self, fstab_for_slot, merge_accounts, Baseline, Carry};
use crate::error::{classified, refuse, ErrorClass};
use crate::state::Slot;
use crate::systemd_boot::{Boot, SlotWrite, SystemdBoot};
use crate::target::Target;
use recinit::live::{INITRAMFS_INSTALLED_ISO_PATH, KERNEL_ISO_PATH, ROOTFS_ISO_PATH};
use recpart::types::Role;

/// The prebuilt system `stage` writes: files, or a release medium holding them.
#[derive(Debug, Clone)]
pub enum StageSource {
    Files(StageRequest),
    /// A release medium (its device, or where it is mounted): the rootfs,
    /// kernel and installed initramfs at the live protocol's paths.
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

/// Where the fstab is, when the image's list does not say.
const FSTAB: &str = "etc/fstab";

/// Everything a stage writes, checked: nothing has changed yet.
pub(crate) struct StagePlan {
    slot: Slot,
    device: PathBuf,
    rootfs: PathBuf,
    kernel: PathBuf,
    initrd: Option<PathBuf>,
    /// The image's carried list, and the running slot's baseline.
    list: Vec<Carry>,
    baseline: Option<Baseline>,
    /// Files written into the new slot as prepared: merged account files
    /// and the fstab, with their mode.
    prepared: Vec<(String, String, u32)>,
    /// The image's init system (the running system's).
    init: InitSystem,
    /// The release medium, mounted until the slot is written.
    _medium: Option<Mounted>,
}

/// Check a stage of the inactive `slot` from `source`: the inputs, the
/// slot's partition, the image itself (its distro, init and flavour, its
/// carried list and account files), the running fstab, and that the kernel
/// can read EROFS. A refusal here has changed nothing (exit 21).
pub(crate) fn check(
    target: &Target,
    backend: &SystemdBoot,
    boot: &Boot,
    slot: Slot,
    source: &StageSource,
    change_flavour: bool,
) -> Result<StagePlan> {
    let (medium, request) = match source {
        StageSource::Files(request) => (None, request.clone()),
        StageSource::Media(media) => open_media(target, media)?,
    };
    let rootfs = target_root::erofs_image(&request.rootfs)
        .map_err(|err| refuse(format!("rootfs image: {err}")))?
        .map_err(|why| {
            refuse(format!(
                "rootfs '{}' is not an EROFS image ({why}); pass the system's .erofs image, or the partition holding it",
                request.rootfs.display()
            ))
        })?;
    let is_partition = rootfs.rdev.is_some();
    let (kernel, initrd) = check_files(backend, boot, slot, &request)?;
    let (device, partuuid) = check_partition(target, backend, boot, slot, &rootfs)?;

    let filesystems = target_root::read_optional(&target.proc_dir.join("filesystems"))?.unwrap_or_default();
    if !filesystems.split_whitespace().any(|fs| fs == "erofs") {
        return Err(refuse("the running kernel cannot read EROFS images (no erofs in /proc/filesystems)"));
    }

    // The image, read once, read-only.
    let point = target.stage_mount("image");
    fs::create_dir_all(&point).with_context(|| format!("creating '{}'", point.display()))?;
    let options = if is_partition { "ro" } else { "ro,loop" };
    let mounted = Mounted::new(target, &["-t", "erofs", "-o", options], &rootfs.path, &point)?;
    let image = read_image(target, &point, &rootfs.path, change_flavour);
    mounted.unmount()?;
    let image = image?;

    let mut prepared = Vec::new();
    for (rel, text, mode) in &image.accounts {
        let machine = target_root::read_optional(&target.root.join(rel))?;
        let merged = match machine {
            Some(machine) => account_merge(rel, text, &machine)?,
            None => text.clone(),
        };
        prepared.push((rel.clone(), merged, *mode));
    }
    let fstab = image.list.iter().find_map(|c| matches!(c, Carry::Fstab(_)).then(|| c.path().to_string()));
    let fstab = fstab.unwrap_or_else(|| FSTAB.to_string());
    let machine_fstab = fs::read_to_string(target.root.join(&fstab))
        .with_context(|| format!("reading '{}'", target.root.join(&fstab).display()))?;
    let role = slot_role(slot);
    prepared.push((fstab, fstab_for_slot(&machine_fstab, &partuuid, role.filesystem().as_str())?, 0o644));

    Ok(StagePlan {
        slot,
        device,
        rootfs: rootfs.path,
        kernel,
        initrd,
        list: image.list,
        baseline: Baseline::read(&target.root)?,
        prepared,
        init: image.init,
        _medium: medium,
    })
}

/// A release medium's system: the rootfs, kernel and installed initramfs at
/// the live protocol's paths. A block device is mounted read-only under the
/// stage directory; a directory is taken as the mounted medium.
fn open_media(target: &Target, media: &Path) -> Result<(Option<Mounted>, StageRequest)> {
    let meta = fs::metadata(media).map_err(|err| refuse(format!("release medium '{}': {err}", media.display())))?;
    let (mounted, root) = if meta.is_dir() {
        (None, media.to_path_buf())
    } else if meta.file_type().is_block_device() {
        let point = target.stage_mount("media");
        fs::create_dir_all(&point).with_context(|| format!("creating '{}'", point.display()))?;
        (Some(Mounted::new(target, &["-o", "ro"], media, &point)?), point)
    } else {
        return Err(refuse(format!("release medium '{}' is neither a block device nor a directory", media.display())));
    };
    // Every release medium carries the installed initramfs; one without it
    // is not a release medium.
    let initrd = root.join(INITRAMFS_INSTALLED_ISO_PATH);
    if !target_root::exists(&initrd)? {
        return Err(refuse(format!("release medium '{}' has no installed initramfs ('{}')", media.display(), initrd.display())));
    }
    let request =
        StageRequest { rootfs: root.join(ROOTFS_ISO_PATH), kernel: root.join(KERNEL_ISO_PATH), initrd: Some(initrd) };
    Ok((mounted, request))
}

/// The kernel and initrd, canonical: files, and none of them (nor the
/// rootfs) inside the slot's kernel directory, which the write removes.
fn check_files(backend: &SystemdBoot, boot: &Boot, slot: Slot, request: &StageRequest) -> Result<(PathBuf, Option<PathBuf>)> {
    let inputs = [("rootfs", Some(&request.rootfs)), ("kernel", Some(&request.kernel)), ("initrd", request.initrd.as_ref())];
    let slot_dir = target_root::canonical(&backend.kernel_dir(boot, slot))?;
    let mut canonical = Vec::new();
    for (what, path) in inputs {
        let Some(path) = path else { continue };
        if what != "rootfs" && !target_root::metadata(path)?.is_some_and(|meta| meta.is_file()) {
            return Err(refuse(format!("{what} '{}' is not a file", path.display())));
        }
        let resolved = target_root::canonical(path)?.unwrap_or_else(|| path.clone());
        if slot_dir.as_ref().is_some_and(|dir| resolved.starts_with(dir)) {
            return Err(refuse(format!(
                "{what} '{}' is inside slot {slot}'s kernel directory, which stage removes before installing the new kernel; \
copy it elsewhere first (for example to /var/tmp) and pass that path",
                path.display()
            )));
        }
        canonical.push((what, resolved));
    }
    let find = |name: &str| canonical.iter().find(|(what, _)| *what == name).map(|(_, p)| p.clone());
    Ok((find("kernel").expect("checked above"), find("initrd")))
}

/// The slot's partition, by the PARTUUID its entry names: present, not the
/// running root, not mounted, not the rootfs itself, and large enough for
/// the image; a rootfs partition must not be mounted either.
fn check_partition(
    target: &Target,
    backend: &SystemdBoot,
    boot: &Boot,
    slot: Slot,
    rootfs: &target_root::ErofsImage,
) -> Result<(PathBuf, String)> {
    let partuuid = backend.slot_partuuid(boot, slot)?;
    if partuuid == backend.running_root()? {
        return Err(refuse(format!("slot {slot}'s entry names the running root (PARTUUID={partuuid}); refusing to overwrite it")));
    }
    let device = target_root::device_with_tag("PARTUUID", &partuuid)?
        .ok_or_else(|| refuse(format!("slot {slot}'s entry names root PARTUUID={partuuid}, which is not present")))?;
    let device = target_root::canonical(&device)?.unwrap_or(device);
    let table = target.mount_table()?;
    if let Some(point) = target_root::mounted_at(&table, &device)? {
        return Err(refuse(format!(
            "slot partition '{}' is mounted at '{}'; unmount it before staging",
            device.display(),
            point.display()
        )));
    }
    if rootfs.path == device || rootfs.rdev.is_some() && rootfs.rdev == Some(target_root::block_rdev(&device)?) {
        return Err(refuse(format!(
            "rootfs '{}' is slot {slot}'s partition, which stage formats; pass the EROFS image file or the partition holding it",
            rootfs.path.display()
        )));
    }
    if rootfs.rdev.is_some() {
        let root_dev = fs::metadata(&target.root).with_context(|| format!("reading '{}'", target.root.display()))?.dev();
        let point = match target_root::mounted_at(&table, &rootfs.path)? {
            Some(point) => Some(point),
            None => (Some(root_dev) == rootfs.rdev).then(|| target.root.clone()),
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
    Ok((device, partuuid))
}

/// What a stage needs from the image itself.
struct Image {
    init: InitSystem,
    list: Vec<Carry>,
    /// Each account file the list names: path, the image's text, its mode.
    accounts: Vec<(String, String, u32)>,
}

/// Read the image mounted at `point`: it must be of the running system's
/// distro and init (and flavour, unless `change_flavour`); its carried list
/// and account files.
fn read_image(target: &Target, point: &Path, image: &Path, change_flavour: bool) -> Result<Image> {
    let text = target_root::os_release(point)?
        .ok_or_else(|| refuse(format!("rootfs image '{}' has no os-release", image.display())))?;
    let id = os_release_value(&text, "ID").unwrap_or_default().to_ascii_lowercase();
    let running_id = target.entry_prefix()?;
    if id != running_id {
        return Err(refuse(format!(
            "rootfs image '{}' is a '{id}' system and this machine runs '{running_id}'; stage only updates a system with its own distro's images",
            image.display()
        )));
    }
    let init: InitSystem = os_release_fact(&text, "REC_INIT").map_err(|err| refuse(format!("rootfs image '{}': {err}", image.display())))?;
    let running_init = target.init()?;
    if init != running_init {
        return Err(refuse(format!(
            "rootfs image '{}' runs {init} and this machine runs {running_init}; stage only updates a system with images of its own init",
            image.display()
        )));
    }
    if !change_flavour {
        let running = target.flavour()?;
        let staged = Flavour::from_os_release(&text).map_err(|err| refuse(format!("rootfs image '{}': {err}", image.display())))?;
        if staged != running {
            return Err(refuse(format!(
                "rootfs image '{}' is the {staged} system and this machine runs the {running} system: staging it \
would turn this machine into the {staged} system. Stage a {running} image (the rootfs of a {running} ISO), \
or pass --change-flavour to change this machine to the {staged} system",
                image.display()
            )));
        }
    }
    let list = carry::read_list(point)?;
    let mut accounts = Vec::new();
    for entry in &list {
        if let Carry::Accounts(rel) = entry {
            let path = point.join(rel);
            if let Some(meta) = target_root::metadata(&path)? {
                let text = fs::read_to_string(&path).with_context(|| format!("reading '{}'", path.display()))?;
                accounts.push((rel.clone(), text, meta.mode() & 0o7777));
            }
        }
    }
    Ok(Image { init, list, accounts })
}

/// Merge an account file by its kind: passwd and group by name with their
/// ids checked, group and gshadow with their members joined.
fn account_merge(rel: &str, image: &str, machine: &str) -> Result<String> {
    let (id, members) = match Path::new(rel).file_name().and_then(|n| n.to_str()) {
        Some("passwd") => (Some(2), None),
        Some("group") => (Some(2), Some(3)),
        Some("gshadow") => (None, Some(3)),
        _ => (None, None),
    };
    merge_accounts(image, machine, id, members)
}

/// The slot's role in the disk layout, which gives its filesystem and label.
fn slot_role(slot: Slot) -> Role {
    let letter = slot.to_string().chars().next().expect("a slot is one letter");
    Role::slot(letter).expect("A and B are slot roles")
}

/// Write the slot as `plan` checked it. From here its old system is gone:
/// a failure leaves the slot without a kernel (it cannot boot) and says so
/// (exit 22).
pub(crate) fn write(target: &Target, plan: StagePlan, write: SlotWrite) -> Result<()> {
    let slot = plan.slot;
    fill(target, &plan).map_err(|err| {
        classified(
            ErrorClass::PartialUpdate,
            format!("slot {slot} was not written: its old system is gone and it cannot boot until a stage succeeds: {err:#}"),
        )
    })?;
    write
        .finish(&plan.kernel, plan.initrd.as_deref(), plan.init)
        .map_err(|err| classified(ErrorClass::PartialUpdate, format!("slot {slot}'s kernel and entry were not installed: {err:#}")))
}

fn fill(target: &Target, plan: &StagePlan) -> Result<()> {
    let role = slot_role(plan.slot);
    let (program, mut args) = role.filesystem().mkfs(role.label());
    args.push(plan.device.display().to_string());
    target_root::run(Command::new(program).args(&args))?;

    let point = target.stage_mount(&format!("slot-{}", plan.slot.to_string().to_ascii_lowercase()));
    fs::create_dir_all(&point).with_context(|| format!("creating '{}'", point.display()))?;
    let mounted = Mounted::new(target, &[], &plan.device, &point)?;
    target_root::run(Command::new(&target.programs.recstrap).args(["--quiet", "--rootfs"]).arg(&plan.rootfs).arg(&point))
        .context("extracting the rootfs image into the slot")?;
    // What the new image ships in the carried paths, before the carry
    // changes them: the next update tells the machine's changes from it.
    Baseline::write(&point, &plan.list)?;
    carry::carry(&target.root, &point, &plan.list, plan.baseline.as_ref())?;
    for (rel, text, mode) in &plan.prepared {
        let path = point.join(rel);
        let meta = target_root::metadata(&path)?;
        fs::write(&path, text).with_context(|| format!("writing '{}'", path.display()))?;
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(*mode))?;
        if let Some(meta) = meta {
            std::os::unix::fs::chown(&path, Some(meta.uid()), Some(meta.gid()))?;
        }
    }
    sync_filesystem(&point)?;
    mounted.unmount()
}

/// Flush the filesystem holding `path` to disk.
fn sync_filesystem(path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let dir = fs::File::open(path).with_context(|| format!("opening '{}'", path.display()))?;
    // SAFETY: syncfs on a file descriptor this function owns.
    if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("syncing '{}'", path.display()));
    }
    Ok(())
}

/// A filesystem mounted for the stage, unmounted when dropped.
pub(crate) struct Mounted {
    point: PathBuf,
    umount: String,
    active: bool,
}

impl Mounted {
    /// `source` mounted at `point` with `mount`'s `options`.
    fn new(target: &Target, options: &[&str], source: &Path, point: &Path) -> Result<Self> {
        target_root::run(Command::new(&target.programs.mount).args(options).arg(source).arg(point))?;
        Ok(Self { point: point.to_path_buf(), umount: target.programs.umount.clone(), active: true })
    }

    fn unmount(mut self) -> Result<()> {
        self.active = false;
        target_root::run(Command::new(&self.umount).arg(&self.point))?;
        Ok(())
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if self.active {
            if let Err(err) = target_root::run(Command::new(&self.umount).arg(&self.point)) {
                eprintln!("recab: warning: {err:#}");
            }
        }
    }
}
