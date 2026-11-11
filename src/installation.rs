//! Initial A/B publication. The frozen personalized source survives retries;
//! the normal continuation synchronizer owns copying and metadata fidelity.
use crate::error::{classified, refuse, ErrorClass};
use crate::filesystem::Root;
use crate::state::{Operation, PersistentState, Readiness, Slot, SlotState};
use crate::store::Store;
use crate::systemd_boot::SystemdBoot;
use crate::target::{partuuid, Target};
use crate::{BootSeal, Generation, InitRequest, SyncReport};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::{
    fd::AsRawFd,
    unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
};
use std::{fs, path::Path, process::Command};
const RECORD: &str = "installation.json";

#[derive(Serialize, Deserialize)]
struct Installation {
    version: u32,
    roots: [String; 2],
    state: String,
    esp: String,
    options: String,
    generation: Option<Generation>,
    seal: Option<BootSeal>,
    synchronization: Option<SyncReport>,
    complete: bool,
}
fn read(target: &Target) -> Result<Option<Installation>> {
    let dir = Root::open(&target.state_dir);
    let dir = match dir {
        Err(e) if crate::filesystem::missing(&e) => return Ok(None),
        v => v?,
    };
    dir.optional_text(Path::new(RECORD))?
        .map(|text| serde_json::from_str(&text).map_err(Into::into))
        .transpose()
}
fn save(target: &Target, record: &Installation) -> Result<()> {
    let root = Root::open(&target.root.join("var"))?;
    let dest = root.entry(Path::new("lib/recab/installation.json"), true)?;
    target_root::write_durable(&dest.path(), &serde_json::to_vec(record)?, Some(0o644))?;
    Ok(())
}
pub(crate) fn require_complete(target: &Target) -> Result<()> {
    if read(target)?.is_some_and(|r| r.version != 1 || !r.complete) {
        return Err(refuse("initial A/B installation is incomplete; resume recab init on the mounted installation target"));
    }
    Ok(())
}
fn expected(record: &Installation) -> PersistentState {
    PersistentState {
        slots: Slot::ALL
            .into_iter()
            .map(|slot| SlotState {
                slot,
                partuuid: record.roots[slot as usize].clone(),
                generation: record.generation.clone(),
                seal: record.seal.clone(),
                readiness: if slot == Slot::A {
                    Readiness::Committed
                } else {
                    Readiness::Ready
                },
            })
            .collect(),
        known_good_slot: Some(Slot::A),
        last_operation: Some(Operation::Init(Slot::A).label()),
        ..Default::default()
    }
}
fn same_ledger(target: &Target, record: &Installation) -> Result<bool> {
    let Some(state) = Store::new(target).load()? else {
        return Ok(false);
    };
    if record.generation.is_none()
        || serde_json::to_value(state)? != serde_json::to_value(expected(record))?
    {
        return Err(refuse(
            "init cannot reset a live or changed generation ledger",
        ));
    }
    Ok(true)
}
fn protect_source(target: &Target, device: &fs::File) -> Result<()> {
    crate::filesystem::flush(&Root::open(&target.root)?)?;
    target_root::run(
        Command::new(&target.programs.mount)
            .args(["-o", "remount,ro"])
            .arg(&target.root),
    )
    .context("sealing installed slot A")?;
    let value: libc::c_int = 1;
    if unsafe { libc::ioctl(device.as_raw_fd(), 0x125d, &value) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn record_matches(root: &Path, slot: Slot, record: &Installation) -> Result<()> {
    let generation = record
        .generation
        .as_ref()
        .context("installation has no frozen generation")?;
    generation.verify_root(root, slot, &record.roots[slot as usize])?;
    let text = Root::open(root)?.optional_text(Path::new(recinit::installed::BOOT_RECORD))?;
    if text.as_deref()
        != Some(&format!(
            "1 {slot} {} {} {}\n",
            record.roots[slot as usize],
            generation.id,
            record.roots[slot.other() as usize]
        ))
    {
        return Err(refuse(
            "installed root boot record does not match the frozen installation",
        ));
    }
    Ok(())
}

/// Seed shared homes once, before either root can boot. Atomic publication
/// prevents a first boot into B from publishing an empty root home over A's.
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
                "installed home must be a directory or shared home link",
            ));
        }
        if let Some(m) = state.metadata(Path::new(to))? {
            if !m.is_dir() {
                return Err(refuse("shared home destination is not a real directory"));
            }
            if !source.names(Path::new(from))?.is_empty() && to == "home" {
                return Err(refuse(
                    "populated /home must use /var/home before initialization",
                ));
            }
            continue;
        }
        let input = source.entry(Path::new(from), false)?;
        let location = fs::canonicalize(input.path())?;
        if target_root::mount_table()?
            .iter()
            .any(|m| m.mount_point.starts_with(&location))
        {
            return Err(refuse("unexpected mount inside an initial home"));
        }
        let temporary = format!(".recab-install-{to}");
        let scratch = state.entry(Path::new(&temporary), false)?;
        scratch.remove()?;
        target_root::run(
            Command::new("cp")
                .arg("-a")
                .arg(input.child_path())
                .arg(scratch.child_path()),
        )?;
        crate::filesystem::flush(&state)?;
        let dest = state.entry(Path::new(to), false)?;
        fs::rename(scratch.path(), dest.path())?;
        crate::filesystem::flush(&state)?;
    }
    Ok(())
}

