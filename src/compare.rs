//! How alike the two systems are. Reads both and changes neither: the other
//! slot is mounted read-only without replaying its journal, under the run
//! lock, so no other recab command works on it meanwhile. It is what
//! `inventory` reads for one system, taken for both and set side by side.
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::generation::{Inventory, Node};
use crate::slot_mount::SlotMount;
use crate::{OtherSystem, Slot, System};

/// The schema of [`Comparison`].
pub const COMPARISON_SCHEMA_VERSION: u32 = 1;

/// Differences listed one by one; the rest is counted in `more`.
const LISTED: usize = 5000;
/// Folders are reported down to this many names below `/`.
const FOLDER_DEPTH: usize = 3;
/// Each system names its own partition in these; they differ by design.
const OWN: &[&[u8]] = &[b"etc/fstab", b"usr/lib/recab/immutable"];
/// Their modification times follow from the slot's own files being written,
/// as in the generation digest.
const OWN_TIMES: &[&[u8]] = &[b"etc/fstab", b"usr/lib/recab/immutable"];

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ComparedSystem {
    /// Absent when two trees were compared and not this computer's slots.
    pub slot: Option<Slot>,
    pub device: Option<String>,
    /// Files and links, not folders.
    pub files: u64,
    /// Length of every file, each hard-linked file once.
    pub bytes: u64,
    /// What the other system is to the running one; absent for the running one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<OtherSystem>,
}

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Same {
    pub files: u64,
    pub bytes: u64,
}

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FolderSize {
    pub path: String,
    pub running_bytes: u64,
    pub other_bytes: u64,
}

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DifferenceKind {
    /// Only the running system has it. A folder stands for all it holds.
    OnlyRunning,
    /// Only the other system has it. A folder stands for all it holds.
    OnlyOther,
    /// Both have it and what it holds differs.
    Content,
    /// Same content; owner, mode, links, attributes or time differ.
    Metadata,
}

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Difference {
    pub path: String,
    pub change: DifferenceKind,
    pub running_bytes: u64,
    pub other_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Comparison {
    pub schema_version: u32,
    pub running: ComparedSystem,
    pub other: ComparedSystem,
    pub same: Same,
    /// Every folder down to [`FOLDER_DEPTH`] with all it holds, on both sides.
    pub folders: Vec<FolderSize>,
    /// Paths that differ by design and are not counted as differences.
    pub own: Vec<String>,
    pub differences: Vec<Difference>,
    /// Differences left out of the list.
    pub more: u64,
}

fn shown(path: &[u8]) -> String {
    format!("/{}", String::from_utf8_lossy(path))
}

fn parent(path: &[u8]) -> Option<&[u8]> {
    if path.is_empty() {
        return None;
    }
    Some(match path.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => &path[..slash],
        None => &[],
    })
}

fn depth(path: &[u8]) -> usize {
    if path.is_empty() {
        0
    } else {
        1 + path.iter().filter(|byte| **byte == b'/').count()
    }
}

/// The room a node takes by its length; a further hard link takes none.
fn room(node: &Node) -> u64 {
    if node.kind == "file" && node.hardlink.is_none() {
        node.size
    } else {
        0
    }
}

/// Every folder of one system with the room of all it holds.
fn folder_sizes(inventory: &Inventory) -> BTreeMap<&[u8], u64> {
    let mut sizes = BTreeMap::new();
    for node in &inventory.entries {
        if node.kind == "dir" {
            sizes.entry(node.path.as_slice()).or_insert(0);
        }
        let bytes = room(node);
        let mut above = parent(&node.path);
        while let Some(folder) = above {
            *sizes.entry(folder).or_insert(0) += bytes;
            above = parent(folder);
        }
    }
    sizes
}

fn what(kind: &str) -> &'static str {
    match kind {
        "dir" => "a folder",
        "file" => "a file",
        _ => "a link",
    }
}

