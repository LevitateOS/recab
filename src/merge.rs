//! Persisted O/C/N merge. Plans contain fingerprints, never account bodies.
//! C is captured once; recovery reuses that plan instead of diffing a mixed root.
use crate::{
    PreparationPhase, Readiness, SyncReport,
    filesystem::Root,
    generation::{Inventory, Node},
    slot_mount::SlotMount,
    system::System,
};
use anyhow::{Context, Result, ensure};
use distro_contract::release::{self as format, Identity};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};
pub(crate) const BASELINE: &str = "usr/lib/recab/release-baseline.json";
const WORK: &str = "usr/lib/recab/update-work";
const BOUND: u64 = 256 * 1024 * 1024;
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conflict {
    pub id: String,
    pub path: String,
    pub path_hex: String,
    pub kind: String,
    pub resolution: Option<String>,
}
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeStatus {
    pub preparation: String,
    pub revision: u64,
    pub release: String,
    pub phase: String,
    pub conflicts: Vec<Conflict>,
    pub unresolved: u64,
    pub report: Option<SyncReport>,
    pub failure: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Baseline {
    version: u32,
    identity: Identity,
    inventory: Inventory,
    accounts: crate::merge_accounts::Base,
    #[serde(default)]
    release_id: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Decision {
    choice: String,
    edited: Option<Node>,
    work: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    preparation: String,
    source_generation: String,
    revision: u64,
    release: String,
    trusted_local: bool,
    allow_flavour: bool,
    phase: String,
    current: Inventory,
    current_accounts: crate::merge_accounts::Base,
    baseline: Baseline,
    next: Baseline,
    conflicts: Vec<Conflict>,
    choices: BTreeMap<String, Decision>,
    desired: Option<Inventory>,
    sources: BTreeMap<String, String>,
    removals: Vec<Vec<u8>>,
    report: Option<SyncReport>,
    failure: Option<String>,
    #[serde(default)]
    reproduced: BTreeSet<String>,
    #[serde(default)]
    overrides: Vec<Node>,
    #[serde(default)]
    override_sources: BTreeMap<String, String>,
    #[serde(default)]
    package_removals: BTreeSet<String>,
    #[serde(default)]
    receipt: Option<levitate_recipe::system::Receipt>,
    #[serde(default)]
    previous: Option<String>,
    #[serde(default)]
    completed_digest: Option<String>,
}
fn path(bytes: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(bytes))
}
fn skip(bytes: &[u8]) -> bool {
    bytes == BASELINE.as_bytes()
        || bytes == WORK.as_bytes()
        || bytes.starts_with(format!("{WORK}/").as_bytes())
        || bytes == levitate_recipe::target::MANIFEST.as_bytes()
}
fn inventory(root: &Path) -> Result<Inventory> {
    let mut inv = Inventory::capture(root)?;
    inv.entries.retain(|n| !skip(&n.path));
    Ok(inv)
}
fn completed_inventory(root: &Path) -> Result<String> {
    let mut inventory = Inventory::capture(root)?;
    inventory.entries.retain(|n| {
        n.path != WORK.as_bytes() && !n.path.starts_with(format!("{WORK}/").as_bytes())
    });
    inventory.digest()
}
fn signature(n: &Node) -> String {
    let mut n = n.clone();
    n.modified = 0;
    n.modified_ns = 0;
    n.extents = None;
    if let Some(value) = n.normalized.take() {
        n.content = Some(value);
        n.size = 0;
    }
    crate::generation::digest(&serde_json::to_vec(&n).expect("node serialization"))
}
fn metadata_equal(a: Option<&Node>, b: Option<&Node>) -> bool {
    fn sig(n: &Node) -> String {
        crate::generation::digest(
            &serde_json::to_vec(&(
                &n.kind,
                n.mode,
                n.uid,
                n.gid,
                &n.xattrs,
                &n.link,
                &n.hardlink,
            ))
            .expect("metadata serialization"),
        )
    }
    a.map(sig) == b.map(sig)
}
fn account_metadata(j: &mut Journal) {
    let o = map(&j.baseline.inventory);
    let c = map(&j.current);
    let n = map(&j.next.inventory);
    for file in crate::merge_accounts::FILES {
        let key = file.as_bytes();
        if !metadata_equal(c.get(key), o.get(key))
            && !metadata_equal(n.get(key), o.get(key))
            && !metadata_equal(c.get(key), n.get(key))
        {
            add_conflict(j, conflict(key, "account_metadata"));
        }
    }
}
fn equal(a: Option<&Node>, b: Option<&Node>) -> bool {
    a.map(signature) == b.map(signature)
}
fn assigned_labels(next: &mut Inventory, old: &Inventory) {
    let old = map(old);
    for node in &mut next.entries {
        if !node
            .xattrs
            .iter()
            .any(|(name, _)| name == b"security.selinux")
        {
            if let Some(label) = old.get(&node.path).and_then(|n| {
                n.xattrs
                    .iter()
                    .find(|(name, _)| name == b"security.selinux")
            }) {
                node.xattrs.push(label.clone());
                node.xattrs.sort();
            }
        }
    }
}
fn map(inv: &Inventory) -> BTreeMap<Vec<u8>, Node> {
    inv.entries
        .iter()
        .map(|n| (n.path.clone(), n.clone()))
        .collect()
}
fn read<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T> {
    let file = levitate_recipe::fsx::open_regular(p)?;
    ensure!(
        file.metadata()?.len() <= BOUND,
        "merge record exceeds bound"
    );
    Ok(serde_json::from_reader(file.take(BOUND + 1))?)
}
fn save<T: Serialize>(p: &Path, value: &T) -> Result<()> {
    Ok(target_root::write_durable(
        p,
        &serde_json::to_vec(value)?,
        Some(0o600),
    )?)
}
fn journal_path(sys: &System) -> Result<PathBuf> {
    let p = sys
        .state
        .preparation
        .as_ref()
        .context("prepare the inactive system first")?;
    Ok(sys.target.state_dir.join(format!("merge-{}.json", p.id)))
}
fn baseline(root: &Path, identity: Identity) -> Result<Baseline> {
    Ok(Baseline {
        version: 1,
        identity,
        inventory: inventory(root)?,
        accounts: crate::merge_accounts::baseline(&crate::merge_accounts::read(root)?),
        release_id: None,
    })
}
pub(crate) fn capture_baseline(root: &Path) -> Result<()> {
    let opened = Root::open(root)?;
    if opened.metadata(Path::new(BASELINE))?.is_some() {
        return Ok(());
    }
    if opened.metadata(Path::new(format::SOURCE_CONFIG))?.is_none() {
        return Ok(());
    }
    ensure!(
        opened
            .metadata(Path::new(crate::generation::MARKER))?
            .is_none(),
        "cannot invent a release baseline from a personalized generation; adopt a verified original release"
    );
    let (identity, _) = crate::release::installed(&crate::target::Target::installed(root)?)?;
    let value = baseline(root, identity)?;
    write_baseline(root, &value)
}
fn write_baseline(root: &Path, value: &Baseline) -> Result<()> {
    Root::open(root)?
        .entry(Path::new(BASELINE), true)?
        .write(&serde_json::to_vec(value)?, 0o600)?;
    crate::filesystem::flush(&Root::open(root)?)
}
fn load_baseline(root: &Path) -> Result<Baseline> {
    let opened = Root::open(root)?;
    let file = opened.entry(Path::new(BASELINE), false)?.open().context(
        "release baseline missing; use update adopt-baseline with the original verified release",
    )?;
    let m = file.metadata()?;
    ensure!(
        m.uid() == 0 && m.mode() & 0o077 == 0 && m.len() <= BOUND,
        "unsafe release baseline"
    );
    let b: Baseline = serde_json::from_reader(file.take(BOUND + 1))?;
    ensure!(b.version == 1, "unsupported release baseline");
    Ok(b)
}
fn mounted(
    sys: &System,
    input: &crate::release::VerifiedRelease,
) -> Result<(tempfile::TempDir, crate::stage::Mounted)> {
    let dir = tempfile::Builder::new()
        .prefix("merge-release-")
        .tempdir_in(&sys.target.stage_dir)?;
    let mount = crate::stage::Mounted::new(
        &sys.target,
        &["-t", "erofs", "-o", "ro,loop,nosuid,nodev,noexec"],
        &input.rootfs(),
        dir.path(),
    )?;
    Ok((dir, mount))
}
fn conflict(bytes: &[u8], kind: &str) -> Conflict {
    Conflict {
        id: crate::generation::digest(&[bytes, kind.as_bytes()].concat()),
        path: path(bytes).to_string_lossy().into_owned(),
        path_hex: hex(bytes),
        kind: kind.into(),
        resolution: None,
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(s: &str) -> Result<Vec<u8>> {
    ensure!(
        s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid merge path bytes"
    );
    (0..s.len())
        .step_by(2)
        .map(|i| Ok(u8::from_str_radix(&s[i..i + 2], 16)?))
        .collect()
}
fn covered(j: &Journal, c: &Conflict) -> bool {
    let Ok(p) = unhex(&c.path_hex) else {
        return false;
    };
    j.conflicts.iter().any(|a| {
        a.kind == "directory_tree"
            && a.id != c.id
            && j.choices.contains_key(&a.id)
            && unhex(&a.path_hex).is_ok_and(|parent| subtree(&p, &parent))
    })
}
fn unresolved(j: &Journal) -> usize {
    j.conflicts
        .iter()
        .filter(|c| !j.choices.contains_key(&c.id) && !covered(j, c))
        .count()
}
fn public(j: &Journal) -> MergeStatus {
    MergeStatus {
        preparation: j.preparation.clone(),
        revision: j.revision,
        release: j.release.clone(),
        phase: j.phase.clone(),
        conflicts: j
            .conflicts
            .iter()
            .cloned()
            .map(|mut c| {
                c.resolution = j
                    .choices
                    .get(&c.id)
                    .map(|d| d.choice.clone())
                    .or_else(|| covered(j, &c).then(|| "subtree".into()));
                c
            })
            .collect(),
        unresolved: unresolved(j) as u64,
        report: j.report.clone(),
        failure: j.failure.clone(),
    }
}
fn checked(sys: &System) -> Result<Journal> {
    let j: Journal = read(&journal_path(sys)?)?;
    let p = sys.state.preparation.as_ref().unwrap();
    ensure!(
        j.version == 1
            && j.preparation == p.id
            && j.source_generation == p.source_generation
            && (j.revision == p.revision
                || j.revision == p.revision + 1
                || (j.phase == "complete" && j.revision <= p.revision)),
        "merge preparation/revision mismatch; explicit recovery required"
    );
    format::hash(&j.release)?;
    Ok(j)
}
pub(crate) fn status(sys: &System) -> Result<Option<MergeStatus>> {
    let Some(p) = &sys.state.preparation else {
        return Ok(None);
    };
    let blocked = sys
        .target
        .state_dir
        .join(format!("merge-blocked-{}.json", p.id));
    if blocked.exists() {
        let value: MergeStatus = read(&blocked)?;
        ensure!(value.preparation == p.id, "blocked merge identity mismatch");
        return Ok(Some(value));
    }
    if !journal_path(sys)?.exists() {
        return Ok(None);
    };
    Ok(Some(public(&checked(sys)?)))
}
fn persist(sys: &System, j: &mut Journal) -> Result<()> {
    j.revision += 1;
    save(&journal_path(sys)?, j)?;
    publish_state(sys, j, false)
}
fn publish_state(sys: &System, j: &Journal, complete: bool) -> Result<()> {
    let mut next = sys.state.clone();
    let p = next.preparation.as_mut().unwrap();
    p.revision = j.revision;
    p.operation = if complete { "prepare" } else { "update" }.into();
    p.phase = if complete {
        PreparationPhase::Prepared
    } else {
        PreparationPhase::Interrupted
    };
    p.failure = if complete {
        None
    } else {
        Some(
            if j.failure.is_some() {
                "release merge needs recovery"
            } else if unresolved(j) > 0 {
                "release merge has unresolved conflicts"
            } else {
                "release merge needs recovery"
            }
            .into(),
        )
    };
    p.finalization = None;
    let slot = p.slot;
    next.slot_mut(slot)?.readiness = Readiness::Incomplete;
    sys.store.save(&next)?;
    sys.store.audit(
        crate::state::Operation::Update(slot),
        sys.boot.selection.active,
        &sys.state,
        &next,
    );
    Ok(())
}
fn validate_request(sys: &System, id: Option<&str>, revision: Option<u64>) -> Result<()> {
    let p = sys
        .state
        .preparation
        .as_ref()
        .context("preparation missing")?;
    ensure!(
        id.is_none_or(|v| v == p.id) && revision.is_none_or(|v| v == p.revision),
        "stale preparation id/revision"
    );
    ensure!(
        p.source_slot == sys.boot.selection.active
            && p.source_generation == sys.state.require_ready(p.source_slot)?.id
            && p.synchronized,
        "merge preparation belongs to another source generation"
    );
    Ok(())
}
fn input(sys: &System, j: &Journal) -> Result<crate::release::VerifiedRelease> {
    let v = crate::release::for_target(
        &sys.target,
        &sys.target
            .state_dir
            .join("releases")
            .join(&j.release)
            .join(format::MANIFEST),
        j.trusted_local,
        j.allow_flavour,
    )?;
    ensure!(v.id() == j.release, "pinned merge release changed");
    Ok(v)
}
fn subtree(p: &[u8], parent: &[u8]) -> bool {
    p == parent || (p.starts_with(parent) && p.get(parent.len()) == Some(&b'/'))
}
fn add_conflict(j: &mut Journal, c: Conflict) {
    if !j.conflicts.iter().any(|v| v.id == c.id) {
        j.conflicts.push(c)
    }
}
fn changes(j: &mut Journal) -> Result<(Inventory, BTreeMap<String, String>)> {
    let o = map(&j.baseline.inventory);
    let c = map(&j.current);
    let n = map(&j.next.inventory);
    let keys = o
        .keys()
        .chain(c.keys())
        .chain(n.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut chosen = BTreeMap::new();
    let mut sources = BTreeMap::new();
    let accounts = crate::merge_accounts::FILES;
    for p in keys {
        if p.is_empty() || p == b"usr/lib/recab/immutable" {
            if let Some(v) = c.get(&p) {
                chosen.insert(p.clone(), v.clone());
                sources.insert(hex(&p), "current".into());
            }
            continue;
        }
        if accounts.iter().any(|f| f.as_bytes() == p) {
            continue;
        }
        let a = o.get(&p);
        let b = c.get(&p);
        let d = n.get(&p);
        let kind = if b.zip(d).is_some_and(|(b, d)| b.kind != d.kind) {
            "type"
        } else if b.is_none() || d.is_none() {
            "deletion"
        } else {
            "content_or_metadata"
        };
        let mut cf = conflict(&p, kind);
        let side = if equal(b, a) {
            "release"
        } else if equal(d, a) || equal(b, d) {
            "local"
        } else {
            add_conflict(j, cf.clone());
            "conflict"
        };
        let decision = j.choices.get(&cf.id);
        let side = decision.map(|d| d.choice.as_str()).unwrap_or(side);
        let value = if side == "edited" {
            decision.and_then(|d| d.edited.as_ref())
        } else if side == "release" {
            d
        } else {
            b
        };
        if let Some(node) = value {
            chosen.insert(p.clone(), node.clone());
            sources.insert(
                hex(&p),
                if side == "edited" {
                    decision.unwrap().work.clone().unwrap()
                } else if side == "release" {
                    "release".into()
                } else {
                    "current".into()
                },
            );
        }
        cf.resolution = None;
    }
    // A parent replacement/removal and a retained child are one subtree
    // conflict. Choosing a side for the parent chooses its complete subtree.
    let paths = o
        .keys()
        .chain(c.keys())
        .chain(n.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for parent in paths.iter().filter(|p| !p.is_empty()) {
        let blocker = chosen.get(parent).is_none_or(|v| v.kind != "dir");
        let has_children = chosen.keys().any(|p| p != parent && subtree(p, parent));
        let changed_type = o.get(parent).is_some_and(|v| v.kind == "dir")
            && c.get(parent).is_some_and(|v| v.kind == "dir")
            && n.get(parent).is_none_or(|v| v.kind != "dir");
        let local_child = changed_type
            && paths
                .iter()
                .any(|p| p != parent && subtree(p, parent) && !equal(c.get(p), o.get(p)));
        if (blocker && has_children) || local_child {
            let cf = conflict(parent, "directory_tree");
            add_conflict(j, cf.clone());
            if let Some(decision) = j.choices.get(&cf.id) {
                ensure!(
                    decision.choice != "edited",
                    "directory tree resolution selects local or release"
                );
                chosen.retain(|p, _| !subtree(p, parent));
                sources.retain(|p, _| unhex(p).is_ok_and(|p| !subtree(&p, parent)));
                let (tree, side) = if decision.choice == "release" {
                    (&n, "release")
                } else {
                    (&c, "current")
                };
                for (p, v) in tree.iter().filter(|(p, _)| subtree(p, parent)) {
                    chosen.insert(p.clone(), v.clone());
                    sources.insert(hex(p), side.into());
                }
            }
        }
    }
    for cf in j
        .conflicts
        .iter()
        .filter(|c| c.kind == "ownership_identity" || c.kind == "release_identity")
    {
        if let Some(d) = j.choices.get(&cf.id) {
            let p = unhex(&cf.path_hex)?;
            let node = if d.choice == "edited" {
                d.edited.as_ref()
            } else if d.choice == "release" {
                n.get(&p)
            } else {
                c.get(&p)
            };
            if let Some(node) = node {
                chosen.insert(p.clone(), node.clone());
                sources.insert(
                    hex(&p),
                    if d.choice == "edited" {
                        d.work.clone().unwrap()
                    } else if d.choice == "release" {
                        "release".into()
                    } else {
                        "current".into()
                    },
                );
            } else {
                chosen.remove(&p);
                sources.remove(&hex(&p));
            }
        }
    }
    let anchors = c
        .values()
        .chain(n.values())
        .filter_map(|v| v.hardlink.clone())
        .collect::<BTreeSet<_>>();
    for anchor in anchors {
        let members = c
            .values()
            .chain(n.values())
            .filter(|v| v.path == anchor || v.hardlink.as_ref() == Some(&anchor))
            .map(|v| v.path.clone())
            .collect::<BTreeSet<_>>();
        let invalid = members.iter().any(|p| {
            chosen.get(p).is_some_and(|node| {
                node.hardlink.as_ref().is_some_and(|a| {
                    chosen.get(a).is_none_or(|first| {
                        first.kind != node.kind
                            || first.content != node.content
                            || first.uid != node.uid
                            || first.gid != node.gid
                            || first.mode != node.mode
                            || first.xattrs != node.xattrs
                    })
                })
            })
        });
        if invalid {
            let cf = conflict(&anchor, "hardlink_group");
            add_conflict(j, cf.clone());
            if let Some(d) = j.choices.get(&cf.id) {
                ensure!(
                    d.choice != "edited",
                    "hardlink groups select a complete local or release side"
                );
                let (tree, side) = if d.choice == "release" {
                    (&n, "release")
                } else {
                    (&c, "current")
                };
                for p in members {
                    if let Some(node) = tree.get(&p) {
                        chosen.insert(p.clone(), node.clone());
                        sources.insert(hex(&p), side.into());
                    } else {
                        chosen.remove(&p);
                        sources.remove(&hex(&p));
                    }
                }
            }
        }
    }
    for removed in &j.package_removals {
        if let Some(n) = n.get(removed.as_bytes()) {
            chosen.insert(removed.as_bytes().to_vec(), n.clone());
            sources.insert(hex(removed.as_bytes()), "release".into());
        } else {
            chosen.remove(removed.as_bytes());
            sources.remove(&hex(removed.as_bytes()));
        }
    }
    for node in &j.overrides {
        if node.kind == "dir" && chosen.get(&node.path).is_some_and(|n| n.kind == "dir") {
            continue;
        }
        chosen.insert(node.path.clone(), node.clone());
        sources.insert(
            hex(&node.path),
            j.override_sources[&hex(&node.path)].clone(),
        );
    }
    Ok((
        Inventory {
            version: 1,
            entries: chosen.into_values().collect(),
        },
        sources,
    ))
}
fn work_root(candidate: &Path) -> Result<PathBuf> {
    let root = Root::open(candidate)?;
    let e = root.entry(Path::new(WORK), true)?;
    if e.metadata()?.is_none() {
        fs::create_dir(e.path())?;
    }
    ensure!(
        e.metadata()?.is_some_and(|m| m.is_dir() && m.uid() == 0),
        "unsafe merge work directory"
    );
    fs::set_permissions(e.path(), fs::Permissions::from_mode(0o700))?;
    Ok(candidate.join(WORK))
}
fn write_work(candidate: &Path, name: &str, bytes: &[u8], mode: u32) -> Result<PathBuf> {
    format::relative(name)?;
    let work = work_root(candidate)?;
    let root = Root::open(&work)?;
    root.entry(Path::new(name), true)?.write(bytes, mode)?;
    Ok(work.join(name))
}
fn edited_node(candidate: &Path, decision: &Decision) -> Result<Node> {
    let expected = decision
        .edited
        .as_ref()
        .context("edited fingerprint missing")?;
    let name = decision.work.as_ref().context("edited input missing")?;
    format::relative(name)?;
    let mut n = Inventory::capture_paths(&candidate.join(WORK), &[PathBuf::from(name)])?
        .entries
        .pop()
        .context("edited merge input disappeared")?;
    n.path = expected.path.clone();
    ensure!(
        n.kind == "file" && n.content == expected.content && n.size == expected.size,
        "edited merge input changed"
    );
    Ok(expected.clone())
}
fn materialize_accounts(
    j: &mut Journal,
    candidate: &Path,
    new_root: &Path,
    desired: &mut Inventory,
    sources: &mut BTreeMap<String, String>,
) -> Result<()> {
    let current = crate::merge_accounts::read(candidate)?;
    ensure!(
        crate::merge_accounts::baseline(&current) == j.current_accounts,
        "captured account inputs changed before merge"
    );
    let next = crate::merge_accounts::read(new_root)?;
    let mut choices = BTreeMap::new();
    for file in crate::merge_accounts::FILES {
        let id = conflict(file.as_bytes(), "account_fields").id;
        if let Some(d) = j.choices.get(&id) {
            choices.insert(file.into(), d.choice.clone());
        }
    }
    let (merged, conflicts) =
        crate::merge_accounts::merge(&j.baseline.accounts, &current, &next, &choices)?;
    for file in conflicts {
        add_conflict(j, conflict(file.as_bytes(), "account_fields"));
    }
    if unresolved(j) > 0 {
        return Ok(());
    }
    let old = map(&j.baseline.inventory);
    let local = map(&j.current);
    let release = map(&j.next.inventory);
    for (file, text) in merged {
        let cf = conflict(file.as_bytes(), "account_fields");
        let mc = conflict(file.as_bytes(), "account_metadata");
        let decision = j
            .choices
            .get(&cf.id)
            .filter(|d| d.choice == "edited")
            .or_else(|| j.choices.get(&mc.id).filter(|d| d.choice == "edited"))
            .or_else(|| j.choices.get(&cf.id));
        let (bytes, mut node) = if let Some(d) = decision.filter(|d| d.choice == "edited") {
            let n = edited_node(candidate, d)?;
            let name = d.work.as_ref().unwrap();
            let bytes = fs::read(candidate.join(WORK).join(name))?;
            (bytes, n)
        } else {
            let l = local.get(file.as_bytes());
            let n = release.get(file.as_bytes());
            let o = old.get(file.as_bytes());
            let prefer_release = j
                .choices
                .get(&mc.id)
                .map(|d| d.choice == "release")
                .unwrap_or_else(|| metadata_equal(l, o));
            let node = if prefer_release { n.or(l) } else { l.or(n) }
                .context("merged account metadata missing")?
                .clone();
            (text.into_bytes(), node)
        };
        node.kind = "file".into();
        node.hardlink = None;
        node.link = None;
        node.path = file.as_bytes().to_vec();
        node.content = Some(crate::generation::digest(&bytes));
        node.normalized = None;
        node.size = bytes.len() as u64;
        node.modified = 0;
        node.modified_ns = 0;
        node.extents = None;
        ensure!(
            node.uid == 0
                && node.mode & 0o022 == 0
                && (!file.ends_with("shadow") || node.mode & 0o007 == 0),
            "unsafe resolved account metadata"
        );
        let name = format!("accounts/{file}");
        let output = write_work(candidate, &name, &bytes, node.mode)?;
        sources.insert(hex(file.as_bytes()), name);
        desired.entries.push(node);
        ensure!(output.is_file(), "account output was not saved");
    }
    desired.entries.sort_by(|a, b| a.path.cmp(&b.path));
    let account_root = candidate.join(WORK).join("accounts");
    if let Err(_error) = crate::candidate::accounts(&Root::open(&account_root)?) {
        for file in crate::merge_accounts::FILES {
            add_conflict(j, conflict(file.as_bytes(), "account_fields"));
        }
        j.failure=Some("account identity validation failed; resolve the account tables explicitly (duplicate IDs, primary groups or membership)".into());
        // A previous local/release choice is not proof of a valid identity set.
        for file in crate::merge_accounts::FILES {
            j.choices
                .remove(&conflict(file.as_bytes(), "account_fields").id);
        }
        return Ok(());
    }
    let final_tables = crate::merge_accounts::read(&account_root)?;
    let users = final_tables
        .get("etc/passwd")
        .context("passwd missing")?
        .values()
        .map(|r| Ok((r[2].parse::<u32>()?, r[0].clone())))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let groups = final_tables
        .get("etc/group")
        .context("group missing")?
        .values()
        .map(|r| Ok((r[2].parse::<u32>()?, r[0].clone())))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let ids =
        |tables: &crate::merge_accounts::Tables, file: &str| -> Result<BTreeMap<u32, String>> {
            tables
                .get(file)
                .map(|t| {
                    t.values()
                        .map(|r| Ok((r[2].parse::<u32>()?, r[0].clone())))
                        .collect()
                })
                .unwrap_or_else(|| Ok(BTreeMap::new()))
        };
    let cu = ids(&current, "etc/passwd")?;
    let cg = ids(&current, "etc/group")?;
    let nu = ids(&next, "etc/passwd")?;
    let ng = ids(&next, "etc/group")?;
    for node in &desired.entries {
        let key = hex(&node.path);
        let release = sources
            .get(&key)
            .is_some_and(|s| s == "release" || s.starts_with("packages/"));
        let (expected_users, expected_groups) = if release { (&nu, &ng) } else { (&cu, &cg) };
        if !users.contains_key(&node.uid)
            || !groups.contains_key(&node.gid)
            || expected_users
                .get(&node.uid)
                .is_some_and(|name| users.get(&node.uid) != Some(name))
            || expected_groups
                .get(&node.gid)
                .is_some_and(|name| groups.get(&node.gid) != Some(name))
        {
            let cf = conflict(&node.path, "ownership_identity");
            j.choices.remove(&cf.id);
            add_conflict(j, cf);
        }
    }
    Ok(())
}
fn controls(
    j: &mut Journal,
    candidate: &Path,
    new_root: &Path,
    desired: &mut Inventory,
    sources: &mut BTreeMap<String, String>,
    sys: &System,
) -> Result<()> {
    let nodes = map(desired);
    let normalize = |p: PathBuf| -> Result<PathBuf> {
        let mut out = PathBuf::new();
        for part in p.components() {
            match part {
                std::path::Component::Normal(n) => out.push(n),
                std::path::Component::ParentDir => {
                    ensure!(out.pop(), "control symlink escapes system");
                }
                std::path::Component::RootDir | std::path::Component::CurDir => {}
                _ => anyhow::bail!("invalid control path"),
            }
        }
        Ok(out)
    };
    let get = |name: &str| -> Result<Vec<u8>> {
        let mut relative = PathBuf::from(name);
        for _ in 0..40 {
            let mut prefix = PathBuf::new();
            let parts = relative
                .components()
                .map(|c| c.as_os_str().to_owned())
                .collect::<Vec<_>>();
            let mut redirected = false;
            for (i, part) in parts.iter().enumerate() {
                prefix.push(part);
                if let Some(node) = nodes.get(prefix.as_os_str().as_bytes()) {
                    if let Some(link) = node.link.as_ref().filter(|_| node.kind == "link") {
                        let target = path(link);
                        let mut value = if target.is_absolute() {
                            target.to_path_buf()
                        } else {
                            prefix.parent().unwrap_or(Path::new("")).join(target)
                        };
                        for tail in &parts[i + 1..] {
                            value.push(tail)
                        }
                        relative = normalize(value)?;
                        redirected = true;
                        break;
                    }
                }
            }
            if redirected {
                continue;
            }
            let key = relative.as_os_str().as_bytes();
            let node = nodes
                .get(key)
                .context("required release control file is absent")?;
            if let Some(anchor) = &node.hardlink {
                relative = path(anchor).to_path_buf();
                continue;
            }
            ensure!(
                node.kind == "file" && node.size <= 1024 * 1024,
                "unsafe release control file"
            );
            let source = sources
                .get(&hex(key))
                .context("release control source missing")?;
            let (root, rel) = if source == "current" {
                (Root::open(candidate)?, relative.clone())
            } else if source == "release" {
                (Root::open(new_root)?, relative.clone())
            } else {
                format::relative(source)?;
                (Root::open(&candidate.join(WORK))?, PathBuf::from(source))
            };
            let mut file = root.entry(&rel, false)?.open()?;
            let mut bytes = Vec::new();
            file.by_ref()
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 == node.size
                    && node.content.as_ref() == Some(&crate::generation::digest(&bytes)),
                "control input changed after capture"
            );
            return Ok(bytes);
        }
        anyhow::bail!("control symlink loop")
    };
    let rendered = (|| -> Result<String> {
        let raw = get("etc/fstab")?;
        let uuid = sys
            .backend
            .slot_partuuid(&sys.boot, sys.state.preparation.as_ref().unwrap().slot)?;
        let text = crate::fstab::for_slot(std::str::from_utf8(&raw)?, &uuid, "ext4")?;
        crate::candidate::mounts_text(
            &Root::open(candidate)?,
            sys.state.preparation.as_ref().unwrap().slot,
            &uuid,
            &crate::immutable::state_partition(&sys.target)?,
            &sys.backend.partition()?,
            &text,
        )?;
        Ok(text)
    })();
    for name in ["etc/os-release", format::SOURCE_CONFIG] {
        let valid = (|| -> Result<()> {
            let bytes = get(name)?;
            if name == format::SOURCE_CONFIG {
                let node = nodes
                    .get(name.as_bytes())
                    .context("release policy missing")?;
                ensure!(
                    node.kind == "file" && node.uid == 0 && node.mode & 0o022 == 0,
                    "resolved release policy must be a protected root-owned regular file"
                );
                let policy: format::Policy = serde_json::from_slice(&bytes)?;
                policy.validate()?;
                ensure!(
                    policy.sequence == j.next.identity.sequence
                        && policy.channel == j.next.identity.channel,
                    "resolved release policy lost incoming baseline sequence/channel"
                );
            } else {
                let os = std::str::from_utf8(&bytes)?;
                let id = &j.next.identity;
                ensure!(
                    distro_profile::os_release_value(os, "ID").as_deref()
                        == Some(id.distro.as_str())
                        && distro_profile::os_release_value(os, "VERSION_ID").as_deref()
                            == Some(id.version.as_str())
                        && distro_profile::os_release_fact::<distro_profile::InitSystem>(
                            os, "REC_INIT"
                        )? == id.init
                        && distro_profile::os_release_fact::<distro_profile::Libc>(os, "REC_LIBC")?
                            == id.libc
                        && distro_profile::os_release_fact::<distro_profile::Flavour>(
                            os,
                            "REC_FLAVOUR"
                        )? == id.flavour,
                    "resolved OS identity differs from incoming release"
                );
            }
            Ok(())
        })();
        if valid.is_err() {
            let cf = conflict(name.as_bytes(), "release_identity");
            j.choices.remove(&cf.id);
            add_conflict(j, cf);
        }
    }
    match rendered {
        Ok(text) => {
            let node = desired
                .entries
                .iter_mut()
                .find(|n| n.path == b"etc/fstab")
                .context("resolved fstab missing")?;
            node.content = Some(crate::generation::digest(text.as_bytes()));
            node.size = text.len() as u64;
            node.normalized = None;
            node.extents = None;
            write_work(candidate, "controls/etc/fstab", text.as_bytes(), 0o600)?;
            sources.insert(hex(b"etc/fstab"), "controls/etc/fstab".into());
        }
        Err(_) => {
            let cf = conflict(b"etc/fstab", "release_identity");
            j.choices.remove(&cf.id);
            add_conflict(j, cf);
        }
    }
    Ok(())
}
fn abi_changed(j: &Journal) -> bool {
    let old = map(&j.baseline.inventory);
    j.next.inventory.entries.iter().any(|n| {
        let p = path(&n.path).to_string_lossy();
        (p.starts_with("usr/lib") || p.starts_with("lib"))
            && (p.contains(".so") || p.contains("ld-"))
            && !equal(Some(n), old.get(&n.path))
    }) || old.values().any(|o| {
        let p = path(&o.path).to_string_lossy();
        (p.starts_with("usr/lib") || p.starts_with("lib"))
            && p.contains(".so")
            && !j.next.inventory.entries.iter().any(|n| n.path == o.path)
    })
}
fn packages(j: &mut Journal, candidate: &Path) -> Result<()> {
    let receipt = if let Some(receipt) = &j.receipt {
        receipt.clone()
    } else {
        levitate_recipe::system::Receipt::parse(
            Root::open(candidate)?
                .optional_text(Path::new(levitate_recipe::system::RECEIPT))?
                .as_deref(),
        )?
    };
    let old = map(&j.baseline.inventory);
    let new = map(&j.next.inventory);
    let runtime_changed = abi_changed(j);
    let kernel_changed = !equal(
        old.get(b"usr/lib/recab/kernel.json".as_slice()),
        new.get(b"usr/lib/recab/kernel.json".as_slice()),
    );
    for (name, package) in &receipt.packages {
        for dependency in &package.dependencies {
            ensure!(
                receipt.packages.contains_key(dependency),
                "selected package dependency missing"
            );
        }
        if !j.reproduced.contains(name) && (runtime_changed || kernel_changed) {
            let mut elf = false;
            for (file, e) in &package.files {
                if kernel_changed && file.starts_with("usr/lib/modules/") {
                    elf = true;
                    break;
                }
                if runtime_changed && (e.kind == "file" || e.kind == "hardlink") {
                    let mut f = Root::open(candidate)?
                        .entry(Path::new(file), false)?
                        .open()?;
                    let mut head = [0; 4];
                    if f.read(&mut head)? == 4 && &head == b"\x7fELF" {
                        elf = true;
                        break;
                    }
                }
            }
            if elf {
                add_conflict(j, conflict(name.as_bytes(), "package_rebuild"));
            }
        }
        // Release/base files may not take ownership of a selected local package.
        for file in package.files.keys() {
            if !equal(old.get(file.as_bytes()), new.get(file.as_bytes()))
                && new.contains_key(file.as_bytes())
            {
                add_conflict(j, conflict(file.as_bytes(), "package_ownership"));
            }
        }
    }
    Ok(())
}
pub(crate) fn start(
    sys: &System,
    release: &str,
    trusted_local: bool,
    allow_flavour: bool,
) -> Result<MergeStatus> {
    crate::filesystem::private_mounts()?;
    crate::change::verify_source(sys)?;
    validate_request(sys, None, None)?;
    let previous = if journal_path(sys)?.exists() {
        let j = checked(sys)?;
        if j.release == release {
            return resume(sys, j);
        }
        ensure!(j.phase == "complete", "another release merge is pinned");
        Some(j)
    } else {
        None
    };
    let p = sys.state.preparation.as_ref().unwrap();
    let v = crate::release::for_target(
        &sys.target,
        &sys.target
            .state_dir
            .join("releases")
            .join(release)
            .join(format::MANIFEST),
        trusted_local,
        allow_flavour,
    )?;
    ensure!(v.id() == release, "merge release identity changed");
    let (directory, _mounted) = mounted(sys, &v)?;
    let mount = SlotMount::open(sys, p.slot, &p.id)?;
    let old = match load_baseline(&mount.point) {
        Ok(old) => old,
        Err(_) => {
            let blocked=MergeStatus{preparation:p.id.clone(),revision:p.revision,release:release.into(),phase:"blocked".into(),conflicts:vec![conflict(BASELINE.as_bytes(),"missing_or_unsafe_baseline")],unresolved:1,report:None,failure:Some("original release baseline unavailable; adopt the verified original release, never capture the personalized system as O".into())};
            save(
                &sys.target
                    .state_dir
                    .join(format!("merge-blocked-{}.json", p.id)),
                &blocked,
            )?;
            mount.close()?;
            return Ok(blocked);
        }
    };
    let (installed, _) =
        crate::release::installed(&crate::target::Target::installed(&mount.point)?)?;
    ensure!(
        old.identity.distro == installed.distro
            && old.identity.architecture == installed.architecture
            && old.identity.init == installed.init
            && old.identity.libc == installed.libc
            && old.identity.flavour == installed.flavour
            && old.identity.channel == installed.channel
            && old.identity.sequence == installed.sequence,
        "release baseline differs from installed release identity"
    );
    ensure!(
        v.manifest().identity.sequence > old.identity.sequence
            || (allow_flavour
                && v.manifest().identity.sequence == old.identity.sequence
                && v.manifest().identity.flavour != old.identity.flavour),
        "incoming release is not newer than the prepared release baseline"
    );
    let mut j = Journal {
        version: 1,
        preparation: p.id.clone(),
        source_generation: p.source_generation.clone(),
        revision: p.revision,
        release: release.into(),
        trusted_local,
        allow_flavour,
        phase: "conflicted".into(),
        current: inventory(&mount.point)?,
        current_accounts: crate::merge_accounts::baseline(&crate::merge_accounts::read(
            &mount.point,
        )?),
        baseline: old,
        next: baseline(directory.path(), v.manifest().identity.clone())?,
        conflicts: vec![],
        choices: BTreeMap::new(),
        desired: None,
        sources: BTreeMap::new(),
        removals: vec![],
        report: None,
        failure: None,
        reproduced: BTreeSet::new(),
        overrides: vec![],
        override_sources: BTreeMap::new(),
        package_removals: BTreeSet::new(),
        receipt: None,
        completed_digest: None,
        previous: previous
            .as_ref()
            .map(|old| format!("{}-{}.json", old.preparation, old.release)),
    };
    ensure!(
        !j.current.entries.iter().any(|n| path(&n.path)
            .file_name()
            .is_some_and(|v| v == format!(".recab-sync-{}", j.preparation).as_str())),
        "captured system contains the merge scratch name"
    );
    j.next.release_id = Some(j.release.clone());
    assigned_labels(&mut j.next.inventory, &j.baseline.inventory);
    account_metadata(&mut j);
    let _ = changes(&mut j)?;
    let current = crate::merge_accounts::read(&mount.point)?;
    let new = crate::merge_accounts::read(directory.path())?;
    let (_, conflicts) =
        crate::merge_accounts::merge(&j.baseline.accounts, &current, &new, &BTreeMap::new())?;
    for file in conflicts {
        add_conflict(&mut j, conflict(file.as_bytes(), "account_fields"));
    }
    packages(&mut j, &mount.point)?;
    mount.close()?;
    if let Some(previous) = &previous {
        let archive = sys.target.state_dir.join("completed-merges");
        fs::create_dir_all(&archive)?;
        save(&archive.join(j.previous.as_ref().unwrap()), previous)?;
        target_root::sync_dir(&archive)?;
    }
    let blocked = sys
        .target
        .state_dir
        .join(format!("merge-blocked-{}.json", p.id));
    if blocked.exists() {
        fs::remove_file(blocked)?;
    }
    persist(sys, &mut j)?;
    drop(_mounted);
    drop(directory);
    drop(v);
    let sys = System::read(None)?;
    resume(&sys, j)
}
fn resume(sys: &System, mut j: Journal) -> Result<MergeStatus> {
    if j.revision == sys.state.preparation.as_ref().unwrap().revision + 1 {
        publish_state(sys, &j, j.phase == "complete")?;
    }
    if j.phase == "complete" {
        return Ok(public(&j));
    }
    if unresolved(&j) > 0 {
        return Ok(public(&j));
    }
    let was_frozen = j.desired.is_some();
    let p = sys.state.preparation.as_ref().unwrap();
    let v = input(sys, &j)?;
    let (directory, _image) = mounted(sys, &v)?;
    let mut mount = SlotMount::open(sys, p.slot, &p.id)?;
    let result = (|| -> Result<()> {
        if j.phase == "validated" {
            mount.writable(sys)?;
            ensure!(
                j.completed_digest.as_ref() == Some(&completed_inventory(&mount.point)?),
                "validated candidate changed before merge completion"
            );
            ensure!(
                serde_json::to_vec(&load_baseline(&mount.point)?)? == serde_json::to_vec(&j.next)?,
                "validated release baseline changed"
            );
            let uuid = sys.backend.slot_partuuid(&sys.boot, p.slot)?;
            crate::candidate::check(
                &mount.point,
                &sys.target.root,
                p.slot,
                &uuid,
                &crate::immutable::state_partition(&sys.target)?,
                &sys.backend.partition()?,
                &v.kernel(),
                &v.initramfs(),
                &sys.state
                    .require_ready(sys.boot.selection.active)?
                    .kernel_digest,
                j.allow_flavour,
            )?;
            if fs::symlink_metadata(mount.point.join(WORK)).is_ok() {
                Root::open(&mount.point)?
                    .entry(Path::new(WORK), false)?
                    .remove()?;
            }
            crate::filesystem::flush(&Root::open(&mount.point)?)?;
            j.phase = "complete".into();
            j.failure = None;
            return Ok(());
        }
        if j.desired.is_none() {
            ensure!(
                inventory(&mount.point)?.digest()? == j.current.digest()?,
                "prepared changes changed after merge capture; refusing to recalculate X"
            );
            let (mut desired, mut sources) = changes(&mut j)?;
            if unresolved(&j) > 0 {
                persist(sys, &mut j)?;
                return Ok(());
            }
            mount.writable(sys)?;
            materialize_accounts(
                &mut j,
                &mount.point,
                directory.path(),
                &mut desired,
                &mut sources,
            )?;
            controls(
                &mut j,
                &mount.point,
                directory.path(),
                &mut desired,
                &mut sources,
                sys,
            )?;
            if unresolved(&j) > 0 {
                persist(sys, &mut j)?;
                return Ok(());
            }
            for (key, source) in &sources {
                if source != "current" && source != "release" {
                    format::relative(source)?;
                    let n = desired
                        .entries
                        .iter()
                        .find(|n| hex(&n.path) == *key)
                        .context("merge source path missing")?;
                    let mut file =
                        levitate_recipe::fsx::open_regular(&mount.point.join(WORK).join(source))?;
                    ensure!(
                        crate::generation::hash_reader(&mut file)?
                            == *n.content.as_ref().context("merge source hash missing")?,
                        "frozen edited/account input changed"
                    );
                }
            }
            let before = map(&j.current);
            let after = map(&desired);
            j.removals = before
                .keys()
                .filter(|p| {
                    !p.is_empty() && after.get(*p).is_none_or(|n| n.kind != before[*p].kind)
                })
                .cloned()
                .collect();
            j.sources = sources;
            j.desired = Some(desired);
            j.phase = "applying".into();
            j.failure = None;
            crate::filesystem::flush(&Root::open(&mount.point)?)?;
            persist(sys, &mut j)?;
        } else {
            mount.writable(sys)?;
        }
        let desired = j.desired.as_ref().unwrap();
        // A killed inode-detach can leave its private temporary file. C never
        // contained this name; remove only this plan's regular scratch inode.
        let scratch = format!(".recab-sync-{}", j.preparation);
        let root = Root::open(&mount.point)?;
        for parent in desired
            .entries
            .iter()
            .filter_map(|n| path(&n.path).parent())
            .collect::<BTreeSet<_>>()
        {
            let temp = parent.join(&scratch);
            match root.metadata(&temp) {
                Ok(Some(m)) => {
                    ensure!(m.is_file() && m.nlink() == 1, "unsafe merge scratch inode");
                    fs::remove_file(root.entry(&temp, false)?.path())?;
                }
                Ok(None) => {}
                Err(e)
                    if e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                        matches!(
                            e.raw_os_error(),
                            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                        )
                    }) => {}
                Err(e) => return Err(e),
            }
        }
        let desired_map = map(desired);
        let current = inventory(&mount.point)?;
        let captured = map(&j.current);
        for n in &current.entries {
            let key = hex(&n.path);
            if j.sources.get(&key).is_some_and(|s| s == "current") {
                ensure!(
                    equal(Some(n), captured.get(&n.path)),
                    "captured local change altered during recovery"
                );
            }
            ensure!(
                desired_map.contains_key(&n.path) || captured.contains_key(&n.path),
                "unexpected system entry appeared during merge recovery"
            );
        }
        let mut sources = BTreeMap::new();
        for (key, source) in &j.sources {
            let p = unhex(key)?;
            if source == "current" {
                sources.insert(p.clone(), mount.point.join(path(&p)));
            } else if source != "release" {
                format::relative(source)?;
                let expected = desired_map
                    .get(&p)
                    .context("frozen source target missing")?;
                let file = mount.point.join(WORK).join(source);
                let mut f = levitate_recipe::fsx::open_regular(&file)?;
                ensure!(
                    crate::generation::hash_reader(&mut f)?
                        == *expected
                            .content
                            .as_ref()
                            .context("frozen input fingerprint missing")?,
                    "frozen merge input altered"
                );
                sources.insert(p, file);
            }
        }
        let uuid = sys.backend.slot_partuuid(&sys.boot, p.slot)?;
        let writer = crate::sync::Synchronization::merged(
            directory.path(),
            &mount.point,
            desired.clone(),
            &j.removals,
            p.slot,
            &uuid,
            &p.id,
            sources,
        )?;
        let report = writer.apply(SyncReport {
            accounting_complete: !was_frozen,
            resumed: was_frozen,
            ..Default::default()
        })?;
        writer.verify_selected(&mount.point)?;
        let final_map = map(&inventory(&mount.point)?);
        for name in &j.removals {
            ensure!(
                !final_map.contains_key(name) || desired_map.contains_key(name),
                "release deletion was not applied"
            );
        }
        crate::candidate::accounts(&Root::open(&mount.point)?)?;
        // Full boot/runtime validation uses the selected release's matching boot
        // set. No baseline is advanced merely because a conflict was resolved.
        crate::candidate::check(
            &mount.point,
            &sys.target.root,
            p.slot,
            &uuid,
            &crate::immutable::state_partition(&sys.target)?,
            &sys.backend.partition()?,
            &v.kernel(),
            &v.initramfs(),
            &sys.state
                .require_ready(sys.boot.selection.active)?
                .kernel_digest,
            j.allow_flavour,
        )?;
        assigned_labels(&mut j.next.inventory, &inventory(&mount.point)?);
        write_baseline(&mount.point, &j.next)?;
        // This checkpoint owns completion after private inputs are collected.
        // A kill after cleanup must not require those already consumed bodies.
        j.completed_digest = Some(completed_inventory(&mount.point)?);
        j.report = Some(report.clone());
        j.phase = "validated".into();
        j.failure = None;
        persist(sys, &mut j)?;
        if fs::symlink_metadata(mount.point.join(WORK)).is_ok() {
            Root::open(&mount.point)?
                .entry(Path::new(WORK), false)?
                .remove()?;
        }
        crate::filesystem::flush(&Root::open(&mount.point)?)?;
        j.report = Some(report);
        j.phase = "complete".into();
        j.failure = None;
        Ok(())
    })();
    let close = mount.seal();
    if let Err(error) = result.and(close) {
        j.failure =
            Some("merge application/validation interrupted; recover the pinned plan".into());
        save(&journal_path(sys)?, &j)?;
        return Err(crate::error::classified(
            crate::ErrorClass::PartialUpdate,
            format!(
                "release merge interrupted; candidate remains unbootable; recover the pinned plan: {error:#}"
            ),
        ));
    }
    if j.phase == "complete" {
        j.revision += 1;
        save(&journal_path(sys)?, &j)?;
        publish_state(sys, &j, true)?;
    }
    Ok(public(&j))
}
pub fn recover(expected: Option<&str>, revision: Option<u64>) -> Result<MergeStatus> {
    let sys = System::locked("update-recover")?;
    crate::filesystem::private_mounts()?;
    crate::change::verify_source(&sys)?;
    validate_request(&sys, expected, revision)?;
    resume(&sys, checked(&sys)?)
}
pub fn conflicts() -> Result<Option<MergeStatus>> {
    status(&System::read(None)?)
}
pub fn resolve(
    preparation: &str,
    revision: u64,
    conflict_id: &str,
    choice: &str,
    file: Option<&Path>,
) -> Result<MergeStatus> {
    let sys = System::locked("update-resolve")?;
    crate::filesystem::private_mounts()?;
    crate::change::verify_source(&sys)?;
    validate_request(&sys, Some(preparation), Some(revision))?;
    let mut j = checked(&sys)?;
    ensure!(
        j.revision == revision,
        "merge journal publication needs recovery first"
    );
    ensure!(
        j.desired.is_none() && j.phase != "complete",
        "a started merge must recover its frozen plan"
    );
    let cf = j
        .conflicts
        .iter()
        .find(|c| c.id == conflict_id)
        .context("unknown merge conflict")?
        .clone();
    ensure!(
        cf.kind != "package_ownership" || choice == "local",
        "keeping a release-owned collision requires cancelling and removing the conflicting package through Recipe, or reproducing it with noncolliding paths"
    );
    ensure!(
        cf.kind != "package_rebuild",
        "this package must be reproduced through Recipe against the pinned release"
    );
    ensure!(
        ["local", "release", "edited"].contains(&choice),
        "resolution must be local, release or edited"
    );
    ensure!(
        (choice == "edited") == file.is_some(),
        "edited resolution requires --file; other resolutions do not"
    );
    let mut decision = Decision {
        choice: choice.into(),
        edited: None,
        work: None,
    };
    if let Some(file) = file {
        ensure!(
            cf.kind != "directory_tree",
            "directory conflicts select an entire side"
        );
        let file = fs::canonicalize(file)?;
        let mut source = levitate_recipe::fsx::open_regular(&file)?;
        ensure!(
            source.metadata()?.len() <= 1024 * 1024,
            "edited merge input exceeds 1 MiB"
        );
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes)?;
        let mut n = Inventory::capture_paths(
            file.parent().unwrap(),
            &[PathBuf::from(file.file_name().unwrap())],
        )?
        .entries
        .pop()
        .context("edited input missing")?;
        ensure!(
            n.kind == "file"
                && n.hardlink.is_none()
                && n.content.as_ref() == Some(&crate::generation::digest(&bytes)),
            "edited merge input changed"
        );
        n.path = unhex(&cf.path_hex)?;
        n.modified = 0;
        n.modified_ns = 0;
        n.normalized = None;
        n.extents = None;
        let mut mount = SlotMount::open(
            &sys,
            sys.state.preparation.as_ref().unwrap().slot,
            preparation,
        )?;
        ensure!(
            inventory(&mount.point)?.digest()? == j.current.digest()?,
            "captured prepared system changed"
        );
        mount.writable(&sys)?;
        let name = format!("edited/{conflict_id}");
        write_work(&mount.point, &name, &bytes, 0o600)?;
        crate::filesystem::flush(&Root::open(&mount.point)?)?;
        mount.seal()?;
        decision.edited = Some(n);
        decision.work = Some(name);
    }
    j.choices.insert(conflict_id.into(), decision);
    j.failure = None;
    persist(&sys, &mut j)?;
    let fresh = System::read(None)?;
    resume(&fresh, j)
}
pub fn adopt_baseline(
    manifest: &Path,
    trusted_local: bool,
    expected: Option<&str>,
    revision: Option<u64>,
) -> Result<()> {
    let prepared = if let Ok(sys) = System::read(None) {
        if sys
            .state
            .preparation
            .as_ref()
            .is_some_and(|p| p.operation == "update")
        {
            sys.state.preparation.unwrap()
        } else {
            crate::prepare(expected, revision)?
        }
    } else {
        crate::prepare(expected, revision)?
    };
    let sys = System::locked("update-adopt-baseline")?;
    crate::filesystem::private_mounts()?;
    crate::change::verify_source(&sys)?;
    validate_request(&sys, expected, revision)?;
    ensure!(
        !journal_path(&sys)?.exists(),
        "cannot replace a captured merge baseline"
    );
    let cached = crate::release::import_locked(&sys.target, manifest, trusted_local)?;
    let v = crate::release::for_target(
        &sys.target,
        &cached.cache.join(format::MANIFEST),
        trusted_local,
        false,
    )?;
    let installed = crate::release::installed(&sys.target)?.0;
    ensure!(
        v.manifest().identity == installed,
        "baseline must be the exact original installed release identity"
    );
    let (directory, _image) = mounted(&sys, &v)?;
    let mut b = baseline(directory.path(), v.manifest().identity.clone())?;
    b.release_id = Some(v.id());
    let mut mount = SlotMount::open(&sys, prepared.slot, &prepared.id)?;
    ensure!(
        Root::open(&mount.point)?
            .metadata(Path::new(BASELINE))?
            .is_none(),
        "a release baseline already exists"
    );
    assigned_labels(&mut b.inventory, &inventory(&mount.point)?);
    mount.writable(&sys)?;
    write_baseline(&mount.point, &b)?;
    mount.seal()?;
    let mut next = sys.state.clone();
    next.preparation.as_mut().unwrap().revision += 1;
    sys.store.save(&next)?;
    let blocked = sys
        .target
        .state_dir
        .join(format!("merge-blocked-{}.json", prepared.id));
    if blocked.exists() {
        fs::remove_file(blocked)?;
        target_root::sync_dir(&sys.target.state_dir)?;
    }
    Ok(())
}
pub(crate) fn can_cancel(sys: &System) -> Result<bool> {
    Ok(!journal_path(sys)?.exists() || { let j=checked(sys)?; j.phase=="complete" || j.desired.is_none() })
}
pub(crate) fn cancel(sys: &System) -> Result<()> {
    let blocked = sys.target.state_dir.join(format!(
        "merge-blocked-{}.json",
        sys.state
            .preparation
            .as_ref()
            .context("preparation missing")?
            .id
    ));
    if blocked.exists() {
        fs::remove_file(blocked)?;
        target_root::sync_dir(&sys.target.state_dir)?;
    }
    if !journal_path(sys)?.exists() {
        return Ok(());
    }
    let j = checked(sys)?;
    if j.phase == "complete" {
        return Ok(());
    }
    ensure!(
        j.desired.is_none(),
        "a partially applied release must recover; cancellation cannot reconstruct C"
    );
    let p = sys.state.preparation.as_ref().unwrap();
    crate::filesystem::private_mounts()?;
    let mut mount = SlotMount::open(sys, p.slot, &p.id)?;
    ensure!(
        inventory(&mount.point)?.digest()? == j.current.digest()?,
        "captured changes changed before cancellation"
    );
    mount.writable(sys)?;
    if fs::symlink_metadata(mount.point.join(WORK)).is_ok() {
        Root::open(&mount.point)?
            .entry(Path::new(WORK), false)?
            .remove()?;
    }
    crate::filesystem::flush(&Root::open(&mount.point)?)?;
    mount.seal()?;
    if let Some(previous) = &j.previous {
        format::relative(previous)?;
        let old: Journal = read(&sys.target.state_dir.join("completed-merges").join(previous))?;
        ensure!(
            old.phase == "complete"
                && old.preparation == j.preparation
                && old.source_generation == j.source_generation,
            "previous completed merge identity changed"
        );
        save(&journal_path(sys)?, &old)?;
    } else {
        fs::remove_file(journal_path(sys)?)?;
    }
    target_root::sync_dir(&sys.target.state_dir)?;
    Ok(())
}
pub(crate) fn discard(sys: &System, id: &str) -> Result<()> {
    let blocked = sys
        .target
        .state_dir
        .join(format!("merge-blocked-{id}.json"));
    if blocked.exists() {
        fs::remove_file(blocked)?;
    }
    let file = sys.target.state_dir.join(format!("merge-{id}.json"));
    if file.exists() {
        let j: Journal = read(&file)?;
        ensure!(j.preparation == id, "discarded merge identity mismatch");
        fs::remove_file(file)?;
        target_root::sync_dir(&sys.target.state_dir)?;
    }
    Ok(())
}
pub(crate) fn boot_inputs(sys: &System) -> Result<Option<crate::release::VerifiedRelease>> {
    if sys.state.preparation.is_none() || !journal_path(sys)?.exists() {
        return Ok(None);
    };
    let j = checked(sys)?;
    ensure!(
        j.phase == "complete",
        "release merge must complete before finalization"
    );
    Ok(Some(input(sys, &j)?))
}
/// Reproduce selection through the existing Recipe producer with a small signed
/// release context. The recipe must explicitly acknowledge the new base; no
/// old output or host package activation can satisfy this operation.
pub fn reproduce(
    preparation: &str,
    revision: u64,
    package: &str,
    recipe: Option<&Path>,
) -> Result<MergeStatus> {
    let sys = System::locked("update-reproduce")?;
    crate::filesystem::private_mounts()?;
    crate::change::verify_source(&sys)?;
    validate_request(&sys, Some(preparation), Some(revision))?;
    let mut j = checked(&sys)?;
    ensure!(
        j.revision == revision && j.desired.is_none() && j.phase != "complete",
        "merge publication needs recovery or has started applying"
    );
    let mut mount = SlotMount::open(
        &sys,
        sys.state.preparation.as_ref().unwrap().slot,
        preparation,
    )?;
    ensure!(
        inventory(&mount.point)?.digest()? == j.current.digest()?,
        "captured prepared changes changed"
    );
    let old = if let Some(receipt) = &j.receipt {
        receipt.clone()
    } else {
        levitate_recipe::system::Receipt::parse(
            Root::open(&mount.point)?
                .optional_text(Path::new(levitate_recipe::system::RECEIPT))?
                .as_deref(),
        )?
    };
    let selected = old
        .packages
        .get(package)
        .context("package is not selected")?;
    let verify_pinned = recipe.is_none();
    let recipe = fs::canonicalize(recipe.unwrap_or(&selected.recipe))
        .context("selected recipe unavailable; provide an explicit compatible --recipe")?;
    if verify_pinned {
        let declaration = levitate_recipe::needs::declaration(&recipe, None)?;
        ensure!(
            crate::generation::digest(&serde_json::to_vec(&declaration.source_digests)?)
                == selected.recipe_digest,
            "selected recipe changed; provide an explicit replacement recipe"
        );
    }
    let v = input(&sys, &j)?;
    let scope = crate::generation::digest(&serde_json::to_vec(&(
        &j.release,
        &j.preparation,
        j.revision,
        package,
    ))?);
    let request = crate::ChangeRequest {
        action: "update".into(),
        recipe: recipe.clone(),
        recipe_args: vec![
            "--no-persist-state".into(),
            "--force-install".into(),
            "--input".into(),
            format!("release={}", v.manifest_path().display()),
            "--define".into(),
            format!("RECAB_RELEASE_BASE={}", j.release),
        ],
        ..Default::default()
    };
    let result = crate::change::produce(&sys, &request, &scope, &recipe)?;
    ensure!(
        result["recab_release"].as_str() == Some(j.release.as_str()),
        "Recipe did not acknowledge RECAB_RELEASE_BASE in ctx.recab_release; no compatible reproduction published"
    );
    let payloads = levitate_recipe::system::payloads(&result, &scope)?;
    ensure!(
        payloads.iter().any(|p| p.package.name == package
            && (!verify_pinned
                || (p.package.recipe_digest == selected.recipe_digest
                    && p.package.version == selected.version))),
        "reproducer changed pinned package/source identity"
    );
    let affected = old.affected(&payloads, None);
    let mut observed = BTreeMap::new();
    for node in Inventory::capture_paths(
        &mount.point,
        &affected.iter().map(PathBuf::from).collect::<Vec<_>>(),
    )?
    .entries
    {
        observed.insert(
            String::from_utf8(node.path.clone())?,
            crate::change::logical(&node)?,
        );
    }
    // Earlier queued reproductions are observations for their own fingerprints;
    // no actual system path has been modified yet.
    for node in &j.overrides {
        let name = String::from_utf8(node.path.clone())?;
        if affected.contains(&name) {
            observed.insert(name, crate::change::logical(node)?);
        }
    }
    let plan = levitate_recipe::system::reconcile(&old, &payloads, None, &observed)?;
    mount.writable(&sys)?;
    let work = work_root(&mount.point)?;
    let mut overrides = BTreeMap::new();
    for payload in &payloads {
        ensure!(
            payload.package.architecture == std::env::consts::ARCH,
            "reproducer architecture mismatch"
        );
        let source = Root::open(&payload.root)?;
        for (to, from) in &payload.paths {
            let e = &payload.package.files[to];
            let mut node = Node {
                path: to.as_bytes().to_vec(),
                kind: if e.kind == "symlink" {
                    "link"
                } else if e.kind == "hardlink" {
                    "file"
                } else {
                    &e.kind
                }
                .into(),
                mode: e.mode,
                uid: e.uid,
                gid: e.gid,
                size: 0,
                modified: 0,
                modified_ns: 0,
                xattrs: e
                    .xattrs
                    .iter()
                    .map(|(k, v)| Ok((k.as_bytes().to_vec(), unhex(v)?)))
                    .collect::<Result<_>>()?,
                content: e.sha256.clone(),
                normalized: None,
                link: if e.kind == "symlink" {
                    e.link.as_ref().map(|v| v.as_bytes().to_vec())
                } else {
                    None
                },
                hardlink: if e.kind == "hardlink" {
                    e.link.as_ref().map(|v| v.as_bytes().to_vec())
                } else {
                    None
                },
                extents: None,
            };
            if node.kind == "file" {
                let name = format!("packages/{scope}/{}", hex(to.as_bytes()));
                let destination = Root::open(&work)?.entry(Path::new(&name), true)?;
                let mut output = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(destination.path())?;
                output.set_permissions(fs::Permissions::from_mode(0o600))?;
                let mut input = source.entry(Path::new(from), false)?.open()?;
                node.size = std::io::copy(&mut input, &mut output)?;
                output.sync_all()?;
                let mut copied = destination.open()?;
                ensure!(
                    crate::generation::hash_reader(&mut copied)?
                        == *node.content.as_ref().context("reproducer digest missing")?,
                    "reproduced payload changed"
                );
                j.override_sources.insert(hex(to.as_bytes()), name);
            } else {
                j.override_sources
                    .insert(hex(to.as_bytes()), "release".into());
            }
            overrides.insert(to.clone(), node);
        }
        j.reproduced.insert(payload.package.name.clone());
    }
    for removed in &plan.removals {
        if !plan.wanted.contains_key(removed) {
            j.package_removals.insert(removed.clone());
        }
    }
    let bytes = serde_json::to_vec(&plan.receipt)?;
    let name = format!("packages/{scope}/receipt");
    write_work(&mount.point, &name, &bytes, 0o600)?;
    let receipt_path = levitate_recipe::system::RECEIPT;
    overrides.insert(
        receipt_path.into(),
        Node {
            path: receipt_path.as_bytes().to_vec(),
            kind: "file".into(),
            mode: 0o644,
            uid: 0,
            gid: 0,
            size: bytes.len() as u64,
            modified: 0,
            modified_ns: 0,
            xattrs: vec![],
            content: Some(crate::generation::digest(&bytes)),
            normalized: None,
            link: None,
            hardlink: None,
            extents: None,
        },
    );
    j.override_sources
        .insert(hex(receipt_path.as_bytes()), name);
    j.receipt = Some(plan.receipt);
    j.overrides
        .retain(|n| !overrides.contains_key(&String::from_utf8_lossy(&n.path).into_owned()));
    j.overrides.extend(overrides.into_values());
    j.conflicts
        .retain(|c| c.kind != "package_rebuild" || !j.reproduced.contains(&c.path));
    packages(&mut j, &mount.point)?;
    crate::filesystem::flush(&Root::open(&mount.point)?)?;
    mount.seal()?;
    persist(&sys, &mut j)?;
    let fresh = System::read(None)?;
    resume(&fresh, j)
}
