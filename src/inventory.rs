//! What a system root holds, file by file: metadata and content digests,
//! never file bodies. `copy` and `compare` read both roots this way.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::error::refuse;
use crate::filesystem::{self, Root};
use crate::immutable::SLOT_FILES;

/// Top-level folders that are not part of a root: shared, runtime or
/// mount points. Neither `copy` nor `compare` goes into them.
pub(crate) const EXCLUDED: &[&[u8]] = &[
    b"boot",
    b"var",
    b"home",
    b"root",
    b"proc",
    b"sys",
    b"dev",
    b"run",
    b"tmp",
    b"lost+found",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Extent {
    pub(crate) offset: u64,
    pub(crate) length: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) path: Vec<u8>,
    pub(crate) kind: String,
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) size: u64,
    pub(crate) modified: i64,
    pub(crate) modified_ns: i64,
    pub(crate) xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) content: Option<String>,
    pub(crate) normalized: Option<String>,
    pub(crate) link: Option<Vec<u8>>,
    pub(crate) hardlink: Option<Vec<u8>>,
    pub(crate) extents: Option<Vec<Extent>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Inventory {
    pub(crate) version: u32,
    pub(crate) entries: Vec<Node>,
}

impl Inventory {
    /// Everything `path` holds, its own files (fstab, slot marker) with
    /// their root-specific parts left out of the comparable digest.
    pub fn capture(path: &Path) -> Result<Self> {
        crate::filesystem::check_system_mounts(path)?;
        let root = Root::open(path)?;
        let entry = root.root_entry()?;
        let metadata = entry.metadata()?.context("system root disappeared")?;
        let mut entries = vec![Node {
            path: Vec::new(),
            kind: "dir".into(),
            mode: metadata.mode() & 0o7777,
            uid: metadata.uid(),
            gid: metadata.gid(),
            size: 0,
            modified: metadata.mtime(),
            modified_ns: metadata.mtime_nsec(),
            xattrs: filesystem::xattrs(&entry.path())?,
            content: None,
            normalized: None,
            link: None,
            hardlink: None,
            extents: None,
        }];
        let mut links = BTreeMap::new();
        scan(
            &root,
            Path::new(""),
            root.device()?,
            &mut links,
            &mut entries,
        )?;
        Ok(Self {
            version: 1,
            entries,
        })
    }
    pub fn digest(&self) -> Result<String> {
        let mut canonical = self.clone();
        for node in &mut canonical.entries {
            // Each root writes its own fstab and slot marker.
            if SLOT_FILES.contains(&node.path.as_slice()) {
                node.modified = 0;
                node.modified_ns = 0;
            }
            node.extents = None;
            if let Some(normalized) = node.normalized.take() {
                node.content = Some(normalized);
            }
            if SLOT_FILES.contains(&node.path.as_slice()) {
                node.size = 0;
            }
        }
        Ok(digest(&serde_json::to_vec(&canonical)?))
    }
}

fn scan(
    root: &Root,
    relative: &Path,
    device: u64,
    links: &mut BTreeMap<(u64, u64), Vec<u8>>,
    entries: &mut Vec<Node>,
) -> Result<()> {
    for name in root.names(relative)? {
        let path = relative.join(&name);
        let metadata = root
            .metadata(&path)?
            .ok_or_else(|| refuse("system changed during inventory"))?;
        if relative.as_os_str().is_empty()
            && metadata.is_dir()
            && EXCLUDED.contains(&name.as_bytes())
        {
            continue;
        }
        if metadata.dev() != device {
            return Err(refuse(format!(
                "unexpected mounted filesystem under '{}'",
                path.display()
            )));
        }
        let entry = root.entry(&path, false)?;
        let directory = metadata.is_dir();
        let kind = if directory {
            "dir"
        } else if metadata.is_file() {
            "file"
        } else if metadata.file_type().is_symlink() {
            "link"
        } else {
            return Err(refuse(format!(
                "unsupported special system file '{}'",
                path.display()
            )));
        };
        let mut node = Node {
            path: path.as_os_str().as_bytes().to_vec(),
            kind: kind.into(),
            mode: metadata.mode() & 0o7777,
            uid: metadata.uid(),
            gid: metadata.gid(),
            size: if directory { 0 } else { metadata.len() },
            modified: metadata.mtime(),
            modified_ns: metadata.mtime_nsec(),
            xattrs: filesystem::xattrs(&entry.path())?,
            content: None,
            normalized: None,
            link: None,
            hardlink: None,
            extents: None,
        };
        if metadata.is_file() {
            let mut input = entry.open()?;
            node.content = Some(hash_reader(&mut input)?);
            node.extents = extents(&input, metadata.len())?;
            if path == Path::new("etc/fstab") {
                let text = root
                    .optional_text(&path)?
                    .context("fstab disappeared during inventory")?;
                let normalized = text
                    .lines()
                    .map(|line| {
                        let mut fields = line.split_whitespace().collect::<Vec<_>>();
                        if fields.get(1) == Some(&"/") && !line.trim_start().starts_with('#') {
                            fields[0] = "PARTUUID=<slot>";
                            fields.join("\t")
                        } else {
                            line.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                node.normalized = Some(digest(normalized.as_bytes()));
            } else if path == Path::new(recinit::installed::IMMUTABLE_CONFIG) {
                let text = root
                    .optional_text(&path)?
                    .context("the slot marker disappeared")?;
                let fields = text.split_whitespace().collect::<Vec<_>>();
                node.normalized = fields
                    .get(1)
                    .map(|state| digest(format!("<slot> {state} <other>").as_bytes()));
            }
        } else if kind == "link" {
            node.link = Some(fs::read_link(entry.path())?.as_os_str().as_bytes().to_vec());
        }
        if !directory && metadata.nlink() > 1 {
            if let Some(first) = links.get(&(metadata.dev(), metadata.ino())) {
                node.hardlink = Some(first.clone());
            } else {
                links.insert((metadata.dev(), metadata.ino()), node.path.clone());
            }
        }
        entries.push(node);
        if directory {
            scan(root, &path, device, links, entries)?;
        }
        let after = root
            .metadata(&path)?
            .ok_or_else(|| refuse("system changed during inventory"))?;
        if metadata.ino() != after.ino()
            || metadata.ctime() != after.ctime()
            || metadata.ctime_nsec() != after.ctime_nsec()
        {
            return Err(refuse(format!(
                "system changed during inventory at '{}'",
                path.display()
            )));
        }
    }
    Ok(())
}
pub(crate) fn extents(file: &fs::File, size: u64) -> Result<Option<Vec<Extent>>> {
    let mut ranges = Vec::new();
    let mut offset = 0u64;
    while offset < size {
        let start = unsafe { libc::lseek(file.as_raw_fd(), offset as i64, libc::SEEK_DATA) };
        if start < 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ENXIO) => break,
                Some(libc::EINVAL | libc::ENOTSUP) => return Ok(None),
                _ => return Err(std::io::Error::last_os_error().into()),
            }
        }
        let end = unsafe { libc::lseek(file.as_raw_fd(), start, libc::SEEK_HOLE) };
        if end <= start {
            return Err(refuse("invalid sparse extent from system filesystem"));
        }
        offset = (end as u64).min(size);
        ranges.push(Extent {
            offset: start as u64,
            length: offset - start as u64,
        });
    }
    Ok(Some(ranges))
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn hash_reader(input: &mut impl Read) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
