//! What recab's package carries besides the binary, written by `recab
//! install-unit --init <init>` into the tree Recipe packages: the trial
//! service for the image's init system, enabled, immutable runtime policy
//! and executable-bound compatibility metadata. Every image with recab runs
//! the trial service, which waits for startup health before judging a trial.

use anyhow::{Context, Result};
use distro_profile::InitSystem;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use crate::error::refuse;

/// Where the package installs recab, unless the packager says otherwise.
pub const RECAB_BIN: &str = "/usr/bin/recab";

const SYSTEMD_UNIT: &str = include_str!("services/recab-trial.service");
const OPENRC_SERVICE: &str = include_str!("services/recab-trial.openrc");

/// What both trial services say they are.
const DESCRIPTION: &str = "Commit a healthy A/B trial boot, or reboot into the known-good slot";
/// systemd starts the trial service with `sysinit.target`, OpenRC in its
/// `boot` runlevel: both early, alongside the rest of startup.
const SYSTEMD_WANTED_BY: &str = "sysinit.target";
pub(crate) const OPENRC_RUNLEVEL: &str = "boot";

/// The trial service's file, relative to the root.
fn trial_service_path(init: InitSystem) -> &'static str {
    match init {
        InitSystem::Systemd => "usr/lib/systemd/system/recab-trial.service",
        InitSystem::Openrc => "etc/init.d/recab-trial",
    }
}

/// The link that enables the trial service, relative to the root.
pub fn trial_service_link(init: InitSystem) -> String {
    match init {
        InitSystem::Systemd => {
            format!("etc/systemd/system/{SYSTEMD_WANTED_BY}.wants/recab-trial.service")
        }
        InitSystem::Openrc => format!("etc/runlevels/{OPENRC_RUNLEVEL}/recab-trial"),
    }
}

/// `template` with its `@NAME@` placeholders filled; one left unfilled is
/// an error.
fn render(template: &str, values: &[(&str, &str)]) -> Result<String> {
    let mut text = template.to_string();
    for (name, value) in values {
        text = text.replace(&format!("@{name}@"), value);
    }
    let mut rest = text.as_str();
    while let Some(start) = rest.find('@') {
        let after = &rest[start + 1..];
        let name_len = after
            .bytes()
            .take_while(|b| b.is_ascii_uppercase() || *b == b'_')
            .count();
        if name_len > 0 && after[name_len..].starts_with('@') {
            return Err(refuse(format!(
                "the trial service template has an unfilled @{}@",
                &after[..name_len]
            )));
        }
        rest = after;
    }
    Ok(text)
}

/// Write the trial service for `init` (running the binary at `bin`), its
/// enabling link, runtime policy and compatibility record into the package tree `root`.
pub(crate) fn write(root: &Path, init: InitSystem, bin: &str) -> Result<()> {
    let (template, mode) = match init {
        InitSystem::Systemd => (SYSTEMD_UNIT, 0o644),
        InitSystem::Openrc => (OPENRC_SERVICE, 0o755),
    };
    let name = Path::new(bin)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("recab");
    let text = render(
        template,
        &[
            ("RECAB", bin),
            ("NAME", name),
            ("DESCRIPTION", DESCRIPTION),
            ("WANTED_BY", SYSTEMD_WANTED_BY),
        ],
    )?;
    let path = root.join(trial_service_path(init));
    target_root::write_durable(&path, text.as_bytes(), Some(mode))
        .with_context(|| format!("writing '{}'", path.display()))?;

    let link = root.join(trial_service_link(init));
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating '{}'", parent.display()))?;
    }
    if target_root::exists(&link)? {
        fs::remove_file(&link).with_context(|| format!("replacing '{}'", link.display()))?;
    }
    symlink(Path::new("/").join(trial_service_path(init)), &link)
        .with_context(|| format!("enabling '{}'", link.display()))?;

    crate::runtime::Policy::check(root, init)?.apply(root)?;
    crate::runtime::package_runtime(root, bin)
}
