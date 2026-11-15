//! Make one root an exact copy of another, in place: only what differs is
//! written, and what the source lacks is removed. The caller holds the recab
//! lock and the destination is not booted. Each root keeps its own fstab and
//! slot marker. Every run compares actual contents again.
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::error::refuse;
use crate::filesystem::{self, Entry, Root};
use crate::immutable::SLOT_FILES;
use crate::inventory::{digest, extents, Extent, Inventory, Node, EXCLUDED};
use crate::Slot;

const RANGE: u64 = 1024 * 1024;

/// What a copy wrote: logical writes, not SSD/controller write amplification.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SyncReport {
    pub payload_bytes_written: u64,
    pub payload_bytes_compared: u64,
    pub range_writes: u64,
    pub hole_bytes_punched: u64,
    pub files_created: u64,
    pub links_created: u64,
    pub entries_removed: u64,
    pub metadata_updates: u64,
}

#[derive(Debug)]
enum Operation {
    Remove(Vec<u8>),
    Directory(Vec<u8>),
    File(Vec<u8>),
    Link(Vec<u8>),
    Symlink(Vec<u8>),
    Metadata(Vec<u8>),
}
pub(crate) struct Synchronization {
    source: Root,
    destination: Root,
    desired: Inventory,
    observed: Inventory,
    operations: Vec<Operation>,
    slot: Slot,
    partuuid: String,
    other: String,
    boundaries: BTreeSet<Vec<u8>>,
}

/// The scratch name a detached file is written under before its rename.
const TEMPORARY: &str = ".recab-copy";

fn path(bytes: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(bytes))
}
fn entry(root: &Root, bytes: &[u8]) -> Result<Entry> {
    if bytes.is_empty() {
        root.root_entry()
    } else {
        root.entry(path(bytes), false)
    }
}
fn owned(bytes: &[u8]) -> bool {
    SLOT_FILES.contains(&bytes)
}
fn ranges(node: &Node) -> Vec<Extent> {
    node.extents.clone().unwrap_or_else(|| {
        if node.size == 0 {
            vec![]
        } else {
            vec![Extent {
                offset: 0,
                length: node.size,
            }]
        }
    })
}
fn allocated(node: &Node) -> u64 {
    ranges(node).iter().map(|e| e.length).sum()
}
fn holes(node: &Node) -> Vec<Extent> {
    let mut end = 0;
    let mut holes = Vec::new();
    for e in ranges(node) {
        if e.offset > end {
            holes.push(Extent {
                offset: end,
                length: e.offset - end,
            });
        }
        end = e.offset + e.length;
    }
    if end < node.size {
        holes.push(Extent {
            offset: end,
            length: node.size - end,
        });
    }
    holes
}
fn overlap(a: &Extent, b: &Extent) -> Option<Extent> {
    let start = a.offset.max(b.offset);
    let end = (a.offset + a.length).min(b.offset + b.length);
    (start < end).then_some(Extent {
        offset: start,
        length: end.saturating_sub(start),
    })
}
fn depth(p: &[u8]) -> usize {
    p.iter().filter(|c| **c == b'/').count()
}
fn same_metadata(a: &Node, b: &Node) -> bool {
    a.kind == b.kind
        && a.mode == b.mode
        && a.uid == b.uid
        && a.gid == b.gid
        && a.modified == b.modified
        && a.modified_ns == b.modified_ns
        && a.xattrs == b.xattrs
}
fn has_holes_to_punch(wanted: &Node, old: &Node) -> bool {
    old.kind == "file"
        && holes(wanted)
            .iter()
            .any(|h| ranges(old).iter().any(|e| overlap(h, e).is_some()))
}
fn metadata_matches(node: &Node, entry: &Entry) -> Result<bool> {
    let Some(m) = entry.metadata()? else {
        return Ok(false);
    };
    Ok(m.mode() & 0o7777 == node.mode
        && m.uid() == node.uid
        && m.gid() == node.gid
        && m.mtime() == node.modified
        && m.mtime_nsec() == node.modified_ns
        && filesystem::xattrs(&entry.path())? == node.xattrs)
}
fn metadata(node: &Node, entry: &Entry, report: &mut SyncReport) -> Result<()> {
    if metadata_matches(node, entry)? {
        return Ok(());
    }
    let m = entry.metadata()?.context("metadata target disappeared")?;
    if m.uid() != node.uid || m.gid() != node.gid {
        std::os::unix::fs::lchown(entry.path(), Some(node.uid), Some(node.gid))?;
    }
    if node.kind != "link" && entry.metadata()?.unwrap().mode() & 0o7777 != node.mode {
        fs::set_permissions(entry.path(), fs::Permissions::from_mode(node.mode))?;
    }
    // Ownership and content changes can clear capabilities. Restore xattrs last.
    filesystem::set_xattrs(&entry.path(), &node.xattrs)?;
    let name = std::ffi::CString::new(entry.path().as_os_str().as_bytes())?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: node.modified,
            tv_nsec: node.modified_ns,
        },
    ];
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("restoring system modification time");
    }
    report.metadata_updates += 1;
    Ok(())
}

