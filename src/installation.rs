//! An A/B installation: its record (which partition is root A and which is
//! root B), setting it up (`init`: root A as installed, root B a copy of it,
//! both boot entries, A the default) and checking it (`verify-installation`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::process::Command;

use crate::boot::Esp;
use crate::error::{classified, refuse, ErrorClass};
use crate::filesystem::Root;
use crate::inventory::Inventory;
use crate::other::Mounted;
use crate::sync::Synchronization;
use crate::target::{partuuid, require_root, Target};
use crate::Slot;
use crate::{init, InitRequest};

/// The record's file, in recab's directory on the shared `/var`.
const RECORD: &str = "installation.json";

/// recab's installation record, as an absolute path on an installed system.
pub fn installation_file() -> String {
    format!("{}/lib/recab/{RECORD}", recpart::policy::STATE_MOUNT)
}

/// Which partition is which root: the only state recab keeps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Installation {
    pub version: u32,
    /// The PARTUUIDs of root A and root B.
    pub roots: [String; 2],
}

impl Installation {
    pub fn read(target: &Target) -> Result<Option<Self>> {
        let Some(text) = target_root::read_optional(&target.state_dir.join(RECORD))? else {
            return Ok(None);
        };
        let mut record: Self = serde_json::from_str(&text)
            .with_context(|| format!("reading '{}'", target.state_dir.join(RECORD).display()))?;
        for root in &mut record.roots {
            *root = root.to_ascii_lowercase();
        }
        Ok(Some(record))
    }

    /// The record, which every A/B command needs.
    pub fn require(target: &Target) -> Result<Self> {
        Self::read(target)?.ok_or_else(|| {
            refuse(format!(
                "'{}' is missing: this is not an A/B installation",
                target.state_dir.join(RECORD).display()
            ))
        })
    }

    fn save(&self, target: &Target) -> Result<()> {
        target_root::write_durable(
            &target.state_dir.join(RECORD),
            &serde_json::to_vec_pretty(self)?,
            Some(0o644),
        )?;
        Ok(())
    }

    pub fn uuid(&self, slot: Slot) -> &str {
        &self.roots[slot as usize]
    }

    /// The root whose partition is `uuid`.
    pub fn slot_of(&self, uuid: &str) -> Result<Slot> {
        Slot::ALL
            .into_iter()
            .find(|slot| self.uuid(*slot).eq_ignore_ascii_case(uuid))
            .ok_or_else(|| {
                refuse(format!(
                    "the booted root (PARTUUID {uuid}) is neither root A nor root B of this installation"
                ))
            })
    }
}

/// Copy the shared homes onto `/var` once, before either root can boot.
/// Publishing a complete copy by rename keeps a power loss from leaving
/// half a home.
fn homes(target: &Target) -> Result<()> {
    let source = Root::open(&target.root)?;
    let state = Root::open(&target.root.join("var"))?;
    for (from, to) in [("root", "roothome"), ("home", "home")] {
        let meta = source
            .metadata(Path::new(from))?
            .context("installed home path is missing")?;
        if meta.file_type().is_symlink() && from == "home" {
            continue;
        }
        if !meta.is_dir() {
            return Err(refuse(
                "an installed home must be a directory or a shared home link",
            ));
        }
        if let Some(m) = state.metadata(Path::new(to))? {
            if !m.is_dir() {
                return Err(refuse("a shared home destination is not a real directory"));
            }
            if !source.names(Path::new(from))?.is_empty() && to == "home" {
                return Err(refuse(
                    "a populated /home must use /var/home before initialization",
                ));
            }
            continue;
        }
        let input = source.entry(Path::new(from), false)?;
        let scratch = state.entry(Path::new(&format!(".recab-install-{to}")), false)?;
        scratch.remove()?;
        target_root::run(
            Command::new("cp")
                .arg("-a")
                .arg(input.child_path())
                .arg(scratch.child_path()),
        )?;
        crate::filesystem::flush(&state)?;
        fs::rename(scratch.path(), state.entry(Path::new(to), false)?.path())?;
        crate::filesystem::flush(&state)?;
    }
    Ok(())
}

