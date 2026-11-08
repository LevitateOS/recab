//! The machine state an A/B update keeps: the image declares it (its
//! `usr/lib/recab/carried`, packaged with recab), and records what it ships
//! there (its `usr/lib/recab/baseline`, written when the image is built and
//! again by `stage` for the slot it writes). An update carries only what the
//! machine changed relative to its own image's baseline: a file it edited or
//! added wins, one it removed stays removed, the rest comes from the new
//! image. Nothing is written through a symlink inside the new slot.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{lchown, MetadataExt, PermissionsExt};
use std::path::{Component, Path};

use crate::error::refuse;

/// The list, relative to an image's root.
pub const CARRIED_LIST: &str = "usr/lib/recab/carried";
/// What the image shipped in the carried paths, relative to a slot's root.
pub const BASELINE: &str = "usr/lib/recab/baseline";

/// One entry of the list (its format is stated at the top of the list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carry {
    /// A file: the machine's when it changed it, else the image's.
    File(String),
    /// A directory's entries, each like a file.
    Dir(String),
    /// Every entry of a directory whose name starts with the last component.
    Prefix(String),
    /// An account file, merged by account name with the image's.
    Accounts(String),
    /// The fstab: the machine's, its root line naming the new slot.
    Fstab(String),
}

impl Carry {
    pub fn path(&self) -> &str {
        match self {
            Carry::File(p) | Carry::Dir(p) | Carry::Prefix(p) | Carry::Accounts(p) | Carry::Fstab(p) => p,
        }
    }
}

/// The list the image under `root` declares; one without a list, or with a
/// line that is not an entry, is refused (nothing has been changed yet).
pub fn read_list(root: &Path) -> Result<Vec<Carry>> {
    let path = root.join(CARRIED_LIST);
    let text = target_root::read_optional(&path)?.ok_or_else(|| {
        refuse(format!("the image declares no carried machine state ('{}' is missing)", path.display()))
    })?;
    parse_list(&text).map_err(|err| refuse(format!("'{}': {err:#}", path.display())))
}

fn parse_list(text: &str) -> Result<Vec<Carry>> {
    let mut entries = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kind, path) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let path = path.trim().to_string();
        if path.is_empty() || path.starts_with('/') || path.split('/').any(|c| c == "..") {
            bail!("line {}: '{line}' needs a path relative to / without '..'", number + 1);
        }
        entries.push(match kind {
            "file" => Carry::File(path),
            "dir" => Carry::Dir(path),
            "prefix" => Carry::Prefix(path),
            "accounts" => Carry::Accounts(path),
            "fstab" => Carry::Fstab(path),
            other => bail!("line {}: '{other}' is not file, dir, prefix, accounts or fstab", number + 1),
        });
    }
    Ok(entries)
}

/// What a path is, as a baseline records it: its type, owner and mode, and
/// a file's content hash or a link's target.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sig {
    Dir { mode: u32, uid: u32, gid: u32 },
    Link { target: String },
    File { mode: u32, uid: u32, gid: u32, sha256: String },
}

impl Sig {
    fn of(path: &Path) -> Result<Option<Self>> {
        let Some(meta) = target_root::metadata(path)? else { return Ok(None) };
        let (mode, uid, gid) = (meta.mode() & 0o7777, meta.uid(), meta.gid());
        Ok(Some(if meta.is_dir() {
            Sig::Dir { mode, uid, gid }
        } else if meta.file_type().is_symlink() {
            Sig::Link { target: fs::read_link(path)?.display().to_string() }
        } else {
            let content = fs::read(path).with_context(|| format!("reading '{}'", path.display()))?;
            Sig::File { mode, uid, gid, sha256: format!("{:x}", Sha256::digest(content)) }
        }))
    }

    fn is_dir(&self) -> bool {
        matches!(self, Sig::Dir { .. })
    }

    /// One baseline line: `kind<TAB>path<TAB>fields`.
    fn line(&self, path: &str) -> String {
        match self {
            Sig::Dir { mode, uid, gid } => format!("d\t{path}\t{mode:o} {uid} {gid}\n"),
            Sig::Link { target } => format!("l\t{path}\t{target}\n"),
            Sig::File { mode, uid, gid, sha256 } => format!("f\t{path}\t{mode:o} {uid} {gid} {sha256}\n"),
        }
    }