/// How two nodes of one kind and one content differ otherwise.
fn metadata_detail(running: &Node, other: &Node) -> Option<String> {
    let mut detail = Vec::new();
    if running.mode != other.mode {
        detail.push(format!("mode {:o} and {:o}", running.mode, other.mode));
    }
    if (running.uid, running.gid) != (other.uid, other.gid) {
        detail.push(format!(
            "owner {}:{} and {}:{}",
            running.uid, running.gid, other.uid, other.gid
        ));
    }
    if running.xattrs != other.xattrs {
        detail.push("extended attributes".into());
    }
    if running.hardlink != other.hardlink {
        detail.push("hard links".into());
    }
    // A folder's time follows from what was added to it or taken out of it,
    // and that is listed by itself.
    if (running.modified, running.modified_ns) != (other.modified, other.modified_ns)
        && running.kind != "dir"
        && !OWN_TIMES.contains(&running.path.as_slice())
    {
        detail.push("modification time".into());
    }
    (!detail.is_empty()).then(|| detail.join(", "))
}

/// Set two inventories side by side. `running` and `other` arrive without
/// counts; this fills them.
pub(crate) fn set_beside(
    running_inventory: &Inventory,
    other_inventory: &Inventory,
    mut running: ComparedSystem,
    mut other: ComparedSystem,
) -> Comparison {
    let running_folders = folder_sizes(running_inventory);
    let other_folders = folder_sizes(other_inventory);
    let mut paths: BTreeMap<&[u8], (Option<&Node>, Option<&Node>)> = BTreeMap::new();
    for node in &running_inventory.entries {
        paths.entry(node.path.as_slice()).or_default().0 = Some(node);
        if node.kind != "dir" {
            running.files += 1;
            running.bytes += room(node);
        }
    }
    for node in &other_inventory.entries {
        paths.entry(node.path.as_slice()).or_default().1 = Some(node);
        if node.kind != "dir" {
            other.files += 1;
            other.bytes += room(node);
        }
    }
    // The room a difference is about: a folder that stands for all it holds
    // has the room of all of it; a folder whose metadata differs has none.
    let whole = |node: &Node, folders: &BTreeMap<&[u8], u64>, change: DifferenceKind| {
        if node.kind != "dir" {
            room(node)
        } else if change == DifferenceKind::Metadata {
            0
        } else {
            folders.get(node.path.as_slice()).copied().unwrap_or(0)
        }
    };

    let mut same = Same::default();
    let mut own = Vec::new();
    let mut differences = Vec::new();
    let mut more = 0;
    // Folders that stand for all they hold: what is under them is not listed.
    let mut whole_folders: BTreeSet<&[u8]> = BTreeSet::new();
    // A parent sorts before what it holds, so it is decided first.
    for (path, pair) in &paths {
        if parent(path).is_some_and(|above| whole_folders.contains(above)) {
            if pair.0.is_some_and(|node| node.kind == "dir")
                || pair.1.is_some_and(|node| node.kind == "dir")
            {
                whole_folders.insert(*path);
            }
            continue;
        }
        let difference = match *pair {
            (Some(_), None) => Some((DifferenceKind::OnlyRunning, None)),
            (None, Some(_)) => Some((DifferenceKind::OnlyOther, None)),
            (Some(here), Some(there)) if here.kind != there.kind => Some((
                DifferenceKind::Content,
                Some(format!(
                    "{} in the running system, {} in the other",
                    what(&here.kind),
                    what(&there.kind)
                )),
            )),
            (Some(here), Some(there)) => {
                if OWN.contains(path) && here.normalized.is_some() && here.normalized == there.normalized {
                    own.push(shown(path));
                    None
                } else if here.content != there.content || here.link != there.link {
                    Some((DifferenceKind::Content, None))
                } else {
                    metadata_detail(here, there).map(|detail| (DifferenceKind::Metadata, Some(detail)))
                }
            }
            (None, None) => None,
        };
        let Some((change, detail)) = difference else {
            if let (Some(here), Some(_)) = *pair {
                if here.kind != "dir" {
                    same.files += 1;
                    same.bytes += room(here);
                }
            }
            continue;
        };
        if change != DifferenceKind::Metadata
            && (pair.0.is_some_and(|node| node.kind == "dir") || pair.1.is_some_and(|node| node.kind == "dir"))
            && pair.0.map(|node| node.kind.as_str()) != pair.1.map(|node| node.kind.as_str())
        {
            whole_folders.insert(*path);
        }
        if differences.len() == LISTED {
            more += 1;
            continue;
        }
        differences.push(Difference {
            path: shown(path),
            change,
            running_bytes: pair.0.map_or(0, |node| whole(node, &running_folders, change)),
            other_bytes: pair.1.map_or(0, |node| whole(node, &other_folders, change)),
            detail,
        });
    }

    let mut folders: BTreeMap<&[u8], (u64, u64)> = BTreeMap::new();
    for (path, bytes) in &running_folders {
        if depth(path) <= FOLDER_DEPTH {
            folders.entry(*path).or_default().0 = *bytes;
        }
    }
    for (path, bytes) in &other_folders {
        if depth(path) <= FOLDER_DEPTH {
            folders.entry(*path).or_default().1 = *bytes;
        }
    }
    Comparison {
        schema_version: COMPARISON_SCHEMA_VERSION,
        running,
        other,
        same,
        folders: folders
            .into_iter()
            .map(|(path, (running_bytes, other_bytes))| FolderSize {
                path: shown(path),
                running_bytes,
                other_bytes,
            })
            .collect(),
        own,
        differences,
        more,
    }
}

