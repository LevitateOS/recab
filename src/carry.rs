//! Configuration carry for the transitional release-image path. The complete
//! copy/removal plan is checked against the mounted image before slot mutation.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt};
use std::path::{Component, Path};

use crate::error::refuse;
use crate::filesystem::{self, Root};

pub const CARRIED_LIST: &str = "usr/lib/recab/carried";
pub const BASELINE: &str = "usr/lib/recab/baseline";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carry {
    File(String),
    Dir(String),
    Prefix(String),
    Accounts(String),
    Fstab(String),
}
impl Carry {
    pub fn path(&self) -> &str {
        match self {
            Self::File(p) | Self::Dir(p) | Self::Prefix(p) | Self::Accounts(p) | Self::Fstab(p) => {
                p
            }
        }
    }
}

pub fn read_list(root: &Path) -> Result<Vec<Carry>> {
    let text = Root::open(root)?
        .optional_text(Path::new(CARRIED_LIST))?
        .ok_or_else(|| refuse("the image declares no carried machine state"))?;
    let mut seen = BTreeSet::new();
    text.lines()
        .enumerate()
        .filter_map(|(number, line)| {
            let line = line.trim();
            (!line.is_empty() && !line.starts_with('#')).then_some((number, line))
        })
        .map(|(number, line)| {
            let (kind, rel) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rel = rel.trim();
            if rel.is_empty()
                || rel
                    .split('/')
                    .any(|p| p.is_empty() || matches!(p, "." | ".."))
                || Path::new(rel)
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
                || rel.contains(['\t', '\n', '\r'])
                || !seen.insert(rel.to_string())
            {
                bail!("invalid or duplicate carried path on line {}", number + 1);
            }
            if !rel.starts_with("etc/") {
                return Err(refuse(
                    "legacy configuration carry is restricted to etc/ paths",
                ));
            }
            // Shared homes are not system configuration and must not be copied.
            if [
                "var", "home", "root", "boot", "proc", "sys", "dev", "run", "tmp",
            ]
            .iter()
            .any(|excluded| rel == *excluded || rel.starts_with(&format!("{excluded}/")))
            {
                return Err(refuse(format!(
                    "carried path '{rel}' is shared data or a runtime mount; use a current image"
                )));
            }
            Ok(match kind {
                "file" => Carry::File(rel.into()),
                "dir" => Carry::Dir(rel.into()),
                "prefix" => Carry::Prefix(rel.into()),
                "accounts" => Carry::Accounts(rel.into()),
                "fstab" => Carry::Fstab(rel.into()),
                _ => bail!("unknown carried kind on line {}", number + 1),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Sig {
    kind: String,
    mode: u32,
    uid: u32,
    gid: u32,
    content: Vec<u8>,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}
impl Sig {
    fn of(root: &Root, rel: &str) -> Result<Option<Self>> {
        let Some(meta) = root.metadata(Path::new(rel))? else {
            return Ok(None);
        };
        let entry = root.entry(Path::new(rel), false)?;
        let (kind, content) = if meta.is_dir() {
            ("dir", Vec::new())
        } else if meta.file_type().is_symlink() {
            (
                "link",
                fs::read_link(entry.path())?.as_os_str().as_bytes().to_vec(),
            )
        } else if meta.is_file() {
            let mut hash = Sha256::new();
            let mut input = entry.open()?;
            let mut buffer = [0u8; 128 * 1024];
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            ("file", hash.finalize().to_vec())
        } else {
            return Err(refuse(format!("unsupported special carried file '{rel}'")));
        };
        Ok(Some(Self {
            kind: kind.into(),
            content,
            mode: meta.mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
            xattrs: filesystem::xattrs(&entry.path())?,
        }))
    }
    fn directory(&self) -> bool {
        self.kind == "dir"
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Baseline {
    version: u32,
    entries: BTreeMap<String, Sig>,
}
impl Baseline {
    pub fn read(root: &Path) -> Result<Option<Self>> {
        let Some(text) = Root::open(root)?.optional_text(Path::new(BASELINE))? else {
            return Ok(None);
        };
        let baseline: Self = serde_json::from_str(&text).map_err(|_| refuse("the carried baseline lacks complete metadata; explicit adoption with a known release baseline is required"))?;
        if baseline.version != 2 {
            return Err(refuse("unsupported carried baseline version"));
        }
        Ok(Some(baseline))
    }
    pub fn capture(root: &Path, list: &[Carry]) -> Result<Self> {
        let root = Root::open(root)?;
        let mut entries = BTreeMap::new();
        for entry in list {
            for rel in listed(&root, entry)? {
                walk(&root, &rel, &mut entries)?;
            }
        }
        Ok(Self {
            version: 2,
            entries,
        })
    }
    pub fn write(root: &Path, list: &[Carry]) -> Result<()> {
        Self::capture(root, list)?.write_to(root)
    }
    pub fn write_to(&self, root: &Path) -> Result<()> {
        Root::open(root)?
            .entry(Path::new(BASELINE), true)?
            .write(&serde_json::to_vec(self)?, 0o600)
    }

}

fn names(root: &Root, rel: &str) -> Result<Vec<String>> {
    match root.names(Path::new(rel)) {
        Err(err) if filesystem::missing(&err) => Ok(Vec::new()),
        result => result?
            .into_iter()
            .map(|name| {
                name.into_string().map_err(|_| {
                    refuse("legacy carry cannot represent this path; use system continuation")
                })
            })
            .collect(),
    }
}
fn listed(root: &Root, entry: &Carry) -> Result<Vec<String>> {
    match entry {
        Carry::Prefix(rel) => {
            let (dir, prefix) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
            Ok(names(root, dir)?
                .into_iter()
                .filter(|name| name.starts_with(prefix))
                .map(|name| {
                    if dir.is_empty() {
                        name
                    } else {
                        format!("{dir}/{name}")
                    }
                })
                .collect())
        }
        _ => Ok(vec![entry.path().to_string()]),
    }
}
fn walk(root: &Root, rel: &str, entries: &mut BTreeMap<String, Sig>) -> Result<()> {
    let Some(sig) = Sig::of(root, rel)? else {
        return Ok(());
    };
    let directory = sig.directory();
    entries.insert(rel.to_string(), sig);
    if directory {
        for name in names(root, rel)? {
            walk(root, &format!("{rel}/{name}"), entries)?;
        }
    }
    Ok(())
}

pub(crate) fn fstab_for_slot(machine: &str, partuuid: &str, fs: &str) -> Result<String> {
    let mut found_root = false;
    let lines: Vec<String> = machine
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            if line.trim_start().starts_with('#') || fields.get(1) != Some(&"/") {
                return line.to_string();
            }
            found_root = true;
            let spec = format!("PARTUUID={partuuid}");
            fields[0] = &spec;
            match fields.get_mut(2) {
                Some(fstype) => *fstype = fs,
                None => fields.push(fs),
            }
            fields.join("\t")
        })
        .collect();
    if !found_root {
        return Err(refuse("the running system's fstab has no entry for /"));
    }
    Ok(lines.join("\n") + "\n")
}
