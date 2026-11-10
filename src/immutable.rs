//! Prepare a slot for the installed initramfs's immutable boot. The root is
//! read-only, including /etc. User data remains on shared /var.

use anyhow::{Context, Result};
use recinit::installed::{IMMUTABLE_CONFIG, check_immutable_initramfs};
use std::path::Path;

use crate::error::refuse;
use crate::filesystem::Root;
use crate::state::Slot;
use crate::target::{Target, partuuid};

pub(crate) fn check_initramfs(initrd: Option<&Path>) -> Result<()> {
    let path = initrd.ok_or_else(|| {
        refuse("immutable A/B boot requires --initrd: use the release's installed-system initramfs")
    })?;
    check_immutable_initramfs(path).map_err(|err| {
        refuse(format!(
            "initramfs '{}': {err}; use a release with immutable A/B boot support",
            path.display()
        ))
    })
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
                "immutable A/B needs its state partition mounted at '{}'",
                point.display()
            ))
        })?;
    if state.fstype != "ext4" {
        return Err(refuse(
            "immutable A/B needs an ext4 state partition at /var",
        ));
    }
    let state_id = partuuid(Path::new(&state.source))?;
    // An alternate mount of root is not a persistent state partition.
    if let Some(root) = mounts.iter().rev().find(|m| m.mount_point == target.root) {
        if partuuid(Path::new(&root.source))? == state_id {
            return Err(refuse(
                "immutable A/B needs different root and /var partitions",
            ));
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
            return Err(refuse(format!(
                "immutable A/B needs '{rel}' to be a real directory"
            )));
        }
    }
    for rel in ["etc/fstab", IMMUTABLE_CONFIG] {
        if root
            .metadata(Path::new(rel))?
            .is_some_and(|meta| !meta.is_file())
        {
            return Err(refuse(format!(
                "immutable A/B needs '{rel}' to be a regular file"
            )));
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
                return Err(refuse(format!(
                    "immutable A/B cannot mount over system path {path}"
                )));
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

pub(crate) fn prepare(root: &Path, slot: Slot, state: &str) -> Result<()> {
    let opened = Root::open(root)?;
    let fstab = opened
        .optional_text(Path::new("etc/fstab"))?
        .context("the slot has no fstab")?;
    check(root, &fstab)?;
    opened
        .entry(Path::new("etc/fstab"), true)?
        .write(render_fstab(&fstab, state)?.as_bytes(), 0o644)?;
    opened
        .entry(Path::new(IMMUTABLE_CONFIG), true)?
        .write(format!("{slot} PARTUUID={state}\n").as_bytes(), 0o644)?;
    Ok(())
}

pub(crate) fn require_readonly(target: &Target) -> Result<()> {
    let mounts = target.mount_table()?;
    let root = mounts
        .iter()
        .rev()
        .find(|mount| mount.mount_point == target.root)
        .ok_or_else(|| refuse("the running root is not a mount point"))?;
    if !root.options.split(',').any(|o| o == "ro") {
        return Err(refuse(
            "the active system root is writable; immutable boot is required before preparation",
        ));
    }
    for mount in &mounts {
        if ["etc", "usr", "opt"]
            .iter()
            .any(|rel| mount.mount_point.starts_with(target.root.join(rel)))
        {
            return Err(refuse(
                "the active system has a separate configuration/system mount; explicit adoption is required",
            ));
        }
    }
    Ok(())
}
