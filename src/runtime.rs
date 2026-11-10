//! Explicit runtime exceptions, without a writable /etc overlay.
use crate::error::refuse;
use crate::filesystem::Root;
use anyhow::Result;
use distro_profile::InitSystem;
use std::fs;
use std::path::Path;

const DROPIN: &str = "[Unit]\nConditionKernelCommandLine=!recab.immutable\n";
const NM_DYNAMIC: &str = "[main]\nrc-manager=symlink\n";
const NM_STATIC: &str = "[main]\nrc-manager=unmanaged\n";
const CLOCK: &str = "# recab immutable clock\nclock_adjfile=\"NO\"\n";
const SKIP_UNITS: &[&str] = &[
    "systemd-remount-fs.service",
    "systemd-update-done.service",
    "systemd-sysusers.service",
    "systemd-firstboot.service",
    "ldconfig.service",
    "systemd-hwdb-update.service",
    "sshd-keygen@.service",
];
struct Change {
    path: String,
    contents: Option<String>,
}
pub(crate) struct Policy {
    changes: Vec<Change>,
}
impl Policy {
    pub fn check(root: &Path, init: InitSystem) -> Result<Self> {
        let opened = Root::open(root)?;
        let mut changes = Vec::new();
        for p in ["etc/mtab", "etc/resolv.conf"] {
            let meta = opened.metadata(Path::new(p))?;
            let target = if p == "etc/mtab" {
                "/proc/self/mounts"
            } else {
                "/run/NetworkManager/resolv.conf"
            };
            match meta {
                Some(m) if m.file_type().is_symlink() => {
                    let link = fs::read_link(opened.entry(Path::new(p), false)?.path())?;
                    if link != Path::new(target)
                        && !(p == "etc/mtab" && link == Path::new("/proc/mounts"))
                    {
                        return Err(refuse(format!(
                            "{p} does not use the supported runtime target"
                        )));
                    }
                }
                Some(m) if m.is_file() && p == "etc/resolv.conf" => {} // Explicit static DNS remains immutable.
                Some(m) if !m.is_file() => {
                    return Err(refuse("runtime path has an unsupported type"))
                }
                _ => changes.push(Change {
                    path: p.into(),
                    contents: None,
                }),
            }
        }
        let static_dns = opened
            .metadata(Path::new("etc/resolv.conf"))?
            .is_some_and(|m| m.is_file());
        add(
            &opened,
            &mut changes,
            "etc/NetworkManager/conf.d/90-recab-immutable.conf",
            if static_dns { NM_STATIC } else { NM_DYNAMIC },
            &[NM_STATIC, NM_DYNAMIC],
        )?;
        if init == InitSystem::Systemd {
            for unit in SKIP_UNITS {
                add(
                    &opened,
                    &mut changes,
                    &format!("etc/systemd/system/{unit}.d/90-recab-immutable.conf"),
                    DROPIN,
                    &[DROPIN],
                )?;
            }
        } else {
            let p = "etc/conf.d/hwclock";
            if let Some(text) = opened.optional_text(Path::new(p))? {
                if !text.ends_with(CLOCK) {
                    if text.contains("# recab immutable clock") {
                        return Err(refuse("immutable hwclock policy was modified"));
                    }
                    changes.push(Change {
                        path: p.into(),
                        contents: Some(format!("{}\n{CLOCK}", text.trim_end())),
                    });
                }
            }
        }
        Ok(Self { changes })
    }
    pub fn apply(self, root: &Path) -> Result<()> {
        let opened = Root::open(root)?;
        for change in self.changes {
            let entry = opened.entry(Path::new(&change.path), true)?;
            if let Some(m) = entry.metadata()? {
                if !m.is_file() {
                    return Err(refuse("runtime policy target changed type"));
                }
                // Detach before changing configuration: no writes through aliases.
                fs::remove_file(entry.path())?;
            }
            match change.contents {
                Some(text) => entry.write(text.as_bytes(), 0o644)?,
                None => std::os::unix::fs::symlink(
                    if change.path == "etc/mtab" {
                        "/proc/self/mounts"
                    } else {
                        "/run/NetworkManager/resolv.conf"
                    },
                    entry.path(),
                )?,
            }
        }
        Ok(())
    }
}
fn add(
    root: &Root,
    changes: &mut Vec<Change>,
    p: &str,
    wanted: &str,
    known: &[&str],
) -> Result<()> {
    if root.metadata(Path::new(p))?.is_some_and(|m| !m.is_file()) {
        return Err(refuse("runtime policy must be a regular file"));
    }
    let current = root.optional_text(Path::new(p))?;
    if current.as_deref() == Some(wanted) {
        return Ok(());
    }
    if current
        .as_deref()
        .is_some_and(|text| !known.contains(&text))
    {
        return Err(refuse(format!(
            "runtime policy '{p}' conflicts with candidate configuration"
        )));
    }
    changes.push(Change {
        path: p.into(),
        contents: Some(wanted.into()),
    });
    Ok(())
}

/// Bind the producer's actual recab executable to the state/boot protocols it
/// supports. Finalization never executes an inactive binary to ask its version.
pub(crate) fn package_runtime(root: &Path, bin: &str) -> Result<()> {
    let opened = Root::open(root)?;
    let Some(mut file) = opened.open_resolved(Path::new(bin.trim_start_matches('/')))? else {
        return Ok(());
    };
    let (major, minor) = match crate::state::StateSchema::default() {
        crate::state::StateSchema::Current { major, minor } => (major, minor),
        _ => unreachable!(),
    };
    let record = serde_json::json!({"version":1,"state_major":major,"state_minor_min":0,"state_minor_max":minor,"boot_protocol":3,"binary":bin.trim_start_matches('/'),"binary_digest":crate::generation::hash_reader(&mut file)?});
    opened
        .entry(Path::new("usr/lib/recab/runtime.json"), true)?
        .write(&serde_json::to_vec(&record)?, 0o644)
}
