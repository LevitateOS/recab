//! Deterministic inventories of system content, independent of reusable slot
//! letters. Inventories contain metadata and content digests, never file bodies.

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
use crate::state::Slot;

pub(crate) const MARKER: &str = "usr/lib/recab/generation.json";
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

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Generation {
    pub id: String,
    pub inventory_digest: String,
    pub kernel_digest: String,
    pub initrd_digest: Option<String>,
}

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Extent {
    pub(crate) offset: u64,
    pub(crate) length: u64,
}
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
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
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inventory {
    pub(crate) version: u32,
    pub(crate) entries: Vec<Node>,
}

impl Inventory {
    pub fn capture(path: &Path) -> Result<Self> {
        Self::capture_with(path, true)
    }
    pub(crate) fn capture_candidate(path: &Path) -> Result<Self> {
        Self::capture_with(path, false)
    }
    fn capture_with(path: &Path, normalize: bool) -> Result<Self> {
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
            normalize,
        )?;
        Ok(Self {
            version: 1,
            entries,
        })
    }
    /// Observe only explicitly selected paths; never export/copy a system tree
    /// merely to install a small package. Parents are resolved by directory FDs.
    pub(crate) fn capture_paths(path: &Path, paths: &[std::path::PathBuf]) -> Result<Self> {
        let root = Root::open(path)?;
        let device = root.device()?;
        let mut entries = Vec::new();
        let mut links = BTreeMap::<(u64, u64), Vec<u8>>::new();
        let mut paths = paths.to_vec();
        paths.sort();
        paths.dedup();
        for path in paths {
            let metadata = match root.metadata(&path) {
                Err(e)
                    if e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                        matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP))
                    }) =>
                {
                    None
                }
                v => v?,
            };
            let Some(m) = metadata else {
                continue;
            };
            if m.dev() != device {
                return Err(refuse("selected system path crosses a mount"));
            }
            let e = root.entry(&path, false)?;
            let mut n = Node {
                path: path.as_os_str().as_bytes().to_vec(),
                kind: if m.is_dir() {
                    "dir"
                } else if m.is_file() {
                    "file"
                } else if m.file_type().is_symlink() {
                    "link"
                } else {
                    return Err(refuse("unsupported selected system inode"));
                }
                .into(),
                mode: m.mode() & 0o7777,
                uid: m.uid(),
                gid: m.gid(),
                size: if m.is_dir() { 0 } else { m.len() },
                modified: m.mtime(),
                modified_ns: m.mtime_nsec(),
                xattrs: filesystem::xattrs(&e.path())?,
                content: None,
                normalized: None,
                link: None,
                hardlink: None,
                extents: None,
            };
            if m.is_file() {
                let mut file = e.open()?;
                n.content = Some(hash_reader(&mut file)?);
                n.extents = extents(&file, m.len())?;
            }
            if m.file_type().is_symlink() {
                n.link = Some(fs::read_link(e.path())?.as_os_str().as_bytes().to_vec());
            }
            if !m.is_dir() && m.nlink() > 1 {
                if let Some(first) = links.get(&(m.dev(), m.ino())) {
                    n.hardlink = Some(first.clone());
                } else {
                    links.insert((m.dev(), m.ino()), n.path.clone());
                }
            }
            entries.push(n);
        }
        Ok(Self {
            version: 1,
            entries,
        })
    }
    pub fn digest(&self) -> Result<String> {
        let mut canonical = self.clone();
        for node in &mut canonical.entries {
            // Slot materialization rewrites these records; publishing the
            // excluded generation marker changes its parent directory's mtime.
            // Other system modification times remain part of the generation.
            if matches!(
                node.path.as_slice(),
                b"etc/fstab" | b"usr/lib/recab/immutable" | b"usr/lib/recab"
            ) {
                node.modified = 0;
                node.modified_ns = 0;
            }
            node.extents = None;
            if let Some(normalized) = node.normalized.take() {
                node.content = Some(normalized);
            }
            if node.path == b"etc/fstab" || node.path == b"usr/lib/recab/immutable" {
                node.size = 0;
            }
        }
        Ok(digest(&serde_json::to_vec(&canonical)?))
    }
    pub fn save(&self, state_dir: &Path) -> Result<String> {
        let id = self.digest()?;
        let path = state_dir.join("inventories").join(format!("{id}.json"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        target_root::write_durable(&path, &serde_json::to_vec(self)?, Some(0o600))?;
        Ok(id)
    }
}

fn scan(
    root: &Root,
    relative: &Path,
    device: u64,
    links: &mut BTreeMap<(u64, u64), Vec<u8>>,
    entries: &mut Vec<Node>,
    normalize: bool,
) -> Result<()> {
    for name in root.names(relative)? {
        let path = relative.join(&name);
        if path == Path::new(MARKER) || path == Path::new(recinit::installed::BOOT_RECORD) {
            continue;
        }
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
            if normalize && path == Path::new("etc/fstab") {
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
            } else if normalize && path == Path::new("usr/lib/recab/immutable") {
                let text = root
                    .optional_text(&path)?
                    .context("immutable configuration disappeared")?;
                let fields = text.split_whitespace().collect::<Vec<_>>();
                if fields.len() != 2 || !matches!(fields[0], "A" | "B") {
                    return Err(refuse("invalid immutable slot configuration"));
                }
                node.normalized = Some(digest(format!("<slot> {}", fields[1]).as_bytes()));
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
            scan(root, &path, device, links, entries, normalize)?;
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
pub(crate) fn hash_file(path: &Path) -> Result<String> {
    let parent = path.parent().context("boot artifact has no parent")?;
    let entry = Root::open(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })?
    .entry(
        Path::new(path.file_name().context("boot artifact has no name")?),
        false,
    )?;
    hash_reader(&mut entry.open()?)
}
impl Generation {
    pub fn capture(
        root: &Path,
        kernel: &Path,
        initrd: Option<&Path>,
        state: &Path,
    ) -> Result<Self> {
        let inventory_digest = Inventory::capture(root)?.save(state)?;
        let kernel_digest = hash_file(kernel)?;
        let initrd_digest = initrd.map(hash_file).transpose()?;
        let id = digest(&serde_json::to_vec(&(
            &inventory_digest,
            &kernel_digest,
            &initrd_digest,
        ))?);
        Ok(Self {
            id,
            inventory_digest,
            kernel_digest,
            initrd_digest,
        })
    }
    pub fn mark(&self, root: &Path, slot: Slot, partuuid: &str) -> Result<()> {
        let bytes = serde_json::to_vec(
            &serde_json::json!({ "version": 1, "generation": self.id, "slot": slot, "partuuid": partuuid }),
        )?;
        write_record(root, MARKER, &bytes)
    }
    pub(crate) fn mark_boot(&self, root: &Path, slot: Slot, uuid: &str, peer: &str) -> Result<()> {
        self.mark(root, slot, uuid)?;
        write_record(
            root,
            recinit::installed::BOOT_RECORD,
            format!("1 {slot} {uuid} {} {peer}\n", self.id).as_bytes(),
        )
    }
    pub fn verify_root(&self, root: &Path, slot: Slot, partuuid: &str) -> Result<()> {
        let marker = Root::open(root)?
            .optional_text(Path::new(MARKER))?
            .context("the committed generation marker is missing")?;
        let marker: serde_json::Value = serde_json::from_str(&marker)?;
        if marker["version"].as_u64() != Some(1)
            || marker["generation"].as_str() != Some(self.id.as_str())
            || marker["slot"].as_str() != Some(slot.to_string().as_str())
            || marker["partuuid"]
                .as_str()
                .is_none_or(|id| !id.eq_ignore_ascii_case(partuuid))
        {
            return Err(refuse("committed generation marker does not match state"));
        }
        if Inventory::capture(root)?.digest()? != self.inventory_digest {
            return Err(refuse(
                "the active system differs from its committed generation; explicit adoption is required",
            ));
        }
        Ok(())
    }
    pub fn valid(&self) -> bool {
        [&self.id, &self.inventory_digest, &self.kernel_digest]
            .into_iter()
            .chain(self.initrd_digest.iter())
            .all(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            && serde_json::to_vec(&(
                &self.inventory_digest,
                &self.kernel_digest,
                &self.initrd_digest,
            ))
            .is_ok_and(|bytes| digest(&bytes) == self.id)
    }
}

fn write_record(root: &Path, path: &str, bytes: &[u8]) -> Result<()> {
    let opened = Root::open(root)?;
    let entry = opened.entry(Path::new(path), true)?;
    if let Some(meta) = entry.metadata()? {
        if !meta.is_file() {
            return Err(refuse("boot metadata must be a regular file"));
        }
        if meta.nlink() != 1 {
            fs::remove_file(entry.path())?;
        }
    }
    entry.write(bytes, 0o600)
}
