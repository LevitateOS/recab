//! Set a root up for the installed initramfs's locked boot: its fstab
//! mounts it read-only (including /etc) with the shared /var, and its slot
//! marker names the root, the shared /var and the other root. User data
//! stays on the shared /var.

use anyhow::{Context, Result};
use recinit::installed::IMMUTABLE_CONFIG;
use std::path::Path;

use crate::error::refuse;
use crate::filesystem::Root;
use crate::target::{partuuid, Target};
use crate::Slot;

/// The files each root has its own version of: they name its own partition.
pub(crate) const SLOT_FILES: &[&[u8]] = &[b"etc/fstab", IMMUTABLE_CONFIG.as_bytes()];

/// The slot marker of `slot` (`A PARTUUID=<var> PARTUUID=<other root>`),
/// which the initramfs reads.
pub(crate) fn slot_marker(slot: Slot, state: &str, other: &str) -> String {
    format!("{slot} {state} PARTUUID={other}\n")
}

/// The shared state partition must be mounted separately from the system.
pub(crate) fn state_partition(target: &Target) -> Result<String> {
    let mounts = target.mount_table()?;
    let point = target.root.join("var");
    let state = mounts
        .iter()
        .rev()
        .find(|m| m.mount_point == point)
        .ok_or_else(|| {
            refuse(format!(
                "A/B needs its shared /var partition mounted at '{}'",
                point.display()
            ))
        })?;
    use std::os::unix::fs::MetadataExt;
    if state.fstype != "ext4"
        || state.root != Path::new("/")
        || std::fs::metadata(&point)?.dev() != libc::makedev(state.dev.0, state.dev.1)
    {
        return Err(refuse("A/B needs an ext4 partition at /var"));
    }
    let state_id = partuuid(Path::new(&state.source))?;
    // An alternate mount of root is not a persistent state partition.
    if let Some(root) = mounts.iter().rev().find(|m| m.mount_point == target.root) {
        if partuuid(Path::new(&root.source))? == state_id {
            return Err(refuse("A/B needs different root and /var partitions"));
        }
    }
    Ok(state_id)
}

/// Validate paths and mount policy before changing the installed tree.
pub(crate) fn check(root: &Path, fstab: &str) -> Result<()> {
    let root = Root::open(root)?;
    for rel in [
        "etc",
        "usr",
        "usr/lib",
        "usr/lib/recab",
        "var",
        "root",
        "run",
        "tmp",
    ] {
        if root
            .metadata(Path::new(rel))?
            .is_some_and(|meta| !meta.is_dir())
        {
            return Err(refuse(format!("A/B needs '{rel}' to be a real directory")));
        }
    }
    for rel in ["etc/fstab", IMMUTABLE_CONFIG] {
        if root
            .metadata(Path::new(rel))?
            .is_some_and(|meta| !meta.is_file())
        {
            return Err(refuse(format!("A/B needs '{rel}' to be a regular file")));
        }
    }
    render_fstab(fstab, "preflight")?;
    Ok(())
}

fn render_fstab(fstab: &str, state: &str) -> Result<String> {
    let mut roots = 0;
    let mut lines = Vec::new();
    for line in fstab.lines() {
        let mut fields = line
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        if line.trim_start().starts_with('#') || fields.is_empty() {
            lines.push(line.to_string());
            continue;
        }
        if fields.len() < 4 {
            return Err(refuse("malformed fstab entry"));
        }
        match fields[1].as_str() {
            "/" => {
                roots += 1;
                let mut options = fields[3]
                    .split(',')
                    .filter(|o| !matches!(*o, "rw" | "ro" | "defaults"))
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                options.push("ro".into());
                fields[3] = options.join(",");
                fields.resize(6, "0".into());
                fields[5] = "0".into();
                lines.push(fields.join("\t"));
            }
            "/var" | "/root" | "/tmp" => {}
            path if ["/etc", "/usr", "/opt"]
                .iter()
                .any(|base| path == *base || path.starts_with(&format!("{base}/"))) =>
            {
                return Err(refuse(format!("A/B cannot mount over system path {path}")));
            }
            _ => lines.push(line.to_string()),
        }
    }
    if roots != 1 {
        return Err(refuse("fstab must have exactly one root entry"));
    }
    lines.extend([
        format!("PARTUUID={state}\t/var\text4\tdefaults\t0\t0"),
        "/var/roothome\t/root\tnone\tbind\t0\t0".into(),
        "tmpfs\t/tmp\ttmpfs\tmode=1777,nosuid,nodev\t0\t0".into(),
    ]);
    Ok(lines.join("\n") + "\n")
}

/// Write the locked-boot fstab, the slot marker (`other`: the other root's
/// PARTUUID) and the read-only runtime policy into the root at `root`.
pub(crate) fn prepare(root: &Path, slot: Slot, state: &str, other: &str) -> Result<()> {
    let opened = Root::open(root)?;
    let fstab = opened
        .optional_text(Path::new("etc/fstab"))?
        .context("the slot has no fstab")?;
    check(root, &fstab)?;
    opened
        .entry(Path::new("etc/fstab"), true)?
        .write(render_fstab(&fstab, state)?.as_bytes(), 0o644)?;
    opened.entry(Path::new(IMMUTABLE_CONFIG), true)?.write(
        slot_marker(slot, &format!("PARTUUID={state}"), other).as_bytes(),
        0o644,
    )?;
    let release = opened
        .optional_text(Path::new("etc/os-release"))?
        .context("missing os-release")?;
    let init = distro_profile::os_release_fact(&release, "REC_INIT")?;
    crate::runtime::Policy::check(root, init)?.apply(root)?;
    Ok(())
}