    fn parse(line: &str) -> Option<(String, Self)> {
        let mut fields = line.splitn(3, '\t');
        let (kind, path, rest) = (fields.next()?, fields.next()?, fields.next()?);
        let words: Vec<&str> = rest.split(' ').collect();
        let ids = |w: &[&str]| -> Option<(u32, u32, u32)> {
            Some((u32::from_str_radix(w.first()?, 8).ok()?, w.get(1)?.parse().ok()?, w.get(2)?.parse().ok()?))
        };
        let sig = match kind {
            "d" => {
                let (mode, uid, gid) = ids(&words)?;
                Sig::Dir { mode, uid, gid }
            }
            "l" => Sig::Link { target: rest.to_string() },
            "f" => {
                let (mode, uid, gid) = ids(&words)?;
                Sig::File { mode, uid, gid, sha256: words.get(3)?.to_string() }
            }
            _ => return None,
        };
        Some((path.to_string(), sig))
    }
}

/// A slot's baseline: path -> what its image shipped there.
pub(crate) struct Baseline(BTreeMap<String, Sig>);

impl Baseline {
    /// The baseline of the slot under `root`, `None` when it has none (a
    /// slot installed before images carried one).
    pub fn read(root: &Path) -> Result<Option<Self>> {
        let path = root.join(BASELINE);
        let Some(text) = target_root::read_optional(&path)? else { return Ok(None) };
        let mut map = BTreeMap::new();
        for line in text.lines() {
            let (p, sig) = Sig::parse(line)
                .ok_or_else(|| refuse(format!("'{}': '{line}' is not a baseline entry", path.display())))?;
            map.insert(p, sig);
        }
        Ok(Some(Self(map)))
    }

    /// Record what the image under `root` ships in every path of `list`.
    pub fn write(root: &Path, list: &[Carry]) -> Result<()> {
        let mut text = String::new();
        for entry in list {
            for rel in listed_paths(root, entry)? {
                walk(root, &rel, &mut |path, sig| text.push_str(&sig.line(path)))?;
            }
        }
        // The slot is flushed whole after it is written: no durable rename.
        let path = root.join(BASELINE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, text).with_context(|| format!("writing '{}'", path.display()))
    }

    fn get(&self, rel: &str) -> Option<&Sig> {
        self.0.get(rel)
    }
}

/// The paths an entry names under `root` (a prefix entry names the
/// matching entries of its directory).
fn listed_paths(root: &Path, entry: &Carry) -> Result<Vec<String>> {
    Ok(match entry {
        Carry::Prefix(rel) => {
            let (dir, prefix) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
            target_root::dir_names(&root.join(dir))?
                .into_iter()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| name.starts_with(prefix))
                .map(|name| if dir.is_empty() { name } else { format!("{dir}/{name}") })
                .collect()
        }
        other => vec![other.path().to_string()],
    })
}

/// Every path under `root/rel` (itself included), sorted, with its signature.
fn walk(root: &Path, rel: &str, visit: &mut impl FnMut(&str, &Sig)) -> Result<()> {
    let Some(sig) = Sig::of(&root.join(rel))? else { return Ok(()) };
    visit(rel, &sig);
    if sig.is_dir() {
        let mut names = target_root::dir_names(&root.join(rel))?;
        names.sort();
        for name in names {
            walk(root, &format!("{rel}/{}", name.to_string_lossy()), visit)?;
        }
    }
    Ok(())
}

/// Refuse a destination under the new slot `to` whose existing parent
/// components include a symlink: writing through it could leave the slot.
fn check_inside(to: &Path, rel: &str) -> Result<()> {
    let mut at = to.to_path_buf();
    let parent = Path::new(rel).parent().unwrap_or(Path::new(""));
    for component in parent.components() {
        let Component::Normal(name) = component else { continue };
        at.push(name);
        match target_root::metadata(&at)? {
            Some(meta) if meta.file_type().is_symlink() => {
                return Err(refuse(format!(
                    "the new image has a symlink at '{}' where the machine's state is carried; refusing to write through it",
                    at.display()
                )))
            }
            Some(_) => {}
            None => break,
        }
    }
    Ok(())
}

/// Carry the machine state `list` names from the running root `from` into
/// the new slot `to`, by the running slot's `baseline`. Account files and
/// the fstab are not carried here: stage's checks prepared them.
pub(crate) fn carry(from: &Path, to: &Path, list: &[Carry], baseline: Option<&Baseline>) -> Result<()> {
    for entry in list {
        if matches!(entry, Carry::Accounts(_) | Carry::Fstab(_)) {
            continue;
        }
        if let Carry::Dir(rel) = entry {
            if target_root::metadata(&from.join(rel))?.is_some_and(|m| !m.is_dir()) {
                return Err(refuse(format!("'{}' is listed as a directory to carry but is not one", from.join(rel).display())));
            }
        }
        let mut paths = listed_paths(from, entry)?;
        // What the machine removed of what its image shipped.
        if let Some(baseline) = baseline {
            for rel in listed_paths_in_baseline(baseline, entry) {
                if !paths.contains(&rel) {
                    paths.push(rel);
                }
            }
        }
        for rel in paths {
            carry_path(from, to, &rel, baseline)?;
        }
    }
    Ok(())
}