/// `bytes` (fstab or the slot marker) as the destination root `slot` (its
/// PARTUUID `uuid`, the other root's `other`) has it.
fn materialized(
    source: &Root,
    bytes: &[u8],
    slot: Slot,
    uuid: &str,
    other: &str,
) -> Result<Vec<u8>> {
    let text = source
        .optional_text(path(bytes))?
        .context("slot identity file is missing")?;
    if text.len() > RANGE as usize {
        return Err(refuse("slot identity file exceeds 1 MiB"));
    }
    Ok(if bytes == b"etc/fstab" {
        crate::fstab::for_slot(&text, uuid, "ext4")?.into_bytes()
    } else {
        let fields = text.split_whitespace().collect::<Vec<_>>();
        let state = fields
            .get(1)
            .ok_or_else(|| refuse("the slot marker does not name the shared /var"))?;
        crate::immutable::slot_marker(slot, state, other).into_bytes()
    })
}

/// The booted root's side of a copy: checked and with the destination's
/// own files already worked out. Nothing here touches the destination.
pub(crate) struct Prepared {
    source: std::path::PathBuf,
    desired: Inventory,
    boundaries: BTreeSet<Vec<u8>>,
    slot: Slot,
    partuuid: String,
    other: String,
}

impl Prepared {
    /// The mount points left out of the copy.
    pub fn boundaries(&self) -> &BTreeSet<Vec<u8>> {
        &self.boundaries
    }
}

/// The destination as read: its root and what it holds.
pub(crate) struct Observed {
    root: Root,
    inventory: Inventory,
}

impl Synchronization {
    /// Check the source (read as `desired`, without `boundaries`) for a copy
    /// into root `slot` (PARTUUID `uuid`; `other`: the source root's
    /// PARTUUID) and work out that root's own fstab and slot marker.
    pub fn prepare(
        source: &Path,
        mut desired: Inventory,
        boundaries: BTreeSet<Vec<u8>>,
        slot: Slot,
        uuid: &str,
        other: &str,
    ) -> Result<Prepared> {
        let source_root = Root::open(source)?;
        for node in &desired.entries {
            if path(&node.path)
                .components()
                .any(|c| c.as_os_str() == TEMPORARY)
            {
                return Err(refuse("the source uses recab's scratch name"));
            }
            if owned(&node.path)
                && (node.kind != "file"
                    || node.hardlink.is_some()
                    || desired
                        .entries
                        .iter()
                        .any(|n| n.hardlink.as_ref() == Some(&node.path)))
            {
                return Err(refuse(
                    "slot identity files must be independent regular files",
                ));
            }
        }
        for node in &mut desired.entries {
            if owned(&node.path) {
                let bytes = materialized(&source_root, &node.path, slot, uuid, other)?;
                node.content = Some(digest(&bytes));
                node.size = bytes.len() as u64;
                node.extents = Some(if bytes.is_empty() {
                    vec![]
                } else {
                    vec![Extent {
                        offset: 0,
                        length: bytes.len() as u64,
                    }]
                });
            }
        }
        Ok(Prepared {
            source: source.to_path_buf(),
            desired,
            boundaries,
            slot,
            partuuid: uuid.into(),
            other: other.into(),
        })
    }

