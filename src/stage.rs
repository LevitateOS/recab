//! `recab stage`: write a prebuilt system into the inactive slot.
//!
//! The slot's partition is named by its boot entry's `root=` (written at
//! install time). Staging makes the slot unbootable first (its kernel goes),
//! then formats and fills the partition, and installs the kernel and entry
//! last, so an interrupted stage never leaves a half-written slot bootable.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{
    ensure_entry_exists, root_selector_from_cmdline, root_selector_from_entry_content,
    tagged_err, write_atomic, Boot, Config, ErrorClass, Slot, StageRequest,
};

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

const SLOT_FILESYSTEM: &str = "ext4";
const KERNEL_FILE: &str = "vmlinuz";
const INITRD_FILE: &str = "initramfs.img";

pub(crate) fn write_slot(
    config: &Config,
    boot: &Boot,
    slot: Slot,
    request: &StageRequest,
) -> Result<()> {
    for (what, path) in [("rootfs image", Some(&request.rootfs)), ("kernel", Some(&request.kernel)), ("initrd", request.initrd.as_ref())] {
        if let Some(path) = path {
            if !path.is_file() {
                bail!("{what} '{}' is not a file", path.display());
            }
        }
    }

    let entry = boot.entry(slot);
    ensure_entry_exists(config, &entry)?;
    let entry_path = config.entries_dir.join(&entry);
    let entry_content = fs::read_to_string(&entry_path)
        .with_context(|| format!("reading entry '{}'", entry_path.display()))?;
    let device = slot_device(config, &entry, &entry_content)?;

    let boot_dir_name = format!("{}-{}", boot.prefix, slot.as_letter().to_ascii_lowercase());
    let boot_dir = config.boot_root.join(&boot_dir_name);
    // The entry names these files; without them the slot cannot be booted.
    if boot_dir.exists() {
        fs::remove_dir_all(&boot_dir)
            .with_context(|| format!("removing slot {slot}'s old kernel '{}'", boot_dir.display()))?;
    }

    let label = command_output("blkid", &["-s", "LABEL", "-o", "value", path_str(&device)?])
        .ok()
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| format!("ROOT_{slot}"));
    run(&format!("mkfs.{SLOT_FILESYSTEM}"), &["-F", "-q", "-L", &label, path_str(&device)?])?;

    let mount_point = PathBuf::from(format!("/run/recab/stage-{}", slot.as_letter().to_ascii_lowercase()));
    fs::create_dir_all(&mount_point)
        .with_context(|| format!("creating '{}'", mount_point.display()))?;
    let mounted = Mounted::mount(&device, &mount_point)?;
    let recstrap = std::env::var("RECAB_RECSTRAP_BIN").unwrap_or_else(|_| "recstrap".to_string());
    run(
        &recstrap,
        &["--quiet", "--rootfs", path_str(&request.rootfs)?, path_str(&mount_point)?],
    )
    .context("extracting the rootfs image into the slot")?;
    carry_configuration(Path::new("/"), &mount_point)?;
    let partuuid = command_output("blkid", &["-s", "PARTUUID", "-o", "value", path_str(&device)?])?;
    write_fstab(Path::new("/etc/fstab"), &mount_point.join("etc/fstab"), &partuuid)?;
    mounted.unmount()?;

    fs::create_dir_all(&boot_dir)
        .with_context(|| format!("creating '{}'", boot_dir.display()))?;
    copy_atomic(&request.kernel, &boot_dir.join(KERNEL_FILE))?;
    if let Some(initrd) = &request.initrd {
        copy_atomic(initrd, &boot_dir.join(INITRD_FILE))?;
    }
    let content = entry_with_boot_files(
        &entry_content,
        &format!("/{boot_dir_name}/{KERNEL_FILE}"),
        request.initrd.as_ref().map(|_| format!("/{boot_dir_name}/{INITRD_FILE}")).as_deref(),
    );
    write_atomic(&entry_path, content.as_bytes())
        .with_context(|| format!("writing '{}'", entry_path.display()))
}