/// Set up an A/B installation: root A is the system mounted at
/// `request.root`, root B (partition `b`) becomes a copy of it.
pub(crate) fn run(target: Target, request: &InitRequest, a: &Path, b: &Path) -> Result<()> {
    let roots = [partuuid(a)?, partuuid(b)?];
    if roots[0] == roots[1] {
        return Err(refuse("root A and root B must be different partitions"));
    }
    init::check_mounts(&target, &request.layout, &roots[0])?;
    let table = target.mount_table()?;
    let mounted = table
        .iter()
        .rev()
        .find(|m| m.mount_point == target.root)
        .context("the installation root is not mounted")?;
    let device_a = fs::canonicalize(a)?;
    if mounted.fstype != "ext4"
        || !fs::metadata(&device_a)?.file_type().is_block_device()
        || fs::metadata(&device_a)?.rdev() != fs::metadata(&target.root)?.dev()
        || fs::metadata("/")?.dev() == fs::metadata(&device_a)?.rdev()
    {
        return Err(refuse(
            "initialization needs a separate installed ext4 root, never the running system",
        ));
    }
    let state = crate::immutable::state_partition(&target)?;
    let esp = Esp::new(&target)?;
    let esp_uuid = esp.partition()?;
    if roots.contains(&esp_uuid) || roots.contains(&state) || esp_uuid == state {
        return Err(refuse(
            "root A, root B, /var and the ESP must be different partitions",
        ));
    }
    if let Some(existing) = Installation::read(&target)? {
        if existing.roots != roots {
            return Err(refuse(
                "this /var already records an installation with other root partitions",
            ));
        }
    }
    let initrd = request.initrd.as_deref().ok_or_else(|| {
        refuse("an A/B installation boots through its installed initramfs (--initrd)")
    })?;
    let device_b = fs::canonicalize(b)?;
    if !fs::metadata(&device_b)?.file_type().is_block_device() {
        return Err(refuse(format!("'{}' is not a block device", b.display())));
    }
    if let Some((point, _)) = crate::other::mounted(&device_b)? {
        return Err(refuse(format!(
            "root B ('{}') is mounted at '{}'; unmount it",
            device_b.display(),
            point.display()
        )));
    }

    Installation {
        version: 2,
        roots: roots.clone(),
    }
    .save(&target)?;
    // A retried initialization finds root A read-only from the last attempt.
    if mounted.options.split(',').any(|o| o == "ro") {
        crate::other::remount_writable(&target.root)?;
    }
    homes(&target)?;
    crate::immutable::prepare(&target.root, Slot::A, &state, &roots[1])?;
    // Root A must not change while it is copied.
    crate::filesystem::flush(&Root::open(&target.root)?)?;
    target_root::run(
        Command::new("mount")
            .args(["-o", "remount,ro"])
            .arg(&target.root),
    )
    .context("making root A read-only for the copy")?;

    eprintln!("recab: copying root A to root B ({})", device_b.display());
    let point = Path::new(crate::other::OTHER_MOUNT).with_file_name("install-b");
    let destination = match Mounted::new(&device_b, &point, "rw,nosuid,nodev") {
        Ok(mounted) => mounted,
        Err(_) => {
            crate::other::make_filesystem(&device_b)?;
            Mounted::new(&device_b, &point, "rw,nosuid,nodev")?
        }
    };
    let desired = Inventory::capture(&target.root)?;
    let sync = Synchronization::check(
        &target.root,
        desired,
        &destination.point,
        Slot::B,
        &roots[1],
        &roots[0],
    )?;
    sync.apply()?;
    sync.verify(&destination.point)?;
    drop(sync);
    drop(destination);

    let result = (|| -> Result<()> {
        esp.install_loader()?;
        for slot in Slot::ALL {
            esp.write_slot(
                Some(slot),
                &roots[slot as usize],
                &request.kernel,
                Some(initrd),
                &request.options,
            )?;
        }
        let default = esp.id(Slot::A);
        esp.write_loader_default(&default)?;
        init::point_firmware(&default)
    })();
    result.map_err(|e| {
        classified(
            ErrorClass::PartialUpdate,
            format!("the boot entries are incomplete; run the same recab init again: {e:#}"),
        )
    })
}

