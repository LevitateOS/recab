//! Configuration carry for the transitional release-image path. The complete
//! copy/removal plan is checked against the mounted image before slot mutation.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown};
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
    pub fn unchanged(&self, root: &Path, rel: &str) -> Result<bool> {
        Ok(Sig::of(&Root::open(root)?, rel)?.as_ref() == self.entries.get(rel))
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

pub(crate) struct CarryPlan {
    source: Root,
    operations: Vec<(String, Option<Sig>)>,
}
impl CarryPlan {
    pub fn check(
        from: &Path,
        image: &Path,
        list: &[Carry],
        baseline: Option<&Baseline>,
    ) -> Result<Self> {
        let source = Root::open(from)?;
        let target = Root::open(image)?;
        let mut operations = BTreeMap::new();
        for entry in list {
            if matches!(entry, Carry::Accounts(_) | Carry::Fstab(_)) {
                continue;
            }
            if matches!(entry, Carry::File(_))
                && (Sig::of(&source, entry.path())?.is_some_and(|s| s.directory())
                    || Sig::of(&target, entry.path())?.is_some_and(|s| s.directory()))
            {
                return Err(refuse(format!(
                    "carried file '{}' is a directory",
                    entry.path()
                )));
            }
            if matches!(entry, Carry::Dir(_))
                && Sig::of(&source, entry.path())?.is_some_and(|s| !s.directory())
            {
                return Err(refuse(format!(
                    "carried directory '{}' is not a directory",
                    entry.path()
                )));
            }
            let mut rels = listed(&source, entry)?;
            if let Some(base) = baseline {
                rels.extend(
                    base.entries
                        .keys()
                        .filter(|path| match entry {
                            Carry::Prefix(prefix) => {
                                path.starts_with(prefix) && !path[prefix.len()..].contains('/')
                            }
                            _ => path.as_str() == entry.path(),
                        })
                        .cloned(),
                );
            }
            for rel in rels {
                plan_path(&source, &target, &rel, baseline, &mut operations)?;
            }
        }
        Ok(Self {
            source,
            operations: operations.into_iter().collect(),
        })
    }
    pub fn additional_usage(&self) -> Result<(u64, u64)> {
        let mut bytes = 0u64;
        let mut entries = 0u64;
        for (rel, sig) in &self.operations {
            if sig.is_some() {
                let meta = self.source.metadata(Path::new(rel))?.ok_or_else(|| refuse("source changed during capacity inspection"))?;
                bytes = bytes.saturating_add(meta.len()).saturating_add(512);
                entries += 1;
            }
        }
        Ok((bytes, entries))
    }
    pub fn apply(&self, to: &Path) -> Result<()> {
        let target = Root::open(to)?;
        for (rel, wanted) in &self.operations {
            if Sig::of(&self.source, rel)? != *wanted {
                return Err(refuse(format!(
                    "source configuration changed during staging at '{rel}'"
                )));
            }
            let entry = match target.entry(Path::new(rel), wanted.is_some()) {
                Err(err) if wanted.is_none() && filesystem::missing(&err) => continue,
                result => result?,
            };
            match wanted {
                None => entry.remove()?,
                Some(sig) => {
                    let src = self.source.entry(Path::new(rel), false)?;
                    if sig.directory() {
                        if entry.metadata()?.is_some_and(|meta| !meta.is_dir()) {
                            entry.remove()?;
                        }
                        fs::create_dir_all(entry.path())?;
                    } else {
                        entry.remove()?;
                        if sig.kind == "link" {
                            std::os::unix::fs::symlink(
                                std::ffi::OsStr::from_bytes(&sig.content),
                                entry.path(),
                            )?;
                        } else {
                            let mut input = src.open()?;
                            let mut output = fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(entry.path())?;
                            std::io::copy(&mut input, &mut output)?;
                        }
                    }
                    if Sig::of(&self.source, rel)?.as_ref() != Some(sig) {
                        return Err(refuse("source configuration changed during copy"));
                    }
                }
            }
        }
        // Ownership/mode/xattrs last: child creation must not change final directory ACLs.
        for (rel, sig) in self
            .operations
            .iter()
            .rev()
            .filter_map(|(rel, sig)| sig.as_ref().map(|sig| (rel, sig)))
        {
            let entry = target.entry(Path::new(rel), false)?;
            lchown(entry.path(), Some(sig.uid), Some(sig.gid))?;
            if sig.kind != "link" {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(sig.mode))?;
            }
            filesystem::set_xattrs(&entry.path(), &sig.xattrs)?;
        }
        Ok(())
    }
}