    /// Read the destination. An error here means the destination itself
    /// cannot be opened or read.
    pub fn observe(destination: &Path, boundaries: &BTreeSet<Vec<u8>>) -> Result<Observed> {
        Ok(Observed {
            root: Root::open(destination)?,
            inventory: Inventory::capture(destination, boundaries)?,
        })
    }

    /// Plan the copy of `prepared` into the destination as `observed`,
    /// checking it fits. Changes nothing.
    pub fn plan(prepared: &Prepared, observed: Observed) -> Result<Self> {
        let source_root = Root::open(&prepared.source)?;
        let desired = prepared.desired.clone();
        let Observed {
            root: destination_root,
            inventory: observed,
        } = observed;
        let source_meta = source_root.root_entry()?.metadata()?.unwrap();
        let destination_meta = destination_root.root_entry()?.metadata()?.unwrap();
        if (source_meta.dev(), source_meta.ino())
            == (destination_meta.dev(), destination_meta.ino())
        {
            return Err(refuse("source and destination identify the same root"));
        }
        let wanted: BTreeMap<_, _> = desired
            .entries
            .iter()
            .map(|n| (n.path.clone(), n))
            .collect();
        let actual: BTreeMap<_, _> = observed
            .entries
            .iter()
            .map(|n| (n.path.clone(), n))
            .collect();
        let mut removals = observed
            .entries
            .iter()
            .filter(|n| {
                !n.path.is_empty() && wanted.get(&n.path).is_none_or(|want| want.kind != n.kind)
            })
            .map(|n| n.path.clone())
            .collect::<Vec<_>>();
        // An excluded real directory was not traversed; never recursively erase it
        // to turn it into a symlink, even when the desired source has that link.
        for node in &desired.entries {
            if !node.path.contains(&b'/')
                && EXCLUDED.contains(&node.path.as_slice())
                && destination_root
                    .metadata(path(&node.path))?
                    .is_some_and(|m| m.is_dir())
                && node.kind != "dir"
            {
                return Err(refuse(
                    "cannot replace a shared/runtime directory with a system entry",
                ));
            }
        }
        removals.sort_by(|a, b| depth(b).cmp(&depth(a)).then(b.cmp(a)));
        let mut operations = removals
            .into_iter()
            .map(Operation::Remove)
            .collect::<Vec<_>>();
        // Directories first, including empty placeholders for excluded mount roots.
        for node in desired.entries.iter().filter(|n| n.kind == "dir") {
            if actual.get(&node.path).is_none_or(|old| old.kind != "dir") {
                operations.push(Operation::Directory(node.path.clone()));
            }
        }
        for name in EXCLUDED.iter().filter(|n| **n != b"lost+found") {
            if source_root
                .metadata(path(name))?
                .is_some_and(|m| m.is_dir())
            {
                if destination_root
                    .metadata(path(name))?
                    .is_some_and(|m| !m.is_dir())
                {
                    return Err(refuse(
                        "shared/runtime mount point has an incompatible type",
                    ));
                }
                if destination_root.metadata(path(name))?.is_none() {
                    operations.push(Operation::Directory(name.to_vec()));
                }
            }
        }
        // Mount points below either root are boundaries: the folder is kept,
        // nothing below it is read, copied or removed.
        let mut boundaries = prepared
            .boundaries
            .iter()
            .filter(|b| {
                let first = b.split(|c| *c == b'/').next().unwrap_or_default();
                !EXCLUDED.contains(&first)
                    && !prepared
                        .boundaries
                        .iter()
                        .any(|a| a != *b && b.starts_with(a) && b.get(a.len()) == Some(&b'/'))
            })
            .collect::<Vec<_>>();
        boundaries.sort_by_key(|b| depth(b));
        for name in boundaries {
            let source_dir = matches!(source_root.metadata(path(name)), Ok(Some(m)) if m.is_dir());
            let parent_wanted = path(name).parent().is_none_or(|p| {
                p.as_os_str().is_empty() || wanted.contains_key(p.as_os_str().as_bytes())
            });
            if !source_dir || !parent_wanted {
                continue;
            }
            match destination_root.metadata(path(name)) {
                Ok(Some(m)) if !m.is_dir() => {
                    return Err(refuse(format!(
                        "'{}' is a mount point in the booted root, but not a folder in the other root",
                        path(name).display()
                    )));
                }
                Ok(Some(_)) => {}
                _ => operations.push(Operation::Directory(name.to_vec())),
            }
        }
        let mut patched = BTreeSet::new();
        for node in &desired.entries {
            let old = actual.get(&node.path);
            match node.kind.as_str() {
                "file"
                    if node.hardlink.is_none()
                        && old.is_none_or(|old| {
                            old.kind != "file"
                                || old.content != node.content
                                || old.size != node.size
                                || old.hardlink != node.hardlink
                                || !same_metadata(old, node)
                                || has_holes_to_punch(node, old)
                        }) =>
                {
                    operations.push(Operation::File(node.path.clone()));
                    patched.insert(node.path.clone());
                }
                "link"
                    if node.hardlink.is_none()
                        && old.is_none_or(|old| {
                            old.kind != "link"
                                || old.link != node.link
                                || old.hardlink != node.hardlink
                                || !same_metadata(old, node)
                        }) =>
                {
                    operations.push(Operation::Symlink(node.path.clone()));
                    patched.insert(node.path.clone());
                }
                _ => {}
            }
        }
        for node in desired.entries.iter().filter(|n| n.hardlink.is_some()) {
            if actual
                .get(&node.path)
                .is_none_or(|old| old.kind != "file" || old.hardlink != node.hardlink)
                || patched.contains(node.hardlink.as_ref().unwrap())
            {
                operations.push(Operation::Link(node.path.clone()));
                patched.insert(node.path.clone());
            }
        }
        let mut parents = BTreeSet::new();
        for op in &operations {
            let p = match op {
                Operation::Remove(p)
                | Operation::Directory(p)
                | Operation::File(p)
                | Operation::Symlink(p)
                | Operation::Link(p)
                | Operation::Metadata(p) => p,
            };
            if let Some(parent) = path(p).parent() {
                parents.insert(parent.as_os_str().as_bytes().to_vec());
            }
        }
        // Only record actual changes and affected parents, not every unchanged
        // file in the system. This also bounds journal writes on equal roots.
        for node in desired.entries.iter().rev() {
            if actual
                .get(&node.path)
                .is_none_or(|old| !same_metadata(old, node))
                || patched.contains(&node.path)
                || (node.kind == "dir" && parents.contains(&node.path))
            {
                operations.push(Operation::Metadata(node.path.clone()));
            }
        }
        let result = Self {
            source: source_root,
            destination: destination_root,
            desired,
            observed,
            operations,
            slot: prepared.slot,
            partuuid: prepared.partuuid.clone(),
            other: prepared.other.clone(),
            boundaries: prepared.boundaries.clone(),
        };
        result.capacity()?;
        Ok(result)
    }