/// One line of JSON on stderr for a front end that follows along.
fn tell(events: bool, event: serde_json::Value) {
    if events {
        eprintln!("{event}");
    }
}

fn step(events: bool, step: &str, status: &str) {
    tell(events, serde_json::json!({"event": "step", "step": step, "status": status}));
}

/// Read the running system and the other slot and set them side by side.
/// With `events`, each step is reported on stderr as it starts and ends.
pub fn compare(events: bool) -> Result<Comparison> {
    // What the other system is, as `status` says it.
    let role = crate::status::status(None)?.lifecycle.other;
    let sys = System::locked("compare")?;
    crate::filesystem::private_mounts()?;
    let active = sys.boot.selection.active;
    let inactive = active.other();
    let side = |slot: Slot, role: Option<OtherSystem>| -> Result<ComparedSystem> {
        let partuuid = sys.backend.slot_partuuid(&sys.boot, slot)?;
        Ok(ComparedSystem {
            slot: Some(slot),
            device: target_root::device_with_tag("PARTUUID", &partuuid)?
                .map(|device| device.display().to_string()),
            files: 0,
            bytes: 0,
            role,
        })
    };
    let running = side(active, None)?;
    let other = side(inactive, Some(role))?;
    tell(
        events,
        serde_json::json!({"event": "plan", "steps": [
            ["running", "Read the running system"],
            ["other", "Read the other system"],
            ["compare", "Compare them"],
        ]}),
    );

    step(events, "running", "running");
    let running_inventory = Inventory::capture(&sys.target.root)?;
    step(events, "running", "done");

    step(events, "other", "running");
    let mount = SlotMount::open(&sys, inactive, &format!("compare-{}", crate::prepare::new_id()?))?;
    let point = mount.point.clone();
    let read = Inventory::capture(&mount.point);
    let closed = mount.close();
    // The mount point was made for this run alone.
    let _ = std::fs::remove_dir(&point);
    let other_inventory = read?;
    closed?;
    step(events, "other", "done");

    step(events, "compare", "running");
    let comparison = set_beside(&running_inventory, &other_inventory, running, other);
    step(events, "compare", "done");
    tell(events, serde_json::json!({"event": "done"}));
    Ok(comparison)
}

/// The same for two system trees that are mounted already, for an installer
/// or a check: `running` is taken as the running system. Takes no lock and
/// mounts nothing.
pub fn compare_roots(running: &Path, other: &Path) -> Result<Comparison> {
    let side = || ComparedSystem {
        slot: None,
        device: None,
        files: 0,
        bytes: 0,
        role: None,
    };
    Ok(set_beside(
        &Inventory::capture(running)?,
        &Inventory::capture(other)?,
        side(),
        side(),
    ))
}
