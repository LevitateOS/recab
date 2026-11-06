//! `recab init`: set up the A/B boot layout of a freshly installed system,
//! so the layout recab later reads and rewrites is written by recab.
//!
//! Slot A holds the installed system; slot B is empty until `recab stage`.
//! Both entries are written now, because their `root=PARTUUID=` is the
//! record of which partition is which slot.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::state::PersistentState;
use crate::store::{copy_durable, read_optional, write_durable, Store};
use crate::systemd_boot::{loader_with_default, SystemdBoot, PANIC_OPTION};
use crate::{tagged_err, unit, ErrorClass, InitRequest, Slot};

pub(crate) fn run(request: &InitRequest) -> Result<()> {
    let root = &request.root;
    let boot_root = root.join("boot");
    if !boot_root.join("loader").is_dir() {
        bail!(
            "'{}' has no loader/ directory; install systemd-boot on the target's ESP first (bootctl install)",
            boot_root.display()
        );
    }
    let (id, name) = os_identity(root)?;
    let partuuids = [
        (Slot::A, partuuid(&request.slot_a)?),
        (Slot::B, partuuid(&request.slot_b)?),
    ];
    if partuuids[0].1 == partuuids[1].1 {
        bail!("slot A and slot B are the same partition");
    }

    // Slot A's kernel; slot B's directory stays empty, so its entry cannot
    // boot until `recab stage` fills it.
    let dir = |slot: Slot| format!("{id}-{}", slot.entry_suffix());
    let a_dir = boot_root.join(dir(Slot::A));
    fs::create_dir_all(&a_dir).with_context(|| format!("creating '{}'", a_dir.display()))?;
    copy_durable(&request.kernel, &a_dir.join("vmlinuz"))?;
    if let Some(initrd) = &request.initrd {
        copy_durable(initrd, &a_dir.join("initramfs.img"))?;
    }

    let entries = boot_root.join("loader/entries");
    fs::create_dir_all(&entries).with_context(|| format!("creating '{}'", entries.display()))?;
    for (slot, partuuid) in &partuuids {
        let mut entry = format!("title {name} (Slot {slot})\nlinux /{}/vmlinuz\n", dir(*slot));
        if request.initrd.is_some() {
            entry.push_str(&format!("initrd /{}/initramfs.img\n", dir(*slot)));
        }
        let extra = if request.options.trim().is_empty() {
            String::new()
        } else {
            format!(" {}", request.options.trim())
        };
        entry.push_str(&format!("options root=PARTUUID={partuuid} ro rootwait {PANIC_OPTION}{extra}\n"));
        let path = entries.join(format!("{id}-{}.conf", slot.entry_suffix()));
        write_durable(&path, entry.as_bytes()).with_context(|| format!("writing '{}'", path.display()))?;
    }

    let default_entry = format!("{id}-a.conf");
    let loader_conf = boot_root.join("loader/loader.conf");
    let before = read_optional(&loader_conf)?;
    write_durable(&loader_conf, loader_with_default(before.as_deref(), &default_entry).as_bytes())
        .with_context(|| format!("writing '{}'", loader_conf.display()))?;

    let state = PersistentState {
        known_good_slot: Some(Slot::A),
        last_operation: Some("init-A".to_string()),
        ..PersistentState::default()
    };
    Store::under(root).save(&state, "init")?;
    unit::install(root)?;

    // A previous install may have left systemd-boot variables naming the
    // other slot; they win over loader.conf. Point them at slot A.
    let backend = SystemdBoot::at_boot_root(&boot_root)?;
    if backend.efivars_available() {
        let esp = format!("--esp-path={}", boot_root.display());
        backend
            .bootctl(&[&esp, "set-default", &default_entry])
            .and_then(|()| backend.bootctl(&[&esp, "set-oneshot", ""]))
            .context("pointing systemd-boot's firmware variables at slot A")?;
    }
    Ok(())
}

/// The target's os-release `ID` (the entry prefix) and `NAME` (entry titles).
fn os_identity(root: &Path) -> Result<(String, String)> {
    let content = ["etc/os-release", "usr/lib/os-release"]
        .iter()
        .find_map(|rel| fs::read_to_string(root.join(rel)).ok())
        .with_context(|| format!("'{}' has no os-release", root.display()))?;
    let field = |key: &str| {
        content
            .lines()
            .filter_map(|line| line.trim().strip_prefix(key))
            .map(|value| value.trim_matches('"').trim().to_string())
            .find(|value| !value.is_empty())
    };
    let id = field("ID=").with_context(|| format!("'{}'s os-release has no ID", root.display()))?;
    let name = field("NAME=").unwrap_or_else(|| id.clone());
    Ok((id.to_ascii_lowercase(), name))
}

fn partuuid(device: &Path) -> Result<String> {
    let out = Command::new("blkid")
        .args(["-s", "PARTUUID", "-o", "value"])
        .arg(device)
        .output()
        .context("running 'blkid'")?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_ascii_lowercase();
    if !out.status.success() || value.is_empty() {
        return Err(tagged_err(
            ErrorClass::StateOrDetection,
            format!("'{}' has no PARTUUID; A/B slots must be GPT partitions", device.display()),
        ));
    }
    Ok(value)
}