    fn capacity(&self) -> Result<()> {
        let actual: BTreeMap<_, _> = self
            .observed
            .entries
            .iter()
            .map(|n| (n.path.clone(), n))
            .collect();
        let root_entry = self.destination.root_entry()?;
        let root_file = File::open(root_entry.path())?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatvfs(root_file.as_raw_fd(), &mut stat) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let block = stat.f_frsize.max(1);
        let round = |bytes: u64| bytes.saturating_add(block - 1) / block * block;
        let mut bytes = 0u64;
        let mut inodes = 0u64;
        for node in self
            .desired
            .entries
            .iter()
            .filter(|n| n.kind == "file" && n.hardlink.is_none())
        {
            let old = actual.get(&node.path).filter(|old| old.kind == "file");
            let existing = if actual.contains_key(&node.path) {
                self.destination.metadata(path(&node.path))?
            } else {
                None
            };
            let different =
                old.is_none_or(|old| old.content != node.content || old.size != node.size);
            let sparse = old.is_some_and(|old| {
                holes(node)
                    .iter()
                    .any(|h| ranges(old).iter().any(|e| overlap(h, e).is_some()))
            });
            let meta_differs = if existing.is_some() {
                !metadata_matches(node, &entry(&self.destination, &node.path)?)?
            } else {
                true
            };
            let detach = match &existing {
                Some(m) if m.is_file() && !self.safe_aliases(node, m)? => {
                    different
                        || sparse
                        || meta_differs
                        || old.is_some_and(|old| old.hardlink != node.hardlink)
                }
                _ => false,
            };
            if old.is_none() || detach {
                bytes = bytes.saturating_add(round(allocated(node)));
                inodes += 1;
            } else if different {
                for range in ranges(node) {
                    let reused = old
                        .map(|old| {
                            ranges(old)
                                .iter()
                                .filter_map(|e| overlap(&range, e))
                                .map(|e| e.length)
                                .sum::<u64>()
                        })
                        .unwrap_or(0);
                    bytes = bytes.saturating_add(round(range.length.saturating_sub(reused)));
                }
            }
            if meta_differs {
                bytes = bytes.saturating_add(round(
                    node.xattrs
                        .iter()
                        .map(|(k, v)| (k.len() + v.len()) as u64)
                        .sum(),
                ));
            }
        }
        inodes += self
            .operations
            .iter()
            .filter(|op| matches!(op, Operation::Directory(_) | Operation::Symlink(_)))
            .count() as u64;
        // Bounded directory/identity scratch, not another expanded root tree.
        bytes = bytes
            .saturating_add(inodes.saturating_mul(block))
            .saturating_add(16 * 1024);
        // Deletions run first. Credit only inodes whose complete link set is
        // removed; an excluded/cache alias retains both the inode and its data.
        let mut groups: BTreeMap<(u64, u64), (u64, u64, u64)> = BTreeMap::new();
        let mut freed_bytes = 0u64;
        let mut freed_inodes = 0u64;
        for op in &self.operations {
            if let Operation::Remove(p) = op {
                let m = entry(&self.destination, p)?
                    .metadata()?
                    .context("deletion target disappeared")?;
                if m.is_dir() {
                    freed_inodes += 1;
                    freed_bytes += m.blocks() * 512;
                } else {
                    let g = groups.entry((m.dev(), m.ino())).or_insert((
                        m.nlink(),
                        m.blocks() * 512,
                        0,
                    ));
                    g.2 += 1;
                }
            }
        }
        for (_, (links, blocks, count)) in groups {
            if links == count {
                freed_bytes += blocks;
                freed_inodes += 1;
            }
        }
        if bytes
            > stat
                .f_bavail
                .saturating_mul(block)
                .saturating_add(freed_bytes)
            || inodes > stat.f_favail.saturating_add(freed_inodes)
        {
            return Err(refuse(format!(
                "synchronization needs up to {bytes} free bytes and {inodes} inodes; destination has {} bytes and {} inodes",
                stat.f_bavail.saturating_mul(block),
                stat.f_favail
            )));
        }
        Ok(())
    }

