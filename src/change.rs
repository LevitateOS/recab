//! Recipe produces isolated payloads; recab serializes and applies a bounded
//! selection to its inactive root. No build gets a whole-system input/export.
use crate::error::{ErrorClass, classified, refuse};
use crate::filesystem::Root;
use crate::generation::Node;
use crate::state::{PreparationPhase, Readiness};
use crate::system::System;
use crate::{Inventory, Slot, SyncReport};
use anyhow::{Context, Result};
use levitate_recipe::{
    system::{Payload, Plan, Receipt},
    target::Entry,
};
use serde::{Deserialize, Serialize};
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeProgress {
    pub id: String,
    pub action: String,
    pub package: String,
    pub report: Option<SyncReport>,
    /// Recipe /proc/self/io deltas, including reaped children and streams.
    /// Not physical SSD traffic; null means unmeasured.
    pub producer_io: Option<BTreeMap<String, u64>>,
    /// Entire recab invocation through cleanup, including the Recipe subprocess.
    /// Available in the returned result; the persisted receipt predates cleanup.
    /// Final result serialization follows this observation.
    pub invocation_io: Option<BTreeMap<String, u64>>,
    pub projection_bytes_written: u64,
}
#[derive(Clone, Debug, Default)]
pub struct ChangeRequest {
    pub action: String,
    pub recipe: PathBuf,
    pub producer_recipe: Option<PathBuf>,
    pub preparation: Option<String>,
    pub revision: Option<u64>,
    pub recipe_args: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    preparation: String,
    source: String,
    revision: u64,
    slot: Slot,
    uuid: String,
    projection: String,
    desired: Inventory,
    removals: Vec<String>,
    progress: ChangeProgress,
}
struct Projection {
    path: PathBuf,
    published: bool,
}
impl std::ops::Deref for Projection {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}
impl Drop for Projection {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
fn receipt(root: &Path) -> Result<Receipt> {
    Receipt::parse(
        Root::open(root)?
            .optional_text(Path::new(levitate_recipe::system::RECEIPT))?
            .as_deref(),
    )
}
pub(crate) fn logical(node: &Node) -> Result<Entry> {
    Ok(Entry {
        kind: if node.hardlink.is_some() {
            "hardlink"
        } else if node.kind == "link" {
            "symlink"
        } else {
            &node.kind
        }
        .into(),
        mode: node.mode,
        uid: node.uid,
        gid: node.gid,
        user: None,
        group: None,
        xattrs: node
            .xattrs
            .iter()
            .map(|(n, v)| Ok((String::from_utf8(n.clone())?, hex(v))))
            .collect::<Result<_>>()?,
        link: node
            .hardlink
            .as_ref()
            .or(node.link.as_ref())
            .map(|p| String::from_utf8(p.clone()))
            .transpose()?,
        sha256: node.content.clone(),
    })
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(refuse("invalid xattr bytes"));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(Into::into))
        .collect()
}
fn journal_path(sys: &System, id: &str) -> PathBuf {
    sys.target.state_dir.join(format!("change-{id}.json"))
}
pub(crate) fn verify_source(sys: &System) -> Result<()> {
    crate::immutable::require_readonly(&sys.target)?;
    let s = sys.boot.selection.active;
    let g = sys.state.require_ready(s)?;
    g.verify_root(
        &sys.target.root,
        s,
        &sys.backend.slot_partuuid(&sys.boot, s)?,
    )?;
    sys.backend.verify_generation(&sys.boot, s, g)
}
fn matches(sys: &System, id: &str, revision: u64) -> Result<()> {
    let p = sys
        .state
        .preparation
        .as_ref()
        .context("prepare the inactive system first")?;
    if p.id != id || p.revision != revision {
        return Err(refuse("stale preparation id or revision"));
    }
    if p.source_slot != sys.boot.selection.active
        || p.source_generation != sys.state.require_ready(p.source_slot)?.id
    {
        return Err(refuse("preparation belongs to another active generation"));
    }
    if !p.synchronized {
        return Err(refuse("synchronization must finish before a system change"));
    }
    Ok(())
}
fn observe(root: &Path, paths: &BTreeSet<String>) -> Result<BTreeMap<String, Entry>> {
    Inventory::capture_paths(root, &paths.iter().map(PathBuf::from).collect::<Vec<_>>())?
        .entries
        .iter()
        .map(|n| Ok((String::from_utf8(n.path.clone())?, logical(n)?)))
        .collect()
}
fn write_projection(root: &Path, path: &str, bytes: &[u8]) -> Result<()> {
    Root::open(root)?
        .entry(Path::new(path), true)?
        .write(bytes, 0o600)
}
fn projection(
    sys: &System,
    id: &str,
    payloads: &[Payload],
    plan: &Plan,
    destination: &Path,
) -> Result<(Projection, Inventory, u64)> {
    let path = sys.target.state_dir.join(format!("change-payload-{id}"));
    fs::create_dir(&path)?;
    let projection = Projection {
        path: path.clone(),
        published: false,
    };
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    let out = Root::open(&path)?;
    let mut written = 0;
    for payload in payloads {
        let input = Root::open(&payload.root)?;
        for (target, from) in &payload.paths {
            let e = &payload.package.files[target];
            let dest = out.entry(Path::new(target), true)?;
            if dest.metadata()?.is_some() {
                continue;
            }
            match e.kind.as_str() {
                "dir" => {
                    fs::create_dir(dest.path())?;
                }
                "symlink" => std::os::unix::fs::symlink(
                    e.link.as_ref().context("missing symlink")?,
                    dest.path(),
                )?,
                "file" | "hardlink" => {
                    let mut source = input.entry(Path::new(from), false)?.open()?;
                    let mut output = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(dest.path())?;
                    written += std::io::copy(&mut source, &mut output)?;
                    output.sync_all()?;
                    if crate::generation::hash_reader(
                        &mut input.entry(Path::new(from), false)?.open()?,
                    )? != *e
                        .sha256
                        .as_ref()
                        .context("producer content digest missing")?
                    {
                        return Err(refuse("producer payload changed after publication"));
                    }
                }
                _ => return Err(refuse("unsupported intended target kind")),
            }
        }
    }
    let data = serde_json::to_vec(&plan.receipt)?;
    write_projection(&path, levitate_recipe::system::RECEIPT, &data)?;
    written += data.len() as u64;
    let mut inventory = Inventory::capture(&path)?;
    inventory.entries.retain(|n| !n.path.is_empty());
    // Apply logical target metadata to nodes, never staging UID/modes/grants.
    for n in &mut inventory.entries {
        let name = std::str::from_utf8(&n.path)?;
        if let Some(e) = plan.wanted.get(name) {
            n.kind = if e.kind == "hardlink" {
                "file"
            } else if e.kind == "symlink" {
                "link"
            } else {
                &e.kind
            }
            .into();
            n.uid = e.uid;
            n.gid = e.gid;
            n.mode = e.mode;
            n.xattrs = e
                .xattrs
                .iter()
                .map(|(n, v)| Ok((n.as_bytes().to_vec(), unhex(v)?)))
                .collect::<Result<_>>()?;
            n.hardlink = if e.kind == "hardlink" {
                Some(e.link.as_ref().unwrap().as_bytes().to_vec())
            } else {
                None
            };
            if n.kind == "file" && n.content != e.sha256 {
                return Err(refuse("projected payload differs from intended digest"));
            }
        } else {
            n.uid = 0;
            n.gid = 0;
            n.mode = if n.kind == "dir" { 0o755 } else { 0o644 };
        }
        n.modified = 0;
        n.modified_ns = 0;
        // Existing directory metadata is not package staging policy. Child
        // writes restore its original timestamp/ACL without claiming its data.
        if n.kind == "dir" {
            if let Some(existing) = Inventory::capture_paths(destination, &[PathBuf::from(name)])?
                .entries
                .pop()
                .filter(|n| n.kind == "dir")
            {
                *n = existing;
            }
        }
    }
    crate::filesystem::flush(&out)?;
    Ok((projection, inventory, written))
}
pub(crate) fn produce(
    sys: &System,
    request: &ChangeRequest,
    scope: &str,
    recipe: &Path,
) -> Result<serde_json::Value> {
    let dir = sys.target.state_dir.join("recipe").join(scope);
    fs::create_dir_all(&dir)
        .and_then(|()| fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)))
        .with_context(|| format!("preparing Recipe work directory '{}'", dir.display()))
        .map_err(|error| crate::error::or_class(error, ErrorClass::StateOrDetection))?;
    for arg in &request.recipe_args {
        if [
            "--state-dir",
            "--build-dir",
            "--package-root",
            "--json-output",
            "--system",
            "--prepared",
            "--no-activate",
        ]
        .iter()
        .any(|f| arg == *f || arg.starts_with(&format!("{f}=")))
            || arg.contains("RECAB_SYSTEM_CONTEXT")
        {
            return Err(refuse(
                "recab owns producer target/state/activation options",
            ));
        }
    }
    // Producer state lives in a scoped cwd; caller paths still refer to the
    // directory in which recab was invoked.
    let mut args = Vec::new();
    let mut values = request.recipe_args.iter();
    while let Some(arg) = values.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(f, v)| (f, Some(v)));
        if matches!(
            flag,
            "--recipes-path"
                | "-r"
                | "--input"
                | "--autofix-cwd"
                | "--autofix-prompt-file"
                | "--autofix-allow-path"
        ) {
            let value = inline
                .or_else(|| values.next().map(String::as_str))
                .context("producer path option needs a value")?;
            let value = if flag == "--input" {
                let (name, path) = value.split_once('=').context("input must be NAME=PATH")?;
                format!("{name}={}", std::path::absolute(path)?.display())
            } else {
                std::path::absolute(value)?.to_string_lossy().into_owned()
            };
            args.extend([flag.to_owned(), value]);
        } else {
            args.push(arg.clone());
        }
    }
    let program = std::env::var_os("RECAB_RECIPE_BIN").unwrap_or_else(|| "recipe".into());
    let result = dir.join("result.json");
    let mut cmd = Command::new(program);
    cmd.current_dir(&dir)
        .arg("install")
        .arg(recipe)
        .args(&args)
        .args(["--no-activate", "--json", "--state-dir"])
        .arg(dir.join("state"))
        .arg("--build-dir")
        .arg(dir.join("build"))
        .arg("--json-output")
        .arg(&result)
        .arg("--define")
        .arg(format!("RECAB_SYSTEM_CONTEXT={scope}"));
    let status = cmd.status().context("starting Recipe producer")?;
    if !status.success() {
        return Err(refuse(
            "Recipe failed; no payload was published to the prepared system",
        ));
    }
    let file = fs::File::open(result)?;
    if file.metadata()?.len() > 8 * 1024 * 1024 {
        return Err(refuse("producer result exceeds 8 MiB"));
    }
    Ok(serde_json::from_reader(file)?)
}
pub fn change(request: &ChangeRequest) -> Result<ChangeProgress> {
    let before = levitate_recipe::system::process_io();
    let mut progress = change_inner(request)?;
    progress.invocation_io = Some(io_delta(&before));
    Ok(progress)
}
fn io_delta(before: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    levitate_recipe::system::process_io()
        .into_iter()
        .map(|(k, v)| {
            let delta = v.saturating_sub(*before.get(&k).unwrap_or(&0));
            (k, delta)
        })
        .collect()
}
fn change_inner(request: &ChangeRequest) -> Result<ChangeProgress> {
    if request.action == "recover" {
        return recover(request.preparation.as_deref(), request.revision, false);
    }
    if !matches!(request.action.as_str(), "install" | "update" | "remove") {
        return Err(refuse("unsupported system change operation"));
    }
    let prepared = crate::prepare(request.preparation.as_deref(), request.revision)?;
    let sys = System::locked("change")?;
    crate::filesystem::private_mounts()?;
    verify_source(&sys)?;
    matches(&sys, &prepared.id, prepared.revision)?;
    let p = sys.state.preparation.as_ref().unwrap();
    if p.change.is_some() {
        return Err(refuse(
            "recover or discard the interrupted system change first",
        ));
    }
    let slot = p.slot;
    let uuid = sys.backend.slot_partuuid(&sys.boot, slot)?;
    let mount = crate::slot_mount::SlotMount::open(&sys, slot, &p.id)?;
    let old = receipt(&mount.point)?;
    let removing = if request.action == "remove" {
        Some(old.find(&request.recipe)?.name.clone())
    } else {
        None
    };
    let recipe = if let Some(path) = &request.producer_recipe {
        path.clone()
    } else if request.action == "update" {
        old.find(&request.recipe)?.recipe.clone()
    } else {
        request.recipe.clone()
    };
    let recipe = if removing.is_none() && (recipe.exists() || recipe.components().count() > 1) {
        fs::canonicalize(&recipe)?
    } else {
        recipe
    };
    let scope = crate::generation::digest(&serde_json::to_vec(&(
        p.source_generation.clone(),
        &p.id,
        p.revision,
        std::env::consts::ARCH,
    ))?);
    let produced = if removing.is_some() {
        None
    } else {
        Some(produce(&sys, request, &scope, &recipe)?)
    };
    let producer_io = produced
        .as_ref()
        .map(|v| serde_json::from_value(v["producer_io"].clone()))
        .transpose()?;
    let payloads = if let Some(produced) = &produced {
        levitate_recipe::system::payloads(produced, &scope)?
    } else {
        vec![]
    };
    for payload in &payloads {
        if payload.package.architecture != std::env::consts::ARCH {
            return Err(refuse("producer architecture differs from this system"));
        }
    }
    if request.action == "update" {
        let name = &old.find(&request.recipe)?.name;
        if !payloads.iter().any(|p| &p.package.name == name) {
            return Err(refuse(
                "updated producer changed the selected package identity",
            ));
        }
    }
    let affected = old.affected(&payloads, removing.as_deref());
    let observed = observe(&mount.point, &affected)?;
    let plan = levitate_recipe::system::reconcile(&old, &payloads, removing.as_deref(), &observed)?;
    let name = removing
        .clone()
        .or_else(|| payloads.last().map(|p| p.package.name.clone()))
        .unwrap();
    let id = crate::prepare::new_id()?;
    let (mut source, desired, bytes) = projection(&sys, &id, &payloads, &plan, &mount.point)?;
    // Capacity and all predictable path/type failures precede durable intent.
    drop(crate::sync::Synchronization::selected(
        &source,
        &mount.point,
        desired.clone(),
        &plan.removals,
        slot,
        &uuid,
        &id,
    )?);
    mount.close()?;
    let progress = ChangeProgress {
        id: id.clone(),
        action: request.action.clone(),
        package: name,
        report: None,
        producer_io,
        invocation_io: None,
        projection_bytes_written: bytes,
    };
    let journal = Journal {
        version: 1,
        preparation: p.id.clone(),
        source: p.source_generation.clone(),
        revision: p.revision,
        slot,
        uuid,
        projection: source.file_name().unwrap().to_str().unwrap().into(),
        desired,
        removals: plan.removals,
        progress: progress.clone(),
    };
    // Once intent publication starts its outcome may be durable even on error.
    // Retain the frozen payload for recovery; preflight errors clean it up.
    source.published = true;
    target_root::write_durable(
        &journal_path(&sys, &p.id),
        &serde_json::to_vec(&journal)?,
        Some(0o600),
    )?;
    let mut state = sys.state.clone();
    let p = state.preparation.as_mut().unwrap();
    p.change = Some(progress);
    p.operation = "change".into();
    p.phase = PreparationPhase::Interrupted;
    p.failure = Some("system publication must finish".into());
    p.revision += 1;
    state.slot_mut(slot)?.seal = None;
    state.slot_mut(slot)?.readiness = Readiness::Incomplete;
    sys.store.save(&state)?;
    drop(sys);
    recover(Some(&journal.preparation), Some(journal.revision + 1), true)
}
fn recover(id: Option<&str>, revision: Option<u64>, fresh: bool) -> Result<ChangeProgress> {
    let sys = System::locked("recover-change")?;
    crate::filesystem::private_mounts()?;
    verify_source(&sys)?;
    let p = sys.state.preparation.as_ref().context("no preparation")?;
    matches(&sys, id.unwrap_or(&p.id), revision.unwrap_or(p.revision))?;
    let path = journal_path(&sys, &p.id);
    let text = Root::open(&sys.target.state_dir)?
        .optional_text(Path::new(path.file_name().unwrap()))?
        .context("no pending system change journal")?;
    let journal: Journal = serde_json::from_str(&text)?;
    if journal.progress.id.len() != 64
        || !journal.progress.id.bytes().all(|b| b.is_ascii_hexdigit())
        || journal.revision + 1 != p.revision
        || journal.uuid != sys.backend.slot_partuuid(&sys.boot, p.slot)?
        || journal.version != 1
        || journal.preparation != p.id
        || journal.source != p.source_generation
        || journal.slot != p.slot
        || !p
            .change
            .as_ref()
            .is_some_and(|c| c.id == journal.progress.id)
    {
        return Err(refuse("system change journal identity mismatch"));
    }
    if journal.projection != format!("change-payload-{}", journal.progress.id) {
        return Err(refuse("unsafe system payload name"));
    }
    let source = sys.target.state_dir.join(&journal.projection);
    let mut mount = crate::slot_mount::SlotMount::open(&sys, p.slot, &p.id)?;
    let result = (|| -> Result<ChangeProgress> {
        sys.backend.invalidate_entry(&sys.boot, p.slot)?;
        // Re-read and verify every staged byte before any writable root access.
        for node in &journal.desired.entries {
            let staged = Inventory::capture_paths(
                &source,
                &[PathBuf::from(std::ffi::OsStr::from_bytes(&node.path))],
            )?;
            if !staged.entries.first().is_some_and(|n| {
                n.kind == node.kind
                    && n.content == node.content
                    && n.link == node.link
                    && n.size == node.size
            }) {
                return Err(refuse("staged system payload is missing or altered"));
            }
        }
        mount.writable(&sys)?;
        let sync = crate::sync::Synchronization::selected(
            &source,
            &mount.point,
            journal.desired.clone(),
            &journal.removals,
            p.slot,
            &journal.uuid,
            &journal.progress.id,
        )?;
        let mut progress = journal.progress.clone();
        let report = sync.apply(SyncReport {
            resumed: !fresh,
            accounting_complete: fresh,
            ..Default::default()
        })?;
        sync.verify_selected(&mount.point)?;
        if !Inventory::capture_paths(
            &mount.point,
            &journal
                .removals
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
        )?
        .entries
        .is_empty()
        {
            return Err(refuse("selected removal is incomplete"));
        }
        drop(sync);
        mount.seal()?;
        progress.report = Some(report);
        let mut next = sys.state.clone();
        let p = next.preparation.as_mut().unwrap();
        p.change = None;
        p.last_change = Some(progress.clone());
        p.changes.push(crate::state::ChangeRecord {
            id: progress.id.clone(),
            action: progress.action.clone(),
            package: progress.package.clone(),
            unix_time: crate::store::now()?,
            actor: crate::actor(),
        });
        p.operation = "prepare".into();
        p.phase = PreparationPhase::Prepared;
        p.failure = None;
        p.revision += 1;
        sys.store.save(&next)?;
        let cleanup = (|| -> Result<()> {
            fs::remove_file(&path)?;
            target_root::sync_dir(&sys.target.state_dir)?;
            fs::remove_dir_all(&source)?;
            Ok(())
        })();
        if let Err(error) = cleanup {
            eprintln!(
                "recab: system change completed; private payload cleanup failed at {}: {error:#}",
                source.display()
            );
        }
        Ok(progress)
    })();
    result.map_err(|e|classified(ErrorClass::PartialUpdate,format!("prepared system change is incomplete; recover it before editing or finalizing: {e:#}")))
}
pub fn changes() -> Result<serde_json::Value> {
    let sys = System::read(None)?;
    let active = receipt(&sys.target.root)?;
    let prepared = if let Some(p) = &sys.state.preparation {
        if unsafe { libc::geteuid() } != 0 || p.change.is_some() {
            None
        } else {
            crate::filesystem::private_mounts()?;
            let m = crate::slot_mount::SlotMount::open(&sys, p.slot, &p.id)?;
            let r = receipt(&m.point)?;
            m.close()?;
            Some(r)
        }
    } else {
        None
    };
    Ok(
        serde_json::json!({"active":active,"prepared":prepared,"preparation":sys.state.preparation,"activation":"pending reboot for prepared changes"}),
    )
}
pub fn edit(
    path: &Path,
    file: Option<&Path>,
    id: Option<&str>,
    revision: Option<u64>,
) -> Result<ChangeProgress> {
    let before = levitate_recipe::system::process_io();
    let mut progress = edit_inner(path, file, id, revision)?;
    progress.invocation_io = Some(io_delta(&before));
    Ok(progress)
}
fn edit_inner(
    path: &Path,
    file: Option<&Path>,
    id: Option<&str>,
    revision: Option<u64>,
) -> Result<ChangeProgress> {
    let path = path.strip_prefix("/").unwrap_or(path);
    let name = path.to_str().context("configuration path must be UTF-8")?;
    levitate_recipe::system::allowed(name)?;
    let prepared = crate::prepare(id, revision)?;
    let sys = System::locked("edit")?;
    crate::filesystem::private_mounts()?;
    verify_source(&sys)?;
    matches(&sys, &prepared.id, prepared.revision)?;
    let p = sys.state.preparation.as_ref().unwrap();
    let uuid = sys.backend.slot_partuuid(&sys.boot, p.slot)?;
    let mount = crate::slot_mount::SlotMount::open(&sys, p.slot, &p.id)?;
    let opened = Root::open(&mount.point)?;
    let mut original = opened.entry(path, false)?.open()?;
    if original.metadata()?.len() > 1024 * 1024 {
        return Err(refuse("configuration edit exceeds 1 MiB"));
    }
    let prior = Inventory::capture_paths(&mount.point, &[path.to_path_buf()])?
        .entries
        .pop()
        .context("configuration file missing")?;
    let mut bytes = Vec::new();
    original.read_to_end(&mut bytes)?;
    drop(original);
    drop(opened);
    let id = crate::prepare::new_id()?;
    let temp = sys.target.state_dir.join(format!("edit-{id}"));
    fs::create_dir(&temp)?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o700))?;
    let edited = temp.join("edited");
    let work = (|| -> Result<ChangeProgress> {
        if let Some(file) = file {
            let canonical = fs::canonicalize(file)?;
            let input = levitate_recipe::fsx::open_regular(&canonical)?;
            if input.metadata()?.len() > 1024 * 1024 {
                return Err(refuse("configuration edit exceeds 1 MiB"));
            }
            bytes.clear();
            input.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        } else {
            fs::write(&edited, &bytes)?;
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
            let mut words = editor.split_whitespace();
            if !Command::new(words.next().context("EDITOR is empty")?)
                .args(words)
                .arg(&edited)
                .status()?
                .success()
            {
                return Err(refuse("editor failed; candidate was not changed"));
            }
            let input = levitate_recipe::fsx::open_regular(&edited)?;
            if input.metadata()?.len() > 1024 * 1024 {
                return Err(refuse("configuration edit exceeds 1 MiB"));
            }
            bytes.clear();
            input.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        }
        if bytes.len() > 1024 * 1024 {
            return Err(refuse("configuration edit exceeds 1 MiB"));
        }
        fs::write(&edited, &bytes)?;
        let mut entry = logical(&prior)?;
        entry.sha256 = Some(crate::generation::digest(&bytes));
        let payload = Payload {
            package: levitate_recipe::system::Package {
                name: "recab-edit".into(),
                version: "1".into(),
                recipe: PathBuf::new(),
                recipe_digest: String::new(),
                architecture: std::env::consts::ARCH.into(),
                target: p.id.clone(),
                dependencies: vec![],
                files: BTreeMap::from([(name.into(), entry.clone())]),
            },
            root: temp.clone(),
            paths: BTreeMap::from([(name.into(), "edited".into())]),
            removals: vec![],
        };
        let plan = Plan {
            receipt: receipt(&mount.point)?,
            wanted: BTreeMap::from([(name.into(), entry)]),
            removals: vec![],
        };
        let (mut source, desired, written) =
            projection(&sys, &id, &[payload], &plan, &mount.point)?;
        drop(crate::sync::Synchronization::selected(
            &source,
            &mount.point,
            desired.clone(),
            &[],
            p.slot,
            &uuid,
            &id,
        )?);
        mount.close()?;
        let progress = ChangeProgress {
            id: id.clone(),
            action: "edit".into(),
            package: name.into(),
            report: None,
            producer_io: Some(BTreeMap::new()),
            invocation_io: None,
            projection_bytes_written: written,
        };
        let journal = Journal {
            version: 1,
            preparation: p.id.clone(),
            source: p.source_generation.clone(),
            revision: p.revision,
            slot: p.slot,
            uuid,
            projection: source.file_name().unwrap().to_str().unwrap().into(),
            desired,
            removals: vec![],
            progress: progress.clone(),
        };
        source.published = true;
        target_root::write_durable(
            &journal_path(&sys, &p.id),
            &serde_json::to_vec(&journal)?,
            Some(0o600),
        )?;
        let mut state = sys.state.clone();
        let p = state.preparation.as_mut().unwrap();
        p.change = Some(progress.clone());
        p.operation = "change".into();
        p.phase = PreparationPhase::Interrupted;
        p.failure = Some("configuration publication must finish".into());
        p.revision += 1;
        state.slot_mut(journal.slot)?.seal = None;
        sys.store.save(&state)?;
        Ok(progress)
    })();
    let prep = p.id.clone();
    let rev = p.revision + 1;
    drop(sys);
    let _ = fs::remove_dir_all(temp);
    work?;
    recover(Some(&prep), Some(rev), true)
}

/// A discard removes only the matching operation's private payload, never data
/// in either system or the Recipe producer caches.
pub(crate) fn discard(sys: &System, id: &str) -> Result<()> {
    let path = journal_path(sys, id);
    let Some(text) =
        Root::open(&sys.target.state_dir)?.optional_text(Path::new(path.file_name().unwrap()))?
    else {
        return Ok(());
    };
    let journal: Journal = serde_json::from_str(&text)?;
    if journal.preparation != id
        || journal.progress.id.len() != 64
        || !journal.progress.id.bytes().all(|b| b.is_ascii_hexdigit())
        || journal.projection != format!("change-payload-{}", journal.progress.id)
    {
        return Err(refuse("unsafe discarded system change journal"));
    }
    let payload = sys.target.state_dir.join(journal.projection);
    if payload.exists() {
        fs::remove_dir_all(payload)?;
    }
    fs::remove_file(path)?;
    target_root::sync_dir(&sys.target.state_dir)?;
    Ok(())
}