/// What `verify-installation` found: both roots and their boot entries.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedInstallation {
    pub schema_version: u32,
    pub complete: bool,
    pub slots: Vec<Slot>,
}

/// The slot marker's root and its other root's PARTUUID.
fn marker(root: &Path) -> Result<(String, String)> {
    let text = Root::open(root)?
        .optional_text(Path::new(recinit::installed::IMMUTABLE_CONFIG))?
        .ok_or_else(|| refuse(format!("'{}' has no slot marker", root.display())))?;
    let fields = text.split_whitespace().collect::<Vec<_>>();
    match fields.as_slice() {
        [slot, _, other] => Ok((
            slot.to_string(),
            other.trim_start_matches("PARTUUID=").to_ascii_lowercase(),
        )),
        _ => Err(refuse(format!(
            "'{}' has an invalid slot marker",
            root.display()
        ))),
    }
}

/// Check the installation under `root` before the installer finishes: the
/// record, both entries and their boot files, A the default, and each
/// root's slot marker. Reads only.
pub fn verify(root: &Path) -> Result<VerifiedInstallation> {
    require_root("verify-installation")?;
    let target = Target::installed(root)?;
    let _lock = target.lock()?;
    let record = Installation::require(&target)?;
    let esp = Esp::new(&target)?;
    if esp.booted_root()? != record.uuid(Slot::A) {
        return Err(refuse(
            "the root mounted at the installation target is not root A",
        ));
    }
    let mut boot_files = Vec::new();
    for slot in Slot::ALL {
        let entry = esp
            .entry(Some(slot))?
            .ok_or_else(|| refuse(format!("root {slot} has no boot entry")))?;
        if entry.root_partuuid().as_deref() != Some(record.uuid(slot)) {
            return Err(refuse(format!(
                "root {slot}'s entry boots another partition"
            )));
        }
        if let Some(missing) = esp.missing_boot_file(slot)? {
            return Err(refuse(format!(
                "root {slot}'s entry loads '{missing}', which is missing"
            )));
        }
        let (kernel, initrd) = entry.kernel_and_initrd();
        let read = |file: Option<String>| -> Result<Option<Vec<u8>>> {
            file.map(|f| fs::read(target.esp.join(f)).map_err(Into::into))
                .transpose()
        };
        boot_files.push((read(kernel)?, read(initrd)?));
    }
    if boot_files[0] != boot_files[1] {
        return Err(refuse(
            "root A and root B boot different kernels or initramfs images",
        ));
    }
    if esp.default_slot()? != Some(Slot::A) {
        return Err(refuse("loader.conf does not boot root A by default"));
    }
    let check = |path: &Path, slot: Slot| -> Result<()> {
        let (marked, other) = marker(path)?;
        if marked != slot.to_string() || other != record.uuid(slot.other()) {
            return Err(refuse(format!(
                "root {slot}'s slot marker names the wrong roots"
            )));
        }
        Ok(())
    };
    check(root, Slot::A)?;
    crate::filesystem::private_mounts()?;
    let device_b = crate::other::device(record.uuid(Slot::B))?;
    let point = Path::new(crate::other::OTHER_MOUNT).with_file_name("verify-b");
    let mounted = Mounted::new(&device_b, &point, "ro,noload,nosuid,nodev,noexec")?;
    check(&mounted.point, Slot::B)?;
    Ok(VerifiedInstallation {
        schema_version: 2,
        complete: true,
        slots: Slot::ALL.to_vec(),
    })
}