/// The slot's block device, from its entry's `root=`; never the running root
/// and never a mounted filesystem.
fn slot_device(config: &Config, entry: &str, entry_content: &str) -> Result<PathBuf> {
    let state_err = |message: String| tagged_err(ErrorClass::StateOrDetection, message);
    let selector = root_selector_from_entry_content(entry_content)
        .ok_or_else(|| state_err(format!("entry '{entry}' has no root= option naming its partition")))?;
    let device = selector
        .device()
        .ok_or_else(|| state_err(format!("entry '{entry}' names root '{selector}', which is not present")))?;

    let cmdline = fs::read_to_string(&config.proc_cmdline).unwrap_or_default();
    if let Some(running) = root_selector_from_cmdline(&cmdline).and_then(|s| s.device()) {
        if running == device {
            return Err(state_err(format!(
                "entry '{entry}' names the running root '{}'; refusing to overwrite it",
                device.display()
            )));
        }
    }
    let mounts = fs::read_to_string("/proc/self/mounts").context("reading /proc/self/mounts")?;
    let mounted = mounts.lines().filter_map(|line| line.split_whitespace().next()).any(|source| {
        source.starts_with('/') && fs::canonicalize(source).is_ok_and(|path| path == device)
    });
    if mounted {
        return Err(state_err(format!(
            "slot partition '{}' is mounted; unmount it before staging",
            device.display()
        )));
    }
    Ok(device)
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
        if dest.symlink_metadata().is_ok() {
            fs::remove_dir_all(&dest).with_context(|| format!("replacing '{}'", dest.display()))?;
        }
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

/// The running fstab with the root line pointed at the new slot's partition,
/// which stage formats as ext4.
fn write_fstab(running: &Path, dest: &Path, partuuid: &str) -> Result<()> {
    let content = fs::read_to_string(running)
        .with_context(|| format!("reading '{}'", running.display()))?;
    let mut found_root = false;
    let lines: Vec<String> = content
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if !line.trim_start().starts_with('#') && fields.get(1) == Some(&"/") {
                found_root = true;
                let mut fields: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
                fields[0] = format!("PARTUUID={partuuid}");
                if let Some(fstype) = fields.get_mut(2) {
                    *fstype = SLOT_FILESYSTEM.to_string();
                }
                fields.join("\t")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found_root {
        bail!("'{}' has no entry for /", running.display());
    }
    write_atomic(dest, (lines.join("\n") + "\n").as_bytes())
        .with_context(|| format!("writing '{}'", dest.display()))
}

/// The entry with its `linux`/`efi`/`initrd` lines replaced.
fn entry_with_boot_files(content: &str, kernel: &str, initrd: Option<&str>) -> String {
    let mut lines = Vec::new();
    let mut placed = false;
    let boot_lines = || {
        let mut out = vec![format!("linux {kernel}")];
        if let Some(initrd) = initrd {
            out.push(format!("initrd {initrd}"));
        }
        out
    };
    for line in content.lines() {
        match line.split_whitespace().next() {
            Some("linux" | "efi" | "initrd") => {
                if !placed {
                    lines.extend(boot_lines());
                    placed = true;
                }
            }
            _ => lines.push(line.to_string()),
        }
    }
    if !placed {
        lines.extend(boot_lines());
    }
    lines.join("\n") + "\n"
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

fn copy_atomic(source: &Path, dest: &Path) -> Result<()> {
    let tmp = dest.with_extension("tmp");
    fs::copy(source, &tmp)
        .with_context(|| format!("copying '{}' to '{}'", source.display(), tmp.display()))?;
    fs::File::open(&tmp).and_then(|file| file.sync_all())?;
    fs::rename(&tmp, dest)
        .with_context(|| format!("renaming '{}' to '{}'", tmp.display(), dest.display()))
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path '{}' is not UTF-8", path.display()))
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running '{program}'"))?;
    if !output.status.success() {
        bail!(
            "{program} {args:?} failed (exit {}): {}",
            output.status.code().unwrap_or(1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running '{program}'"))?;
    if !output.status.success() {
        bail!("{program} {args:?} failed (exit {})", output.status.code().unwrap_or(1));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