pub(crate) fn run(target: Target, request: &InitRequest, a: &Path, b: &Path) -> Result<()> {
    crate::filesystem::private_mounts()?;
    let roots = [partuuid(a)?, partuuid(b)?];
    if roots[0] == roots[1] {
        return Err(refuse("slot A and B must be distinct partitions"));
    }
    crate::init::check_mounts(&target, &request.layout, &roots[0])?;
    let table = target_root::mount_table()?;
    let mounted = table
        .iter()
        .rev()
        .find(|m| m.mount_point == target.root)
        .context("installation root is not mounted")?;
    let source = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(fs::canonicalize(a)?)?;
    if mounted.fstype != "ext4"
        || mounted.root != Path::new("/")
        || !source.metadata()?.file_type().is_block_device()
        || source.metadata()?.rdev() != fs::metadata(&target.root)?.dev()
        || fs::metadata("/")?.dev() == source.metadata()?.rdev()
    {
        return Err(refuse(
            "initialization requires a separate installed ext4 root, never the running system",
        ));
    }
    let state = crate::immutable::state_partition(&target)?;
    let backend = SystemdBoot::new(&target);
    let esp = backend.partition()?;
    if [&roots[0], &roots[1], &state].contains(&&esp) || roots.contains(&state) {
        return Err(refuse(
            "installation root/state/ESP partitions must be distinct",
        ));
    }
    let previous = read(&target)?;
    let fresh = previous.is_none();
    let mut record = previous.unwrap_or(Installation {
        version: 1,
        roots: roots.clone(),
        state: state.clone(),
        esp: esp.clone(),
        options: request.options.clone(),
        generation: None,
        seal: None,
        synchronization: None,
        complete: false,
    });
    if record.version != 1
        || record.roots != roots
        || record.state != state
        || record.esp != esp
        || record.options != request.options
    {
        return Err(refuse(
            "installation request differs from its saved partition/options identity",
        ));
    }
    if fresh && Store::new(&target).load()?.is_some() {
        return Err(refuse(
            "recab state already exists; init cannot reset an installed generation ledger",
        ));
    }
    let ledger_exists = same_ledger(&target, &record)?;
    let id = format!("install-{}", roots[1]);
    let mut destination = crate::slot_mount::SlotMount::installed(&target, &roots[1], &id)?;
    if let Some(g) = &record.generation {
        for (path, wanted) in [
            (Some(&request.kernel), Some(&g.kernel_digest)),
            (request.initrd.as_ref(), g.initrd_digest.as_ref()),
        ] {
            if let (Some(path), Some(wanted)) = (path, wanted) {
                match fs::metadata(path) {
                    Ok(_) => {
                        if crate::generation::hash_file(path)? != *wanted {
                            return Err(refuse(
                                "initialization boot inputs differ from the frozen generation",
                            ));
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    let (kernel, initrd) = match record
        .generation
        .as_ref()
        .and_then(|g| backend.object_inputs(g).ok())
    {
        Some(paths) => paths,
        None => (
            request.kernel.clone(),
            request
                .initrd
                .clone()
                .context("immutable installation requires an installed initramfs")?,
        ),
    };
    if fresh {
        save(&target, &record)?;
    }
    if !record.complete {
        backend.withdraw_installed()?;
    }
    if record.generation.is_none() {
        crate::immutable::prepare(&target.root, Slot::A, &state)?;
    }
    let checked = crate::candidate::check(
        &target.root,
        &target.root,
        Slot::A,
        &roots[0],
        &state,
        &esp,
        &kernel,
        &initrd,
        &crate::generation::hash_file(&kernel)?,
        false,
    )?;
    if Root::open(&target.root)?
        .metadata(Path::new("usr/lib/recab/kernel.json"))?
        .is_none()
    {
        return Err(refuse(
            "immutable installation requires the release kernel/module binding",
        ));
    }
    let loader = if checked.architecture == "x86_64" {
        "x64"
    } else {
        "aa64"
    };
    if !Root::open(&target.root)?
        .metadata(Path::new(&format!(
            "usr/lib/systemd/boot/efi/systemd-boot{loader}.efi"
        )))?
        .is_some_and(|m| m.is_file())
    {
        return Err(refuse(
            "installed system lacks the matching systemd-boot loader",
        ));
    }
    backend.preflight_objects(&checked)?;
    if let Some(g) = &record.generation {
        if g.kernel_digest != checked.kernel_digest
            || g.initrd_digest.as_ref() != Some(&checked.initrd_digest)
        {
            return Err(refuse(
                "installation boot inputs differ from its frozen generation",
            ));
        }
        record_matches(&target.root, Slot::A, &record)?;
    } else {
        homes(&target)?;
        let g = Generation::capture(&target.root, &kernel, Some(&initrd), &target.state_dir)?;
        g.mark_boot(&target.root, Slot::A, &roots[0], &roots[1])?;
        record.seal = Some(BootSeal {
            generation: g.id.clone(),
            kernel_release: checked.release.clone(),
            architecture: checked.architecture.clone(),
            protocol: 3,
        });
        record.generation = Some(g);
        save(&target, &record)?;
    }
    // Preflight B read-only before protecting A and opening the copy phase.
    if !record.complete {
        drop(crate::sync::Synchronization::check(
            &target.root,
            &destination.point,
            Slot::B,
            &roots[1],
            &id,
        )?);
    }
    protect_source(&target, &source)?;
    let was_complete = record.complete;
    let result = (|| -> Result<()> {
        if record.complete {
            if !ledger_exists {
                return Err(refuse("completed installation ledger is missing"));
            }
            record_matches(&destination.point, Slot::B, &record)?;
        } else {
            destination.writable_target(&target)?;
            let sync = crate::sync::Synchronization::check(
                &target.root,
                &destination.point,
                Slot::B,
                &roots[1],
                &id,
            )?;
            let (_, bytes, resumed) = sync.save_plan(
                &target.state_dir,
                &id,
                &record.generation.as_ref().unwrap().id,
            )?;
            let report = sync.apply(SyncReport {
                plan_bytes_written: bytes,
                resumed,
                accounting_complete: !resumed,
                ..Default::default()
            })?;
            sync.verify(&destination.point)?;
            drop(sync);
            let g = record.generation.as_ref().unwrap();
            g.mark_boot(&destination.point, Slot::B, &roots[1], &roots[0])?;
            crate::filesystem::flush(&Root::open(&destination.point)?)?;
            record.synchronization = Some(report);
            save(&target, &record)?;
        }
        crate::candidate::check(
            &destination.point,
            &target.root,
            Slot::B,
            &roots[1],
            &state,
            &esp,
            &kernel,
            &initrd,
            &checked.kernel_digest,
            false,
        )?;
        record_matches(&destination.point, Slot::B, &record)?;
        destination.seal()?;
        record_matches(&target.root, Slot::A, &record)?;
        backend.install_objects(
            &checked,
            record.generation.as_ref().unwrap(),
            record.seal.as_ref().unwrap(),
        )?;
        backend.install_bootloader()?;
        if !ledger_exists {
            Store::new(&target).save(&expected(&record))?;
        }
        // Include newly created state/inventory parent directories, not just
        // the files flushed by their individual durable replacements.
        crate::filesystem::flush(&Root::open(&target.root.join("var"))?)?;
        for slot in Slot::ALL {
            backend.publish_installed(
                slot,
                &roots[slot as usize],
                record.generation.as_ref().unwrap(),
                record.seal.as_ref().unwrap(),
                checked.init,
                &request.options,
            )?;
        }
        let default = backend.installed_default()?;
        crate::init::point_firmware(&default, true)?;
        record.complete = true;
        save(&target, &record)?;
        if !was_complete {
            Store::new(&target).audit(
                Operation::Init(Slot::A),
                Slot::A,
                &PersistentState::default(),
                &expected(&record),
            );
        }
        let journal = target.state_dir.join(format!("synchronize-{id}.json"));
        if journal.exists() {
            fs::remove_file(journal)?;
            target_root::sync_dir(&target.state_dir)?;
        }
        Ok(())
    })();
    result.map_err(|e| {
        classified(
            ErrorClass::PartialUpdate,
            format!("initial A/B installation is incomplete; retry the same initialization: {e:#}"),
        )
    })
}

/// Installer completion is evidence about both personalized roots, not just an
/// exit code from writing one boot entry. This is read-only filesystem validation.
#[cfg_attr(feature="contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedInstallation {
    pub schema_version: u32,
    pub complete: bool,
    pub slots: Vec<Slot>,
    pub generation: String,
}
pub fn verify(root: &Path) -> Result<VerifiedInstallation> {
    crate::system::require_root("verify-installation")?;
    let target=Target::installed(root)?;
    let _lock=Store::new(&target).lock()?;
    let record=read(&target)?.ok_or_else(||refuse("initial A/B installation record is missing"))?;
    if record.version!=1 || !record.complete {return Err(refuse("both initial systems must be completely published"));}
    same_ledger(&target,&record)?;
    Store::new(&target).require_mounted()?;
    crate::filesystem::private_mounts()?;
    let backend=SystemdBoot::new(&target);
    let state=Store::new(&target).load()?.context("installation ledger missing")?;
    let boot=backend.read(&state)?;
    let sys=crate::system::System::read(Some(root))?;
    anyhow::ensure!(boot.selection.active==Slot::A,"installer verification must use the personalized initial root A");
    let generation=record.generation.as_ref().context("initial generation missing")?;
    for slot in Slot::ALL {
        if slot==Slot::A {record_matches(root,slot,&record)?;crate::candidate::runtime_protocol(&Root::open(root)?)?;}
        else {let mount=crate::slot_mount::SlotMount::installed(&target,&record.roots[slot as usize],&generation.id)?;record_matches(&mount.point,slot,&record)?;crate::candidate::runtime_protocol(&Root::open(&mount.point)?)?;mount.seal()?;}
        backend.verify_generation(&boot,slot,generation)?;
        crate::finalize::verify_entry(&sys,slot,generation,record.seal.as_ref().context("installation seal missing")?)?;
        backend.verify_manifest(generation,record.seal.as_ref().context("installation boot seal missing")?)?;
    }
    Ok(VerifiedInstallation {schema_version:1,complete:true,slots:Slot::ALL.to_vec(),generation:generation.id.clone()})
}