/// The baseline's paths an entry names (for a prefix, the matching ones).
fn listed_paths_in_baseline(baseline: &Baseline, entry: &Carry) -> Vec<String> {
    let rel = entry.path();
    match entry {
        Carry::Prefix(_) => baseline.0.keys().filter(|p| p.starts_with(rel) && !p[rel.len()..].contains('/')).cloned().collect(),
        _ => baseline.0.contains_key(rel).then(|| rel.to_string()).into_iter().collect(),
    }
}

/// Carry one path (a file, link, or directory walked entry by entry).
fn carry_path(from: &Path, to: &Path, rel: &str, baseline: Option<&Baseline>) -> Result<()> {
    let machine = Sig::of(&from.join(rel))?;
    let shipped = baseline.and_then(|b| b.get(rel));
    match (&machine, shipped) {
        // Removed by the machine: removed from the new slot too.
        (None, Some(_)) => remove(to, rel),
        (None, None) => Ok(()),
        // A directory the image also shipped: its entries one by one, so
        // only the machine's changes are carried.
        (Some(m), Some(s)) if m.is_dir() && s.is_dir() => {
            if m != s {
                copy_entry(from, to, rel)?;
            }
            let mut names: Vec<String> =
                target_root::dir_names(&from.join(rel))?.into_iter().map(|n| format!("{rel}/{}", n.to_string_lossy())).collect();
            for shipped_child in baseline.expect("shipped").0.keys() {
                let is_child = shipped_child.strip_prefix(rel).and_then(|r| r.strip_prefix('/')).is_some_and(|r| !r.contains('/'));
                if is_child && !names.contains(shipped_child) {
                    names.push(shipped_child.clone());
                }
            }
            for child in names {
                carry_path(from, to, &child, baseline)?;
            }
            Ok(())
        }
        // Unchanged since the image shipped it: the new image's stays.
        (Some(m), Some(s)) if m == s => Ok(()),
        // Without a baseline (a slot installed before images carried one)
        // only what the new image lacks is the machine's for sure.
        (Some(_), None) if baseline.is_none() && target_root::metadata(&to.join(rel))?.is_some() => {
            if machine.as_ref().is_some_and(Sig::is_dir) {
                for name in target_root::dir_names(&from.join(rel))? {
                    carry_path(from, to, &format!("{rel}/{}", name.to_string_lossy()), None)?;
                }
            }
            Ok(())
        }
        // Changed or added by the machine: the machine's wins.
        _ => copy_tree(from, to, rel),
    }
}

fn remove(to: &Path, rel: &str) -> Result<()> {
    check_inside(to, rel)?;
    let gone = to.join(rel);
    match target_root::metadata(&gone)? {
        Some(m) if m.is_dir() => fs::remove_dir_all(&gone),
        Some(_) => fs::remove_file(&gone),
        None => Ok(()),
    }
    .with_context(|| format!("removing '{}'", gone.display()))
}

/// Copy `from/rel` (and, for a directory, everything under it) over
/// `to/rel`, keeping symlinks, owners, modes and extended attributes. Done
/// here rather than with `cp`, whose options differ between GNU coreutils
/// and busybox.
fn copy_tree(from: &Path, to: &Path, rel: &str) -> Result<()> {
    copy_entry(from, to, rel)?;
    if target_root::metadata(&from.join(rel))?.is_some_and(|m| m.is_dir()) {
        for name in target_root::dir_names(&from.join(rel))? {
            copy_tree(from, to, &format!("{rel}/{}", name.to_string_lossy()))?;
        }
    }
    Ok(())
}