fn plan_path(
    source: &Root,
    target: &Root,
    rel: &str,
    base: Option<&Baseline>,
    out: &mut BTreeMap<String, Option<Sig>>,
) -> Result<()> {
    let own = Sig::of(source, rel)?;
    let old = base.and_then(|base| base.entries.get(rel));
    let new = Sig::of(target, rel)?;
    if own.as_ref().is_some_and(Sig::directory) && old.is_some_and(Sig::directory) {
        if new.as_ref().is_some_and(|sig| !sig.directory()) {
            return Err(refuse(format!(
                "release changes directory type at '{rel}'; conflict resolution is required"
            )));
        }
        if own.as_ref() != old {
            if new.as_ref() != old && new != own {
                return Err(refuse(format!("directory metadata conflicts at '{rel}'")));
            }
            out.insert(rel.into(), own.clone());
        }
        let mut children = names(source, rel)?
            .into_iter()
            .map(|name| format!("{rel}/{name}"))
            .collect::<BTreeSet<_>>();
        children.extend(
            base.unwrap()
                .entries
                .keys()
                .filter(|path| {
                    path.strip_prefix(&format!("{rel}/"))
                        .is_some_and(|suffix| !suffix.contains('/'))
                })
                .cloned(),
        );
        for child in children {
            plan_path(source, target, &child, base, out)?;
        }
        return Ok(());
    }
    if own.as_ref() == old || own == new {
        return Ok(());
    }
    if base.is_none() && new.is_some() {
        return Err(refuse(format!(
            "no complete baseline for changed configuration '{rel}'"
        )));
    }
    if new.as_ref() != old && new != own {
        return Err(refuse(format!(
            "local and release changes conflict at '{rel}'"
        )));
    }
    if own.as_ref().is_some_and(Sig::directory) {
        let mut tree = BTreeMap::new();
        walk(source, rel, &mut tree)?;
        for (path, sig) in tree {
            if let Err(err) = target.entry(Path::new(&path), false) {
                if !filesystem::missing(&err) {
                    return Err(err);
                }
            }
            out.insert(path, Some(sig));
        }
    } else {
        out.insert(rel.into(), own);
    }
    Ok(())
}

/// Conservative account carry until baseline-aware structured merging exists.
/// No member union or resurrection of an image account missing locally.
pub(crate) fn merge_accounts(
    image: &str,
    machine: &str,
    id_field: Option<usize>,
    member_field: Option<usize>,
) -> Result<String> {
    let parse = |text: &str| -> Result<BTreeMap<String, String>> {
        let mut records = BTreeMap::new();
        for line in text.lines().filter(|line| !line.is_empty()) {
            let fields = line.split(':').collect::<Vec<_>>();
            let name = fields[0];
            if name.is_empty()
                || fields.len() < member_field.or(id_field).unwrap_or(1) + 1
                || records.insert(name.into(), line.into()).is_some()
            {
                return Err(refuse("malformed or duplicate account record"));
            }
        }
        Ok(records)
    };
    let release = parse(image)?;
    let local = parse(machine)?;
    for (name, line) in &release {
        let Some(own) = local.get(name) else {
            return Err(refuse(format!(
                "account '{name}' is absent locally; structured baseline merging is required"
            )));
        };
        if let Some(index) = id_field {
            let release_id = line.split(':').nth(index).context("missing release account id")?
                .parse::<u32>().map_err(|_| refuse("invalid release account id"))?;
            let local_id = own.split(':').nth(index).context("missing local account id")?
                .parse::<u32>().map_err(|_| refuse("invalid local account id"))?;
            if release_id != local_id {
                return Err(refuse(format!("account '{name}' changes numeric identity; structured ownership migration is required")));
            }
        }
        if let Some(index) = member_field {
            if line.split(':').nth(index) != own.split(':').nth(index) {
                return Err(refuse(format!(
                    "account '{name}' has differing membership; refusing an ambiguous merge"
                )));
            }
        }
    }
    if let Some(index) = id_field {
        let mut ids = BTreeMap::new();
        for (name, line) in &local {
            let id: u32 = line
                .split(':')
                .nth(index)
                .context("missing account id")?
                .parse()
                .map_err(|_| refuse("invalid numeric account id"))?;
            if let Some(other) = ids.insert(id, name) {
                return Err(refuse(format!(
                    "accounts '{name}' and '{other}' share id {id}"
                )));
            }
            if release.iter().any(|(other, row)| {
                other != name
                    && row
                        .split(':')
                        .nth(index)
                        .and_then(|id| id.parse::<u32>().ok())
                        == Some(id)
            }) {
                return Err(refuse(format!(
                    "account '{name}' id {id} conflicts with a release account"
                )));
            }
        }
    }
    Ok(local.into_values().collect::<Vec<_>>().join("\n") + "\n")
}

/// The machine's fstab with its root line naming the partition `partuuid`
/// of filesystem `fs`; one without a root line is refused.
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