    fn safe_aliases(&self, node: &Node, m: &fs::Metadata) -> Result<bool> {
        if m.nlink() == 1 {
            return Ok(true);
        }
        let mut count = 0;
        for member in self.desired.entries.iter().filter(|n| {
            n.kind == node.kind && (n.path == node.path || n.hardlink.as_ref() == Some(&node.path))
        }) {
            let alias = match self.destination.metadata(path(&member.path)) {
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                        matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP))
                    }) =>
                {
                    None
                }
                result => result?,
            };
            if let Some(alias) = alias {
                if alias.ino() == m.ino() && alias.dev() == m.dev() {
                    count += 1;
                }
            }
        }
        Ok(count == m.nlink())
    }

    fn file(&self, node: &Node, old: Option<&Node>, report: &mut SyncReport) -> Result<()> {
        let target = entry(&self.destination, &node.path)?;
        let m = target.metadata()?;
        let data_differs = old.is_none_or(|old| {
            old.kind != "file" || old.content != node.content || old.size != node.size
        });
        let hole_change = old.is_some_and(|old| {
            holes(node)
                .iter()
                .any(|h| ranges(old).iter().any(|data| overlap(h, data).is_some()))
        });
        let meta_differs = !metadata_matches(node, &target)?;
        let detach = match &m {
            Some(m) if !self.safe_aliases(node, m)? => {
                data_differs
                    || hole_change
                    || meta_differs
                    || old.is_some_and(|old| old.hardlink != node.hardlink)
            }
            _ => false,
        };
        if !data_differs && !hole_change && !detach {
            return Ok(());
        }
        let temporary_path = path(&node.path).with_file_name(TEMPORARY);
        let write_target = if detach {
            self.destination.entry(&temporary_path, false)?
        } else {
            entry(&self.destination, &node.path)?
        };
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o600);
        if detach || m.is_none() {
            options.create_new(true);
        }
        let mut output = options.open(write_target.path())?;
        if !output.metadata()?.is_file() {
            return Err(refuse("synchronization write target is not a regular file"));
        }
        if detach || m.is_none() {
            report.files_created += 1;
        }
        if output.metadata()?.len() != node.size {
            output.set_len(node.size)?;
        }
        let allocated_now = extents(&output, node.size)?.unwrap_or_else(|| {
            vec![Extent {
                offset: 0,
                length: node.size,
            }]
        });
        for hole in holes(node) {
            for allocation in &allocated_now {
                if let Some(punch) = overlap(&hole, allocation) {
                    if unsafe {
                        libc::fallocate(
                            output.as_raw_fd(),
                            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                            punch.offset as i64,
                            punch.length as i64,
                        )
                    } != 0
                    {
                        return Err(std::io::Error::last_os_error())
                            .context("preserving sparse holes");
                    }
                    report.hole_bytes_punched += punch.length;
                }
            }
        }
        trait Input: Read + Seek {}
        impl<T: Read + Seek> Input for T {}
        let mut input: Box<dyn Input> = if owned(&node.path) {
            Box::new(std::io::Cursor::new(materialized(
                &self.source,
                &node.path,
                self.slot,
                &self.partuuid,
                &self.other,
            )?))
        } else {
            Box::new(entry(&self.source, &node.path)?.open()?)
        };
        let mut wanted = vec![0u8; RANGE as usize];
        let mut present = vec![0u8; RANGE as usize];
        for extent in ranges(node) {
            let mut offset = extent.offset;
            let end = extent.offset + extent.length;
            while offset < end {
                let length = (end - offset).min(RANGE - offset % RANGE) as usize;
                input.seek(SeekFrom::Start(offset))?;
                input.read_exact(&mut wanted[..length])?;
                output.seek(SeekFrom::Start(offset))?;
                output.read_exact(&mut present[..length])?;
                report.payload_bytes_compared += length as u64;
                if wanted[..length] != present[..length] {
                    output.seek(SeekFrom::Start(offset))?;
                    output.write_all(&wanted[..length])?;
                    report.payload_bytes_written += length as u64;
                    report.range_writes += 1;
                }
                offset += length as u64;
            }
        }
        output.sync_all()?;
        if detach {
            fs::rename(write_target.path(), target.path())?;
        }
        Ok(())
    }

    pub fn apply(&self) -> Result<SyncReport> {
        let wanted: BTreeMap<_, _> = self
            .desired
            .entries
            .iter()
            .map(|n| (n.path.as_slice(), n))
            .collect();
        let actual: BTreeMap<_, _> = self
            .observed
            .entries
            .iter()
            .map(|n| (n.path.as_slice(), n))
            .collect();
        let mut report = SyncReport::default();
        let mut removed = false;
        for op in &self.operations {
            if removed && !matches!(op, Operation::Remove(_)) {
                // Make reclaimed blocks/inodes available before allocation.
                filesystem::flush(&self.destination)?;
                removed = false;
            }
            (|| -> Result<()> {
                match op {
                    Operation::Remove(p) => {
                        let e = entry(&self.destination, p)?;
                        if let Some(m) = e.metadata()? {
                            if m.is_dir() {
                                fs::remove_dir(e.path())?;
                            } else {
                                fs::remove_file(e.path())?;
                            }
                            report.entries_removed += 1;
                            removed = true;
                        }
                    }
                    Operation::Directory(p) => {
                        let e = entry(&self.destination, p)?;
                        if e.metadata()?.is_none() {
                            fs::create_dir(e.path())?;
                            report.files_created += 1;
                            if !wanted.contains_key(p.as_slice()) {
                                let mode = self
                                    .source
                                    .metadata(path(p))?
                                    .context("mount root disappeared")?
                                    .mode()
                                    & 0o7777;
                                fs::set_permissions(e.path(), fs::Permissions::from_mode(mode))?;
                            }
                        }
                        let m = e.metadata()?.unwrap();
                        if !m.is_dir() {
                            return Err(refuse("directory target has an incompatible type"));
                        }
                    }
                    Operation::File(p) => self.file(
                        wanted[p.as_slice()],
                        actual.get(p.as_slice()).copied(),
                        &mut report,
                    )?,
                    Operation::Symlink(p) => {
                        let node = wanted[p.as_slice()];
                        let e = entry(&self.destination, p)?;
                        let detach = match e.metadata()? {
                            Some(m) if !self.safe_aliases(node, &m)? => {
                                !metadata_matches(node, &e)?
                                    || actual
                                        .get(p.as_slice())
                                        .is_some_and(|old| old.hardlink != node.hardlink)
                            }
                            _ => false,
                        };
                        if detach
                            || fs::read_link(e.path())
                                .ok()
                                .as_ref()
                                .map(|p| p.as_os_str().as_bytes())
                                != node.link.as_deref()
                        {
                            remove_leaf(&e)?;
                            std::os::unix::fs::symlink(
                                std::ffi::OsStr::from_bytes(
                                    node.link.as_deref().context("symlink target missing")?,
                                ),
                                e.path(),
                            )?;
                            report.links_created += 1;
                        }
                    }
                    Operation::Link(p) => {
                        let node = wanted[p.as_slice()];
                        let target = entry(&self.destination, p)?;
                        let leader = entry(&self.destination, node.hardlink.as_ref().unwrap())?;
                        let a = leader.metadata()?.context("hardlink leader missing")?;
                        if !target
                            .metadata()?
                            .is_some_and(|b| a.ino() == b.ino() && a.dev() == b.dev())
                        {
                            remove_leaf(&target)?;
                            fs::hard_link(leader.path(), target.path())?;
                            report.links_created += 1;
                        }
                    }
                    Operation::Metadata(p) => metadata(
                        wanted[p.as_slice()],
                        &entry(&self.destination, p)?,
                        &mut report,
                    )?,
                }
                Ok(())
            })()
            .with_context(|| format!("applying synchronization operation {op:?}"))?;
        }
        filesystem::flush(&self.destination)?;
        Ok(report)
    }

    pub fn verify(&self, destination: &Path) -> Result<()> {
        if Inventory::capture(destination, &self.boundaries)?.digest()? != self.desired.digest()? {
            return Err(refuse("the copy does not match the booted root"));
        }
        Ok(())
    }
}

fn remove_leaf(entry: &Entry) -> Result<()> {
    match entry.metadata()? {
        Some(m) if m.is_dir() => Err(refuse("a leaf unexpectedly became a directory")),
        Some(_) => {
            fs::remove_file(entry.path())?;
            Ok(())
        }
        None => Ok(()),
    }
}