/// Copy one entry (a directory without its contents).
fn copy_entry(from: &Path, to: &Path, rel: &str) -> Result<()> {
    check_inside(to, rel)?;
    let (source, dest) = (from.join(rel), to.join(rel));
    let context = || format!("copying '{}' to '{}'", source.display(), dest.display());
    let meta = fs::symlink_metadata(&source).with_context(context)?;
    let existing = target_root::metadata(&dest).with_context(context)?;
    if meta.is_dir() {
        if existing.as_ref().is_some_and(|e| !e.is_dir()) {
            fs::remove_file(&dest).with_context(context)?;
        }
        fs::create_dir_all(&dest).with_context(context)?;
    } else {
        match existing {
            Some(e) if e.is_dir() => fs::remove_dir_all(&dest).with_context(context)?,
            Some(_) => fs::remove_file(&dest).with_context(context)?,
            None => {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).with_context(context)?;
                }
            }
        }
        if meta.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&source)?, &dest).with_context(context)?;
        } else {
            fs::copy(&source, &dest).with_context(context)?;
        }
    }
    lchown(&dest, Some(meta.uid()), Some(meta.gid())).with_context(context)?;
    if !meta.file_type().is_symlink() {
        fs::set_permissions(&dest, fs::Permissions::from_mode(meta.mode() & 0o7777)).with_context(context)?;
    }
    copy_xattrs(&source, &dest).with_context(context)
}

/// Copy `source`'s extended attributes (SELinux labels, capabilities, ACLs)
/// onto `dest`, not following links.
fn copy_xattrs(source: &Path, dest: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| CString::new(p.as_os_str().as_bytes());
    let (src, dst) = (c(source)?, c(dest)?);
    // SAFETY: each call reads or writes buffers this function owns, sized
    // by the length the previous call returned.
    unsafe {
        let len = libc::llistxattr(src.as_ptr(), std::ptr::null_mut(), 0);
        if len < 0 {
            let err = std::io::Error::last_os_error();
            // A filesystem without extended attributes has none to copy.
            return if err.raw_os_error() == Some(libc::ENOTSUP) { Ok(()) } else { Err(err.into()) };
        }
        let mut names = vec![0u8; len as usize];
        let len = libc::llistxattr(src.as_ptr(), names.as_mut_ptr().cast(), names.len());
        if len < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        for name in names[..len as usize].split(|b| *b == 0).filter(|n| !n.is_empty()) {
            let name = CString::new(name)?;
            let size = libc::lgetxattr(src.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0);
            if size < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut value = vec![0u8; size as usize];
            let size = libc::lgetxattr(src.as_ptr(), name.as_ptr(), value.as_mut_ptr().cast(), value.len());
            if size < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if libc::lsetxattr(dst.as_ptr(), name.as_ptr(), value.as_ptr().cast(), size as usize, 0) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    Ok(())
}

/// An account file (`name:...` lines) merged from the image's and the
/// machine's: the image's lines in their order with the machine's line for
/// the same name in their place (a group's members joined), then the
/// machine's other lines. A machine account whose id the image gives to
/// another name is refused: the two would share an id. `id_field` is the
/// field holding the numeric id (2 for passwd and group), or `None`
/// (shadow files); `member_field` the group member list (3 in group and
/// gshadow).
pub(crate) fn merge_accounts(
    image: &str,
    machine: &str,
    id_field: Option<usize>,
    member_field: Option<usize>,
) -> Result<String> {
    let name = |line: &str| line.split(':').next().unwrap_or("").to_string();
    let field = |line: &str, i: usize| line.split(':').nth(i).unwrap_or("").to_string();
    let machine_lines: BTreeMap<String, &str> = machine.lines().filter(|l| !l.is_empty()).map(|l| (name(l), l)).collect();
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for line in image.lines().filter(|l| !l.is_empty()) {
        let n = name(line);
        let merged = match machine_lines.get(&n) {
            Some(own) => match member_field {
                Some(i) => {
                    let mut fields: Vec<String> = own.split(':').map(str::to_string).collect();
                    let mut members: Vec<String> = field(own, i).split(',').filter(|m| !m.is_empty()).map(str::to_string).collect();
                    for m in field(line, i).split(',').filter(|m| !m.is_empty()) {
                        if !members.iter().any(|x| x == m) {
                            members.push(m.to_string());
                        }
                    }
                    if fields.len() > i {
                        fields[i] = members.join(",");
                    }
                    fields.join(":")
                }
                None => own.to_string(),
            },
            None => line.to_string(),
        };
        out.push(merged);
        seen.push(n);
    }
    for (n, own) in &machine_lines {
        if seen.contains(n) {
            continue;
        }
        if let Some(i) = id_field {
            let id = field(own, i);
            if let Some(other) = image.lines().find(|l| field(l, i) == id && name(l) != *n) {
                return Err(refuse(format!(
                    "the machine's account '{n}' has id {id}, which the new image gives to '{}'; staging would give two accounts one id",
                    name(other)
                )));
            }
        }
        out.push(own.to_string());
    }
    Ok(out.join("\n") + "\n")
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
