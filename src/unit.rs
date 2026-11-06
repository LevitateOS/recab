//! `recab-trial.service`, which runs `recab trial` on every boot.
//!
//! It starts early and survives isolation into the emergency target, so a
//! trial that never reaches normal operation is still caught. It is a plain
//! Type=exec service: startup does not wait for it, and it waits for startup.

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use crate::store::write_durable;

pub const UNIT_NAME: &str = "recab-trial.service";

const UNIT: &str = "\
# Installed by `recab install-unit`.
[Unit]
Description=Commit a healthy A/B trial boot, or reboot into the known-good slot
DefaultDependencies=no
Conflicts=shutdown.target
Before=shutdown.target
IgnoreOnIsolate=yes

[Service]
Type=exec
ExecStart=/usr/bin/recab trial

[Install]
WantedBy=sysinit.target
";

const WANTS_DIR: &str = "etc/systemd/system/sysinit.target.wants";

fn unit_path(root: &Path) -> std::path::PathBuf {
    root.join("etc/systemd/system").join(UNIT_NAME)
}

/// The unit is enabled under `root`.
pub(crate) fn enabled(root: &Path) -> bool {
    root.join(WANTS_DIR).join(UNIT_NAME).symlink_metadata().is_ok()
}

/// Write the unit under `root` and enable it, as `systemctl enable` would.
pub(crate) fn install(root: &Path) -> Result<()> {
    let path = unit_path(root);
    write_durable(&path, UNIT.as_bytes()).with_context(|| format!("writing '{}'", path.display()))?;
    let wants = root.join(WANTS_DIR);
    fs::create_dir_all(&wants).with_context(|| format!("creating '{}'", wants.display()))?;
    let link = wants.join(UNIT_NAME);
    if link.symlink_metadata().is_ok() {
        fs::remove_file(&link).with_context(|| format!("replacing '{}'", link.display()))?;
    }
    let target = Path::new("/etc/systemd/system").join(UNIT_NAME);
    symlink(&target, &link).with_context(|| format!("enabling '{}'", link.display()))
}
