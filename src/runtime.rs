//! What a read-only root needs to run: links and drop-ins that keep
//! services from writing to it. Installed with recab's package and by `init`.
//! The NetworkManager part (resolv.conf, the conf.d drop-in) applies only to a
//! root that has NetworkManager, which the `network` group installs; the boot
//! step runs after the software step, so a desktop install has it by then.
use crate::error::refuse;
use crate::filesystem::Root;
use anyhow::{Context, Result};
use distro_profile::InitSystem;
use std::path::Path;

const DROPIN: &str = "[Unit]\nConditionKernelCommandLine=!recab.immutable\n";
const NM_DYNAMIC: &str = "[main]\nrc-manager=symlink\n";
const NM_STATIC: &str = "[main]\nrc-manager=unmanaged\n";
/// The `network` group's NetworkManager program, on both OSes (Fedora and
/// Alpine ship it in usr/sbin; a merged-/usr root may list it in usr/bin).
const NM_PROGRAMS: &[&str] = &["usr/sbin/NetworkManager", "usr/bin/NetworkManager"];
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
        // NetworkManager is the `network` group's, not the base's: a root
        // without it keeps its resolv.conf as it is, and gets no drop-in.
        let mut has_nm = false;
        for program in NM_PROGRAMS {
            // A merged-/usr root links usr/sbin to bin, so that spelling is not a directory there; usr/bin finds it.
            has_nm |= match opened.file_type(Path::new(program)) {
                Ok(kind) => kind.is_some(),
                Err(err) => match err
                    .chain()
                    .find_map(|e| e.downcast_ref::<std::io::Error>())
                    .and_then(|io| io.raw_os_error())
                {
                    Some(libc::ENOTDIR | libc::ELOOP) => false,
                    _ => return Err(err),
                },
            };
        }
        for p in ["etc/mtab", "etc/resolv.conf"] {
            if p == "etc/resolv.conf" && !has_nm {
                continue;
            }
            let meta = opened
                .file_type(Path::new(p))
                .with_context(|| format!("checking runtime policy {p}"))?;
            let target = if p == "etc/mtab" {
                "/proc/self/mounts"
            } else {
                "/run/NetworkManager/resolv.conf"
            };
            match meta {
                Some(libc::S_IFLNK) => {
                    let link = opened.entry(Path::new(p), false)?.read_link()?;
                    if !names_target(&link, "/etc", target)
                        && !(p == "etc/mtab" && names_target(&link, "/etc", "/proc/mounts"))
                    {
                        return Err(refuse(format!(
                            "{p} does not use the supported runtime target"
                        )));
                    }
                }
                Some(libc::S_IFREG) if p == "etc/resolv.conf" => {} // Explicit static DNS remains immutable.
                Some(m) if m != libc::S_IFREG => {
                    return Err(refuse("runtime path has an unsupported type"));
                }
                _ => changes.push(Change {
                    path: p.into(),
                    contents: None,
                }),
            }
        }
        if has_nm {
            let static_dns = opened
                .file_type(Path::new("etc/resolv.conf"))?
                .is_some_and(|m| m == libc::S_IFREG);
            add(
                &opened,
                &mut changes,
                "etc/NetworkManager/conf.d/90-recab-immutable.conf",
                if static_dns { NM_STATIC } else { NM_DYNAMIC },
                &[NM_STATIC, NM_DYNAMIC],
            )?;
        }
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
            let entry = opened
                .entry(Path::new(&change.path), true)
                .with_context(|| format!("opening runtime policy {}", change.path))?;
            if let Some(m) = entry.file_type()? {
                if m != libc::S_IFREG {
                    return Err(refuse("runtime policy target changed type"));
                }
                // Detach before changing configuration: no writes through aliases.
                entry.remove_file()?;
            }
            match change.contents {
                Some(text) => entry.write(text.as_bytes(), 0o644)?,
                None => entry.symlink(Path::new(if change.path == "etc/mtab" {
                    "/proc/self/mounts"
                } else {
                    "/run/NetworkManager/resolv.conf"
                }))?,
            }
        }
        Ok(())
    }
}
/// Whether the symlink target `link`, resolved lexically from the directory
/// `dir` (never followed on the host: the root may be mounted elsewhere),
/// names `wanted`. Absolute and relative spellings (`/proc/self/mounts`,
/// `../proc/self/mounts`) are both the distributions' own.
fn names_target(link: &Path, dir: &str, wanted: &str) -> bool {
    use std::path::Component;
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    let start = if link.is_absolute() { "" } else { dir };
    for c in Path::new(start).components().chain(link.components()) {
        match c {
            Component::Normal(n) => parts.push(n),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    parts
        .iter()
        .copied()
        .eq(Path::new(wanted).components().filter_map(|c| match c {
            Component::Normal(n) => Some(n),
            _ => None,
        }))
}
fn add(
    root: &Root,
    changes: &mut Vec<Change>,
    p: &str,
    wanted: &str,
    known: &[&str],
) -> Result<()> {
    if root
        .file_type(Path::new(p))?
        .is_some_and(|m| m != libc::S_IFREG)
    {
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
            "runtime policy '{p}' conflicts with the root's configuration"
        )));
    }
    changes.push(Change {
        path: p.into(),
        contents: Some(wanted.into()),
    });
    Ok(())
}
