//! `recab init`: set up the A/B boot layout of a freshly installed system,
//! so the layout recab later reads and rewrites is written by recab.
//!
//! Slot A holds the installed system; slot B is empty until `recab stage`.
//! Both entries are written now, because their `root=PARTUUID=` is the
//! record of which partition is which slot.

use anyhow::{bail, Context, Result};
use distro_profile::os_release_value;
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::init_system::{self, InitSystemExt};
use crate::state::{Operation, PersistentState};
use crate::store::{copy_durable, read_optional, write_durable, Store};
use crate::systemd_boot::{loader_with_default, SystemdBoot, PANIC_OPTION};
use crate::{tagged_err, ErrorClass, InitRequest, Slot};

/// Set up the system under `request.root`; `config` is that system's
/// ([`Config::install_target`]).
pub(crate) fn run(config: Config, request: &InitRequest) -> Result<()> {
    let root = &request.root;
    let boot_root = root.join("boot");
    // `bootctl install` creates loader/; installers that copy systemd-boot's
    // EFI binary themselves do not.
    let systemd_boot = [
        "EFI/systemd/systemd-bootx64.efi",
        "EFI/BOOT/BOOTX64.EFI",
        "EFI/systemd/systemd-bootaa64.efi",
        "EFI/BOOT/BOOTAA64.EFI",
    ]
    .iter()
    .any(|rel| boot_root.join(rel).is_file());
    if !systemd_boot {
        bail!(
            "'{}' has no systemd-boot EFI binary; install systemd-boot on the target's ESP first",
            boot_root.display()
        );
    }
    let init = init_system::installed_profile(root)?.init;
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
        let mut entry = format!(
            "title {name} (Slot {slot})\nlinux /{}/vmlinuz\n",
            dir(*slot)
        );
        if request.initrd.is_some() {
            entry.push_str(&format!("initrd /{}/initramfs.img\n", dir(*slot)));
        }
        let extra = if request.options.trim().is_empty() {
            String::new()
        } else {
            format!(" {}", request.options.trim())
        };
        entry.push_str(&format!(
            "options root=PARTUUID={partuuid} ro rootwait {PANIC_OPTION}{extra}\n"
        ));
        let path = entries.join(format!("{id}-{}.conf", slot.entry_suffix()));
        write_durable(&path, entry.as_bytes())
            .with_context(|| format!("writing '{}'", path.display()))?;
    }

    let default_entry = format!("{id}-a.conf");
    let loader_conf = boot_root.join("loader/loader.conf");
    let before = read_optional(&loader_conf)?;
    write_durable(
        &loader_conf,
        loader_with_default(before.as_deref(), &default_entry).as_bytes(),
    )
    .with_context(|| format!("writing '{}'", loader_conf.display()))?;

    let state = PersistentState {
        known_good_slot: Some(Slot::A),
        last_operation: Some(Operation::Init(Slot::A).label()),
        ..PersistentState::default()
    };
    Store::new(&config).save(&state, "init")?;
    init.enable_trial_service(root)?;

    // A previous install may have left systemd-boot variables naming the
    // other slot; they win over loader.conf. Point them at slot A.
    let backend = SystemdBoot::new(&config);
    if backend.efivars_available() {
        backend
            .set_loader_efivar("LoaderEntryDefault", Some(&default_entry))
            .and_then(|()| backend.set_loader_efivar("LoaderEntryOneShot", None))
            .context("pointing systemd-boot's firmware variables at slot A")?;
    } else {
        eprintln!(
            "recab: warning: EFI variables are not available here (no efivarfs), so systemd-boot's \
LoaderEntryDefault and LoaderEntryOneShot left by an earlier install could not be reset to slot A; \
until they are, they may select another entry. 'recab stage' resets them before it writes slot B."
        );
    }
    Ok(())
}

/// The target's os-release `ID` (the entry prefix) and `NAME` (entry titles).
fn os_identity(root: &Path) -> Result<(String, String)> {
    let content = target_root::os_release(root)?
        .with_context(|| format!("'{}' has no os-release", root.display()))?;
    let id = os_release_value(&content, "ID")
        .with_context(|| format!("'{}'s os-release has no ID", root.display()))?;
    let name = os_release_value(&content, "NAME").unwrap_or_else(|| id.clone());
    Ok((id.to_ascii_lowercase(), name))
}

fn partuuid(device: &Path) -> Result<String> {
    target_root::partuuid(device)?.ok_or_else(|| {
        tagged_err(
            ErrorClass::StateOrDetection,
            format!(
                "'{}' has no PARTUUID; A/B slots must be GPT partitions",
                device.display()
            ),
        )
    })
}
