//! What recab's package carries besides the binary, written by `recab
//! install-unit --package <init>` into the tree Recipe packages: the trial
//! service for the image's init system, enabled, and the list of machine state
//! `stage` carries ([`crate::carried`]). Every image with recab runs the trial
//! service; with no trial pending it exits at once.

use anyhow::{Context, Result};
use distro_profile::InitSystem;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use crate::carried::CARRIED_LIST;

/// Where the package installs recab.
pub const RECAB_BIN: &str = "/usr/bin/recab";
/// Seconds the trial waits for startup to finish: the trial service's
/// deadline and `recab trial`'s default.
pub const TRIAL_TIMEOUT_SECS: u64 = 900;

const SYSTEMD_UNIT: &str = include_str!("services/recab-trial.service");
const OPENRC_SERVICE: &str = include_str!("services/recab-trial.openrc");
const CARRIED: &str = include_str!("services/carried");

/// The trial service's file, relative to the root.
fn trial_service_path(init: InitSystem) -> &'static str {
    match init {
        InitSystem::Systemd => "usr/lib/systemd/system/recab-trial.service",
        InitSystem::Openrc => "etc/init.d/recab-trial",
    }
}

/// The link that enables the trial service, relative to the root (systemd's
/// `sysinit.target`, OpenRC's `boot` runlevel: both start it early, alongside
/// the rest of startup).
pub fn trial_service_link(init: InitSystem) -> &'static str {
    match init {
        InitSystem::Systemd => "etc/systemd/system/sysinit.target.wants/recab-trial.service",
        InitSystem::Openrc => "etc/runlevels/boot/recab-trial",
    }
}

/// Write the trial service, its enabling link and the carried-state list into
/// the package tree `root`.
pub(crate) fn write(root: &Path, init: InitSystem) -> Result<()> {
    let (template, mode) = match init {
        InitSystem::Systemd => (SYSTEMD_UNIT, 0o644),
        InitSystem::Openrc => (OPENRC_SERVICE, 0o755),
    };
    let text = template
        .replace("@RECAB@", RECAB_BIN)
        .replace("@TIMEOUT@", &TRIAL_TIMEOUT_SECS.to_string());
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

    let list = root.join(CARRIED_LIST);
    target_root::write_durable(&list, CARRIED.as_bytes(), Some(0o644))
        .with_context(|| format!("writing '{}'", list.display()))
}
