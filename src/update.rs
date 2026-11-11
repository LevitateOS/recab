//! Recipe owns every network request. This coordinator verifies signed metadata,
//! retains pinned inputs and never opens a writable system for check/download.
use crate::{filesystem::Root, system::System, target::Target};
pub(crate) mod production;
use anyhow::{Context, Result, ensure};
use distro_contract::release::{
    self as format, Artifact, Identity, Policy, ReleaseIndex, ReleaseManifest,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseSummary {
    pub id: String,
    pub sequence: u64,
    pub version: String,
    pub channel: String,
    pub total_bytes: u64,
}
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadProgress {
    pub release: String,
    pub verified_files: u32,
    pub total_files: u32,
    pub verified_bytes: u64,
    pub total_bytes: u64,
    pub reused_files: u32,
    pub producer_write_bytes: u64,
    pub producer_cancelled_write_bytes: u64,
    pub downloaded: bool,
    /// False when a previous failed producer may have written unreported bytes.
    pub accounting_complete: bool,
    #[serde(default)]
    pub assembly_write_bytes: u64,
    #[serde(default)]
    pub verified_parts: u32,
    #[serde(default)]
    pub reused_parts: u32,
}
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateStatus {
    pub phase: String,
    pub highest_index: u64,
    pub available: Option<ReleaseSummary>,
    pub download: Option<DownloadProgress>,
    pub pending_release: Option<String>,
    pub pending_preparation: Option<String>,
    pub last_error: Option<String>,
    /// P08 selects/validates the release; filesystem merging is owned by P09.
    pub merge_complete: bool,
    #[serde(default)]
    pub merge: Option<crate::merge::MergeStatus>,
}
#[derive(Default, Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    source: String,
    #[serde(default)]
    feed: String,
    #[serde(default)]
    production: bool,
    highest_index: u64,
    index_hash: String,
    highest_release: Option<(u64, String)>,
    view: UpdateStatus,
}
#[derive(Serialize, Deserialize)]
struct Pin {
    schema_version: u32,
    source: String,
    id: String,
    manifest_json: String,
    signature: Vec<u8>,
    base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    production: Option<production::Proof>,
}
#[derive(Serialize, Deserialize)]
struct Pending {
    schema_version: u32,
    preparation: String,
    source_generation: String,
    revision: u64,
    release: String,
    trusted_local: bool,
    #[serde(default)]
    allow_flavour: bool,
}
fn directory(target: &Target) -> PathBuf {
    target.state_dir.join("updates")
}
fn record_path(target: &Target) -> PathBuf {
    directory(target).join("status.json")
}
fn valid_id(id: &str) -> Result<()> {
    format::hash(id)
}
fn load<T: serde::de::DeserializeOwned>(path: &Path, bound: u64) -> Result<T> {
    let file = levitate_recipe::fsx::open_regular(path)?;
    ensure!(
        file.metadata()?.len() <= bound,
        "update record exceeds bound"
    );
    Ok(serde_json::from_reader(file.take(bound + 1))?)
}
fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    Ok(target_root::write_durable(
        path,
        &serde_json::to_vec(value)?,
        Some(0o600),
    )?)
}
fn root_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let root = Root::open(path)?;
    ensure!(
        fs::metadata(root.location()?)?.uid() == 0,
        "update cache must be root-owned"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn floor_path(target: &Target, feed: &str) -> Result<PathBuf> {
    valid_id(feed)?;
    Ok(directory(target).join("floors").join(format!("{feed}.json")))
}
fn floor(target: &Target, feed: &str) -> Result<format::production::RollbackFloor> {
    let path = floor_path(target, feed)?;
    if path.try_exists()? { load(&path, 65536) } else { Ok(Default::default()) }
}
fn record(target: &Target, source: &str, identity: &Identity, policy: &Policy) -> Result<Record> {
    // Delivery/Recipe revisions and the installed release sequence are not a
    // new authority. A shared floor outlives both slot and cache identities.
    let feed = if let Some(production) = &policy.production {
        production.feed_id(identity)?
    } else {
        format::digest(&serde_json::to_vec(&(
            &policy.trust, &identity.distro, &identity.channel,
            &identity.architecture, identity.init, identity.libc, identity.flavour,
        ))?)
    };
    let mut value = if record_path(target).try_exists()? {
        let old: Record = load(&record_path(target), 4 * 1024 * 1024)?;
        ensure!(old.schema_version == 1, "unsupported update record");
        if old.source == source { old } else { Record::default() }
    } else { Record::default() };
    value.schema_version = 1;
    value.source = source.into();
    value.feed = feed;
    value.production = policy.production.is_some();
    if value.view.phase.is_empty() { value.view.phase = "idle".into(); }
    let floor = floor(target, &value.feed)?;
    if policy.production.is_some() {
        value.highest_index = floor.index_sequence;
        value.index_hash = floor.index_digest.clone();
        value.highest_release = floor.release.clone();
    }
    if floor.index_sequence > value.highest_index {
        value.highest_index = floor.index_sequence;
        value.index_hash = floor.index_digest;
    } else if floor.index_sequence == value.highest_index && floor.index_sequence > 0 {
        ensure!(floor.index_digest == value.index_hash, "stored index floor equivocation");
    }
    if let Some((sequence, id)) = floor.release {
        match &value.highest_release {
            Some((s, old)) if *s == sequence => ensure!(old == &id, "stored release floor equivocation"),
            Some((s, _)) if *s > sequence => {},
            _ => value.highest_release = Some((sequence, id)),
        }
    }
    value.view.highest_index = value.highest_index;
    Ok(value)
}
fn persist(target: &Target, record: &mut Record) -> Result<()> {
    root_dir(&directory(target))?;
    if record.production {
        record.view.highest_index = floor(target, &record.feed)?.index_sequence;
        return save(&record_path(target), record);
    }
    let previous = floor(target, &record.feed)?;
    let mut next = previous.clone();
    ensure!(record.highest_index >= next.index_sequence && (record.highest_index == 0
        || record.highest_index != next.index_sequence || record.index_hash == next.index_digest),
        "index rollback/equivocation refused by shared authority floor");
    if record.highest_index > 0 {
        valid_id(&record.index_hash)?;
        next.index_sequence = record.highest_index;
        next.index_digest = record.index_hash.clone();
    }
    if let Some((sequence, id)) = &record.highest_release { next.release(*sequence, id)?; }
    if next != previous {
        let path = floor_path(target, &record.feed)?;
        root_dir(path.parent().unwrap())?;
        // Floors precede convenience status: a crash cannot forget acceptance.
        save(&path, &next)?;
    }
    record.view.highest_index = record.highest_index;
    save(&record_path(target), record)
}
pub(crate) fn configuration(sys: &System) -> Result<(Identity, Policy, String, PathBuf)> {
    crate::change::verify_source(sys)?;
    source_configuration(&sys.target)
}
fn source_configuration(target: &Target) -> Result<(Identity, Policy, String, PathBuf)> {
    let (identity, policy) = crate::release::installed(target)?;
    let source=policy.source.as_ref().context("no update source configured; the image producer must supply release.source and distribution release trust")?;
    ensure!(
        policy.trust.is_some() || policy.production.is_some(),
        "no distribution release key configured"
    );
    let root = Root::open(&target.root)?;
    let mut file = root.entry(Path::new(&source.recipe), false)?.open()?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == 0 && metadata.mode() & 0o022 == 0 && metadata.len() <= 1024 * 1024,
        "acquisition recipe must be a bounded root-owned slot file"
    );
    let recipe_hash = crate::generation::hash_reader(&mut file)?;
    let id = if let Some(production) = &policy.production { production.feed_id(&identity)? } else { format::digest(&serde_json::to_vec(&(
        &policy,
        &identity.distro,
        &identity.architecture,
        identity.init,
        identity.libc,
        identity.flavour,
        recipe_hash,
    ))?) };
    let recipe = target.root.join(&source.recipe);
    Ok((identity, policy, id, recipe))
}
fn url(source: &str, member: &str) -> Result<String> {
    format::relative(member)?;
    ensure!(
        !member.contains([':', '?', '#', '%']),
        "unsafe update URL member"
    );
    let base = url::Url::parse(source)?;
    let resolved = base.join(member)?;
    ensure!(
        base.origin() == resolved.origin(),
        "update payload leaves configured origin"
    );
    Ok(resolved.to_string())
}
fn pin_path(target: &Target, id: &str) -> PathBuf {
    directory(target).join("pins").join(format!("{id}.json"))
}
fn checked_pin(
    target: &Target,
    id: &str,
    source: &str,
    identity: &Identity,
    policy: &Policy,
) -> Result<(Pin, ReleaseManifest)> {
    valid_id(id)?;
    let pin: Pin = load(&pin_path(target, id), 4 * 1024 * 1024)?;
    if policy.production.is_some() { return production::checked_pin(target, id, source, identity, policy, pin); }
    ensure!(
        pin.schema_version == 1
            && pin.id == id
            && pin.source == source
            && format::digest(pin.manifest_json.as_bytes()) == id,
        "pinned update identity/source mismatch"
    );
    format::verify_signature(
        pin.manifest_json.as_bytes(),
        &pin.signature,
        policy.trust.as_ref().context("release trust missing")?,
    )?;
    let manifest = ReleaseManifest::parse(pin.manifest_json.as_bytes())?;
    manifest.matches(identity)?;
    manifest
        .compatibility
        .check(crate::release::RECAB_VERSION)?;
    ensure!(
        manifest.identity.sequence > identity.sequence,
        "pinned release is not newer than the installed baseline"
    );
    let configured = url::Url::parse(
        &policy
            .source
            .as_ref()
            .context("update source missing")?
            .index,
    )?;
    ensure!(
        url::Url::parse(&pin.base_url)?.origin() == configured.origin(),
        "pinned update leaves configured origin"
    );
    Ok((pin, manifest))
}
struct Acquired {
    file: fs::File,
    scope: PathBuf,
    write_bytes: u64,
    cancelled: u64,
}
impl Drop for Acquired {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.scope);
    }
}
fn acquire(
    target: &Target,
    recipe: &Path,
    policy: &Policy,
    uri: &str,
    sha: &str,
    size: u64,
) -> Result<Acquired> {
    let (origins, prefixes) = if let Some(production) = &policy.production {
        let origins = production.request_origins(uri)?;
        let prefixes = if production.metadata_url(uri).is_ok() { vec![production.metadata_base.clone()] }
            else { std::iter::once(production.asset_prefix.clone()).chain(production.redirect_origins.iter().map(|o|format!("{o}/"))).collect() };
        (origins, Some(prefixes))
    } else {
        let origin = url::Url::parse(&policy.source.as_ref().context("source missing")?.index)?;
        ensure!(url::Url::parse(uri)?.origin() == origin.origin(), "acquisition leaves configured origin");
        (vec![origin.origin().ascii_serialization()], None)
    };
    let scope = directory(target)
        .join("recipe")
        .join(crate::prepare::new_id()?);
    root_dir(&scope)?;
    let request = scope.join("request.json");
    let mut input = serde_json::json!({"url":uri,"sha256":sha,"size":size});
    if let Some(prefixes) = prefixes { input["prefixes"] = serde_json::to_value(prefixes)?; }
    save(&request, &input)?;
    let output = scope.join("result.json");
    eprintln!(
        "recab: acquiring {} ({} bytes{})",
        uri,
        size,
        if sha.is_empty() {
            " maximum"
        } else {
            " verified"
        }
    );
    let mut command = Command::new(std::env::var_os("RECAB_RECIPE_BIN").unwrap_or_else(|| "recipe".into()));
    command
            .current_dir(&scope)
            .arg("install")
            .arg(recipe)
            .args([
                "--no-activate",
                "--no-persist-state",
                "--force-install",
                "--json",
                "--state-dir",
            ])
            .arg(scope.join("state"))
            .arg("--build-dir")
            .arg(scope.join("build"))
            .arg("--json-output")
            .arg(&output)
            .arg("--input")
            .arg(format!("request={}", request.display()))

            .arg("--network-max-bytes")
            .arg(size.saturating_add(size / 50).saturating_add(1024 * 1024).to_string())
            .stdout(std::process::Stdio::from(std::io::stderr()));
    for origin in origins { command.arg("--network-host").arg(format::endpoint(&origin)?); }
    let status = command.status().context("starting Recipe update acquisition")?;
    ensure!(
        status.success(),
        "Recipe acquisition failed; completed objects are retained for retry; diagnostics: {}",
        scope.display()
    );
    let result: serde_json::Value = load(&output, 2 * 1024 * 1024)?;
    let path = Path::new(
        result["outputs"]["artifact"]
            .as_str()
            .context("Recipe did not publish artifact output")?,
    );
    let file = levitate_recipe::fsx::open_regular(path)?;
    ensure!(
        file.metadata()?.len() <= size && (sha.is_empty() || file.metadata()?.len() == size),
        "acquired object length mismatch"
    );
    Ok(Acquired {
        file,
        write_bytes: result["producer_io"]["write_bytes"].as_u64().unwrap_or(0),
        cancelled: result["producer_io"]["cancelled_write_bytes"]
            .as_u64()
            .unwrap_or(0),
        scope,
    })
}
fn metadata(
    target: &Target,
    recipe: &Path,
    policy: &Policy,
    uri: &str,
    sha: &str,
    size: u64,
) -> Result<Vec<u8>> {
    let mut result = acquire(target, recipe, policy, uri, sha, size)?;
    let mut bytes = Vec::new();
    result
        .file
        .by_ref()
        .take(size + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= size
            && (sha.is_empty() || (bytes.len() as u64 == size && format::digest(&bytes) == sha)),
        "acquired metadata digest/size mismatch"
    );
    Ok(bytes)
}
fn summarize(id: &str, manifest: &ReleaseManifest) -> ReleaseSummary {
    ReleaseSummary {
        id: id.into(),
        sequence: manifest.identity.sequence,
        version: manifest.identity.version.clone(),
        channel: manifest.identity.channel.clone(),
        total_bytes: manifest.artifacts.all().iter().map(|a| a.size).sum(),
    }
}
pub(crate) fn cached_summary(target:&Target,id:&str)->Result<ReleaseSummary> {
    valid_id(id)?;
    let root=Root::open(&target.state_dir.join("releases").join(id))?;
    let mut bytes=Vec::new();root.entry(Path::new(format::MANIFEST),false)?.open()?.take(format::MAX_MANIFEST as u64+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len()<=format::MAX_MANIFEST && crate::generation::digest(&bytes)==id,"cached manifest identity mismatch");
    let manifest:ReleaseManifest=serde_json::from_slice(&bytes)?;
    Ok(summarize(id,&manifest))
}
pub fn check() -> Result<UpdateStatus> {
    let sys = System::locked("update-check")?;
    let (identity, policy, source, recipe) = configuration(&sys)?;
    if policy.production.is_some() { return production::check(&sys, &identity, &policy, &source, &recipe); }
    let mut state = record(&sys.target, &source, &identity, &policy)?;
    state.view.phase = "checking".into();
    state.view.last_error = None;
    persist(&sys.target, &mut state)?;
    let result = (|| -> Result<()> {
        let index_url = &policy.source.as_ref().unwrap().index;
        let raw = metadata(
            &sys.target,
            &recipe,
            &policy,
            index_url,
            "",
            format::MAX_MANIFEST as u64,
        )?;
        let hash = format::digest(&raw);
        let signature = metadata(
            &sys.target,
            &recipe,
            &policy,
            &url(index_url, &format!("index-signatures/{hash}.sig"))?,
            "",
            64,
        )?;
        format::verify_signature(&raw, &signature, policy.trust.as_ref().unwrap())?;
        let index = ReleaseIndex::parse(&raw)?;
        ensure!(
            index.channel == policy.channel,
            "update index channel mismatch"
        );
        ensure!(
            index.sequence >= state.highest_index
                && (index.sequence != state.highest_index
                    || state.index_hash.is_empty()
                    || hash == state.index_hash),
            "stale/equivocating update index refused"
        );
        state.highest_index = index.sequence;
        state.index_hash = hash;
        state.view.available = None;
        persist(&sys.target, &mut state)?;
        let selected = index
            .releases
            .iter()
            .filter(|e| {
                e.identity.distro == identity.distro
                    && e.identity.architecture == identity.architecture
                    && e.identity.init == identity.init
                    && e.identity.libc == identity.libc
                    && e.identity.flavour == identity.flavour
            })
            .max_by_key(|e| e.identity.sequence);
        let Some(entry) = selected else {
            state.view.phase = "up_to_date".into();
            return Ok(());
        };
        if let Some((floor, id)) = &state.highest_release {
            ensure!(
                entry.identity.sequence >= *floor
                    && (entry.identity.sequence != *floor || entry.manifest.sha256 == *id),
                "stale/equivocating indexed release refused"
            );
        }
        state.highest_release = Some((entry.identity.sequence, entry.manifest.sha256.clone()));
        persist(&sys.target, &mut state)?;
        if entry.identity.sequence <= identity.sequence {
            state.view.phase = "up_to_date".into();
            return Ok(());
        }
        let raw = metadata(
            &sys.target,
            &recipe,
            &policy,
            &url(index_url, &entry.manifest.path)?,
            &entry.manifest.sha256,
            entry.manifest.size,
        )?;
        let signature = metadata(
            &sys.target,
            &recipe,
            &policy,
            &url(index_url, &entry.signature.path)?,
            &entry.signature.sha256,
            entry.signature.size,
        )?;
        format::verify_signature(&raw, &signature, policy.trust.as_ref().unwrap())?;
        let manifest = ReleaseManifest::parse(&raw)?;
        manifest.matches(&identity)?;
        manifest
            .compatibility
            .check(crate::release::RECAB_VERSION)?;
        ensure!(
            manifest.identity == entry.identity,
            "indexed release identity differs from signed manifest"
        );
        let id = format::digest(&raw);
        let base_url = url::Url::parse(&url(index_url, &entry.manifest.path)?)?
            .join("./")?
            .to_string();
        root_dir(pin_path(&sys.target, &id).parent().unwrap())?;
        save(
            &pin_path(&sys.target, &id),
            &Pin {
                schema_version: 1,
                source: source.clone(),
                id: id.clone(),
                manifest_json: String::from_utf8(raw)?,
                signature,
                base_url,
                production: None,
            },
        )?;
        state.view.available = Some(summarize(&id, &manifest));
        state.view.phase = "available".into();
        Ok(())
    })();
    if let Err(e) = &result {
        state.view.phase = "check_failed".into();
        state.view.last_error = Some(format!("{e:#}").chars().take(2048).collect());
    }
    persist(&sys.target, &mut state)?;
    result?;
    view(&sys)
}
fn matches_file(path: &Path, artifact: &Artifact) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut file = levitate_recipe::fsx::open_regular(path)?;
    Ok(file.metadata()?.len() == artifact.size
        && crate::generation::hash_reader(&mut file)? == artifact.sha256)
}
fn publish_file(
    mut object: Acquired,
    destination: &Path,
    artifact: &Artifact,
) -> Result<(u64, u64)> {
    ensure!(
        object.file.metadata()?.len() == artifact.size
            && crate::generation::hash_reader(&mut object.file)? == artifact.sha256,
        "Recipe payload differs from signed digest"
    );
    fs::create_dir_all(destination.parent().unwrap())?;
    // Publication objects are append-only. Link the verified inode, not a
    // pathname reopened after hashing; the private transfer pins it against GC.
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
    let empty = c"";
    let rc = unsafe {
        libc::linkat(
            object.file.as_raw_fd(),
            empty.as_ptr(),
            libc::AT_FDCWD,
            name.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        ensure!(
            matches!(
                error.raw_os_error(),
                Some(libc::EXDEV | libc::EPERM | libc::EOPNOTSUPP)
            ),
            "cannot pin acquired object: {error}"
        );
        object.file.seek(SeekFrom::Start(0))?;
        let mut file = tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
        std::io::copy(&mut object.file, &mut file)?;
        file.as_file().sync_all()?;
        file.persist(destination)?;
    }
    object.file.sync_all()?;
    target_root::sync_dir(destination.parent().unwrap())?;
    Ok((object.write_bytes, object.cancelled))
}
pub fn download(requested: Option<&str>) -> Result<UpdateStatus> {
    let sys = System::locked("update-download")?;
    let (identity, policy, source, recipe) = configuration(&sys)?;
    let mut state = record(&sys.target, &source, &identity, &policy)?;
    let id = requested
        .map(str::to_owned)
        .or_else(|| state.view.available.as_ref().map(|a| a.id.clone()))
        .context("no checked release selected; run recab update check")?;
    let (pin, manifest) = checked_pin(&sys.target, &id, &source, &identity, &policy)?;
    let transfer = directory(&sys.target).join("transfers").join(&id);
    let previously_interrupted = state
        .view
        .download
        .as_ref()
        .is_some_and(|p| p.release == id && !p.downloaded);
    root_dir(&transfer)?;
    let progress = DownloadProgress {
        release: id.clone(),
        accounting_complete: !previously_interrupted,
        total_files: 4,
        total_bytes: manifest.artifacts.all().iter().map(|a| a.size).sum(),
        ..Default::default()
    };
    state.view.download = Some(progress);
    state.view.phase = "downloading".into();
    state.view.last_error = None;
    persist(&sys.target, &mut state)?;
    let result = (|| -> Result<()> {
        let cached = sys
            .target
            .state_dir
            .join("releases")
            .join(&id)
            .join(format::MANIFEST);
        if cached.exists() {
            if pin.production.is_some() { production::cache_metadata(&sys.target, &policy, &pin, cached.parent().unwrap())?; }
            let verified = crate::release::for_target(&sys.target, &cached, false, false)?;
            ensure!(verified.id() == id, "cached release identity mismatch");
            let p = state.view.download.as_mut().unwrap();
            p.verified_files = 4;
            p.reused_files = 4;
            p.verified_bytes = p.total_bytes;
            p.downloaded = true;
            if pin.production.is_some() { production::complete(&sys.target, &pin)?; }
            state.view.phase = "downloaded".into();
            return Ok(());
        }
        target_root::write_durable(
            &transfer.join(format::MANIFEST),
            pin.manifest_json.as_bytes(),
            Some(0o600),
        )?;
        target_root::write_durable(
            &transfer.join(format::SIGNATURE),
            &pin.signature,
            Some(0o600),
        )?;
        if pin.production.is_some() {
            production::download(&sys.target, &recipe, &policy, &pin, &manifest, &transfer, &mut state)?;
        }
        if pin.production.is_none() {
        for artifact in manifest.artifacts.all() {
            let destination = transfer.join(&artifact.path);
            let reused = matches_file(&destination, artifact)?;
            if !reused {
                if fs::symlink_metadata(&destination).is_ok() {
                    fs::remove_file(&destination)?;
                }
                let object = acquire(
                    &sys.target,
                    &recipe,
                    &policy,
                    &url(&pin.base_url, &artifact.path)?,
                    &artifact.sha256,
                    artifact.size,
                )?;
                let (written, cancelled) = publish_file(object, &destination, artifact)?;
                let p = state.view.download.as_mut().unwrap();
                p.producer_write_bytes += written;
                p.producer_cancelled_write_bytes += cancelled;
            }
            let p = state.view.download.as_mut().unwrap();
            p.verified_files += 1;
            p.verified_bytes += artifact.size;
            if reused && pin.production.is_none() {
                p.reused_files += 1;
            }
            persist(&sys.target, &mut state)?;
        }
        }
        let imported =
            crate::release::import_locked(&sys.target, &transfer.join(format::MANIFEST), false)?;
        ensure!(imported.id == id, "downloaded release identity changed");
        if pin.production.is_some() { production::complete(&sys.target, &pin)?; }
        state.view.phase = "downloaded".into();
        state.view.download.as_mut().unwrap().downloaded = true;
        if let Err(error) = fs::remove_dir_all(&transfer) {
            eprintln!("recab: warning: completed transfer cleanup failed: {error}");
        }
        Ok(())
    })();
    if let Err(e) = &result {
        state.view.phase = "interrupted".into();
        state.view.download.as_mut().unwrap().accounting_complete = false;
        state.view.last_error = Some(format!("{e:#}").chars().take(2048).collect());
    }
    persist(&sys.target, &mut state)?;
    result?;
    view(&sys)
}
fn pending_path(target: &Target, id: &str) -> PathBuf {
    target.state_dir.join(format!("update-apply-{id}.json"))
}
fn pending(sys: &System) -> Result<Option<Pending>> {
    let Some(p) = &sys.state.preparation else {
        return Ok(None);
    };
    if p.operation != "update" {
        return Ok(None);
    }
    let value: Pending = load(&pending_path(&sys.target, &p.id), 65536)?;
    ensure!(
        value.schema_version == 1
            && value.preparation == p.id
            && value.source_generation == p.source_generation
            && value
                .revision
                .checked_add(1)
                .is_some_and(|r| p.revision >= r),
        "pending update preparation identity mismatch"
    );
    valid_id(&value.release)?;
    Ok(Some(value))
}
pub(crate) fn view(sys: &System) -> Result<UpdateStatus> {
    let mut view = if record_path(&sys.target).exists() {
        let stored: Record = load(&record_path(&sys.target), 4 * 1024 * 1024)?;
        ensure!(stored.schema_version == 1, "unsupported update record");
        if let Ok((identity, policy, source, _)) = source_configuration(&sys.target) {
            record(&sys.target, &source, &identity, &policy)?.view
        } else {
            UpdateStatus {
                phase: "idle".into(),
                ..Default::default()
            }
        }
    } else {
        UpdateStatus {
            phase: "idle".into(),
            ..Default::default()
        }
    };
    if let Ok((identity,_))=crate::release::installed(&sys.target) {
        if view.available.as_ref().is_some_and(|r|r.sequence<=identity.sequence) {view.available=None;}
        if view.download.as_ref().is_some_and(|d|cached_summary(&sys.target,&d.release).is_ok_and(|r|r.sequence<=identity.sequence)) {view.download=None;}
    }
    if let Some(value) = pending(sys)? {
        view.phase = "awaiting_merge".into();
        view.pending_release = Some(value.release);
        view.pending_preparation = Some(value.preparation);
    }
    if let Some(merge) = crate::merge::status(sys)?.filter(|m| {
        view.pending_release
            .as_ref()
            .is_none_or(|id| id == &m.release)
    }) {
        view.merge_complete = merge.phase == "complete";
        view.phase = merge.phase.clone();
        if !view.merge_complete {
            view.pending_release = Some(merge.release.clone());
            view.pending_preparation = Some(merge.preparation.clone());
        }
        view.merge = Some(merge);
    }
    Ok(view)
}
pub fn status() -> Result<UpdateStatus> {
    view(&System::read(None)?)
}
/// P08 verifies/selects offline inputs. P09 consumes the receipt to merge; this
/// operation never silently falls back to destructive release-image staging.
pub fn apply(
    id: Option<&str>,
    manifest: Option<&Path>,
    trusted_local: bool,
    expected: Option<&str>,
    revision: Option<u64>,
) -> Result<UpdateStatus> {
    apply_options(id, manifest, trusted_local, expected, revision, false)
}
pub(crate) fn apply_options(
    id: Option<&str>,
    manifest: Option<&Path>,
    trusted_local: bool,
    expected: Option<&str>,
    revision: Option<u64>,
    allow_flavour: bool,
) -> Result<UpdateStatus> {
    let sys = System::locked("update-apply")?;
    crate::change::verify_source(&sys)?;
    if let Some(value) = pending(&sys)? {
        let retry_id = if let Some(manifest) = manifest {
            ensure!(
                id.is_none(),
                "choose a cached release or manifest, not both"
            );
            crate::release::import_locked_options(
                &sys.target,
                manifest,
                value.trusted_local,
                value.allow_flavour,
            )?
            .id
        } else {
            id.context("retry requires the selected release id or manifest")?
                .to_owned()
        };
        ensure!(
            retry_id == value.release
                && expected.is_none_or(|s| s == value.preparation)
                && revision.is_none_or(|r| r == sys.state.preparation.as_ref().unwrap().revision),
            "another update is selected or preparation request is stale"
        );
        let input = crate::release::for_target(
            &sys.target,
            &sys.target
                .state_dir
                .join("releases")
                .join(&value.release)
                .join(format::MANIFEST),
            value.trusted_local,
            value.allow_flavour,
        )?;
        ensure!(input.id() == value.release, "selected cache changed");
        drop(input);
        crate::merge::start(
            &sys,
            &value.release,
            value.trusted_local,
            value.allow_flavour,
        )?;
        return view(&System::read(None)?);
    }
    let id = if let Some(path) = manifest {
        ensure!(
            id.is_none(),
            "choose a cached release or manifest, not both"
        );
        crate::release::import_locked_options(&sys.target, path, trusted_local, allow_flavour)?.id
    } else {
        id.context("apply requires a cached release id or --manifest")?
            .to_owned()
    };
    valid_id(&id)?;
    let input = crate::release::for_target(
        &sys.target,
        &sys.target
            .state_dir
            .join("releases")
            .join(&id)
            .join(format::MANIFEST),
        trusted_local,
        allow_flavour,
    )?;
    ensure!(input.id() == id, "cached release identity mismatch");
    if crate::merge::status(&sys)?.is_some_and(|m| m.phase == "complete" && m.release == id) {
        let p = sys
            .state
            .preparation
            .as_ref()
            .context("preparation missing")?;
        ensure!(
            expected.is_none_or(|id| id == p.id) && revision.is_none_or(|r| r == p.revision),
            "stale preparation id/revision"
        );
        return view(&sys);
    }
    let (installed, _) = crate::release::installed(&sys.target)?;
    ensure!(
        input.manifest().identity.sequence > installed.sequence
            || (allow_flavour && input.manifest().identity.sequence == installed.sequence),
        "update must be newer than the installed baseline"
    );
    drop(input);
    drop(sys);
    let prepared = crate::prepare(expected, revision)?;
    let sys = System::locked("update-select")?;
    crate::change::verify_source(&sys)?;
    let p = sys
        .state
        .preparation
        .as_ref()
        .context("preparation missing")?;
    ensure!(
        p.id == prepared.id
            && p.revision == prepared.revision
            && p.operation == "prepare"
            && p.synchronized,
        "preparation changed while selecting update"
    );
    let input = crate::release::for_target(
        &sys.target,
        &sys.target
            .state_dir
            .join("releases")
            .join(&id)
            .join(format::MANIFEST),
        trusted_local,
        allow_flavour,
    )?;
    ensure!(input.id() == id, "cached release changed");
    save(
        &pending_path(&sys.target, &p.id),
        &Pending {
            schema_version: 1,
            preparation: p.id.clone(),
            source_generation: p.source_generation.clone(),
            revision: p.revision,
            release: id,
            trusted_local,
            allow_flavour,
        },
    )?;
    let mut next = sys.state.clone();
    let p = next.preparation.as_mut().unwrap();
    p.operation = "update".into();
    p.phase = crate::PreparationPhase::Interrupted;
    p.failure = Some("verified release selected; resume the captured release merge".into());
    p.revision += 1;
    sys.store.save(&next)?;
    let updated = System::read(None)?;
    crate::merge::start(
        &updated,
        &pending(&updated)?.unwrap().release,
        trusted_local,
        allow_flavour,
    )?;
    view(&System::read(None)?)
}
pub fn cancel(id: &str, revision: u64) -> Result<UpdateStatus> {
    let sys = System::locked("cancel-update")?;
    crate::change::verify_source(&sys)?;
    let selected = pending(&sys)?.context("no selected update")?;
    let p = sys.state.preparation.as_ref().unwrap();
    ensure!(
        p.id == id && p.revision == revision,
        "stale preparation id/revision"
    );
    crate::merge::cancel(&sys)?;
    let mut next = sys.state.clone();
    let p = next.preparation.as_mut().unwrap();
    p.operation = "prepare".into();
    p.phase = crate::PreparationPhase::Prepared;
    p.failure = None;
    p.revision += 1;
    sys.store.save(&next)?;
    let cleanup = fs::remove_file(pending_path(&sys.target, &selected.preparation))
        .and_then(|()| target_root::sync_dir(&sys.target.state_dir));
    if let Err(error) = cleanup {
        eprintln!("recab: warning: cancelled update receipt cleanup failed: {error}");
    }
    view(&System::read(None)?)
}
pub(crate) fn discard(sys: &System, id: &str) -> Result<()> {
    let path = pending_path(&sys.target, id);
    if path.exists() {
        let value: Pending = load(&path, 65536)?;
        ensure!(value.preparation == id, "discarded update receipt mismatch");
        fs::remove_file(path)?;
        target_root::sync_dir(&sys.target.state_dir)?;
    }
    Ok(())
}
