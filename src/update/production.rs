//! Version-2 authentication and part transport inside the existing updater.
use super::*;
use format::production::{
    self as protocol, AUTHORITY_FILE, AuthorityProof, MAX_AUTHORITY, ProductionEntry,
    ReleaseIndexV2, RollbackFloor, SigningRole, VerifiedAuthority,
};
use std::{
    collections::BTreeSet,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
};

pub(crate) fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Proof {
    authority: AuthorityProof,
    index: String,
    signature: Vec<u8>,
    accepted_at: u64,
    completed: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Known {
    proof: AuthorityProof,
    floor: RollbackFloor,
    revoked: BTreeSet<String>,
}
fn policy(policy: &Policy) -> Result<&protocol::ProductionPolicy> {
    policy
        .production
        .as_ref()
        .context("delegated production policy missing")
}
fn authority_path(target: &Target, identity: &Identity, policy: &Policy) -> Result<PathBuf> {
    let p = policy
        .production
        .as_ref()
        .context("production authority missing")?;
    let id = format::digest(&serde_json::to_vec(&(
        &p.root.lineage,
        &identity.distro,
        &identity.channel,
    ))?);
    Ok(directory(target)
        .join("authorities")
        .join(format!("{id}.json")))
}
fn root_proof(root: &Root, member: &Path) -> Result<AuthorityProof> {
    let file = root.entry(member, false)?.open()?;
    let md = file.metadata()?;
    ensure!(
        md.is_file() && md.uid() == 0 && md.mode() & 0o022 == 0 && md.len() <= MAX_AUTHORITY as u64,
        "initial authority must be a bounded root-owned slot file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_AUTHORITY as u64 + 1)
        .read_to_end(&mut bytes)?;
    AuthorityProof::parse(&bytes)
}
fn known(target: &Target, identity: &Identity, config: &Policy) -> Result<Known> {
    let p = policy(config)?;
    let initial = root_proof(
        &Root::open(&target.root)?,
        Path::new("usr/lib/recab/release-keys.json"),
    )?;
    let initial_auth = initial.verify_archived(p, &identity.distro, &identity.channel)?;
    let path = authority_path(target, identity, config)?;
    let mut out = if path.try_exists()? {
        let saved: Known = load(&path, 128 * 1024)?;
        let authenticated = saved
            .proof
            .verify_archived(p, &identity.distro, &identity.channel)?;
        let mut expected = saved.floor.clone();
        expected.authority(&authenticated)?;
        ensure!(
            expected == saved.floor
                && saved
                    .revoked
                    .is_superset(&authenticated.delegation().revoked),
            "stored authority floor/proof mismatch"
        );
        saved
    } else {
        Known {
            proof: initial.clone(),
            floor: Default::default(),
            revoked: Default::default(),
        }
    };
    if initial_auth.delegation().version >= out.floor.authority_version {
        out.floor.authority(&initial_auth)?;
        out.revoked
            .extend(initial_auth.delegation().revoked.iter().cloned());
        out.proof = initial;
    }
    Ok(out)
}
fn accept(
    target: &Target,
    identity: &Identity,
    config: &Policy,
    proof: AuthorityProof,
) -> Result<VerifiedAuthority> {
    let auth = proof.verify(policy(config)?, &identity.distro, &identity.channel, now()?)?;
    let mut previous = known(target, identity, config)?;
    previous.floor.authority(&auth)?;
    for key in auth
        .delegation()
        .release_keys
        .iter()
        .chain(&auth.delegation().index_keys)
    {
        ensure!(
            !previous.revoked.contains(&key.id()?),
            "root authorization attempts to reactivate a revoked signer"
        );
    }
    previous
        .revoked
        .extend(auth.delegation().revoked.iter().cloned());
    previous.proof = proof;
    let path = authority_path(target, identity, config)?;
    root_dir(path.parent().unwrap())?;
    // Persist revocation before trying the index or any release payload.
    save(&path, &previous)?;
    Ok(auth)
}
fn saved_authority(
    target: &Target,
    identity: &Identity,
    config: &Policy,
) -> Result<(Known, VerifiedAuthority)> {
    let known = known(target, identity, config)?;
    let auth = known
        .proof
        .verify_archived(policy(config)?, &identity.distro, &identity.channel)?;
    Ok((known, auth))
}
fn entry(index: &ReleaseIndexV2, id: &str) -> Result<ProductionEntry> {
    index
        .releases
        .iter()
        .find(|v| v.release.manifest.sha256 == id)
        .cloned()
        .context("pinned release is not in its signed index")
}
fn verify_proof(
    pin: &Pin,
    identity: &Identity,
    config: &Policy,
) -> Result<(
    VerifiedAuthority,
    ReleaseIndexV2,
    ProductionEntry,
    format::Trust,
)> {
    let proof = pin
        .production
        .as_ref()
        .context("production pin proof missing")?;
    let p = policy(config)?;
    let authority =
        proof
            .authority
            .verify(p, &identity.distro, &identity.channel, proof.accepted_at)?;
    let index = ReleaseIndexV2::authenticate(
        proof.index.as_bytes(),
        &proof.signature,
        &authority,
        p,
        proof.accepted_at,
    )?;
    let member = entry(&index, &pin.id)?;
    let manifest = ReleaseManifest::parse(pin.manifest_json.as_bytes())?;
    ensure!(
        format::digest(pin.manifest_json.as_bytes()) == pin.id
            && member.release.manifest.size == pin.manifest_json.len() as u64
            && member.release.signature.sha256 == format::digest(&pin.signature),
        "pinned metadata bytes differ from signed index"
    );
    member.for_manifest(&manifest, p)?;
    let signer = authority.verify(
        SigningRole::Release,
        pin.manifest_json.as_bytes(),
        &pin.signature,
    )?;
    Ok((authority, index, member, signer))
}
fn admission(
    target: &Target,
    pin: &Pin,
    identity: &Identity,
    config: &Policy,
    offline: bool,
) -> Result<ProductionEntry> {
    let (_, _, entry, signer) = verify_proof(pin, identity, config)?;
    let (known, current) = saved_authority(target, identity, config)?;
    ensure!(
        !known.revoked.contains(&signer.id()?),
        "cached release signer has been revoked"
    );
    if !offline {
        current.require_current(now()?)?;
        ensure!(
            current.delegation().release_keys.contains(&signer),
            "release signer is no longer authorized for acquisition"
        );
        ReleaseIndexV2::authenticate(
            pin.production.as_ref().unwrap().index.as_bytes(),
            &pin.production.as_ref().unwrap().signature,
            &current,
            policy(config)?,
            now()?,
        )?;
    }
    Ok(entry)
}
pub(super) fn checked_pin(
    target: &Target,
    id: &str,
    source: &str,
    identity: &Identity,
    config: &Policy,
    pin: Pin,
) -> Result<(Pin, ReleaseManifest)> {
    ensure!(
        pin.schema_version == 2 && pin.source == source && pin.id == id,
        "production pin identity/feed mismatch"
    );
    let cached = target
        .state_dir
        .join("releases")
        .join(id)
        .join(format::MANIFEST)
        .try_exists()?;
    admission(target, &pin, identity, config, cached)?;
    let manifest = ReleaseManifest::parse(pin.manifest_json.as_bytes())?;
    manifest.matches(identity)?;
    manifest
        .compatibility
        .check(crate::release::RECAB_VERSION)?;
    ensure!(
        manifest.identity.sequence > identity.sequence,
        "pinned release is not newer than installed baseline"
    );
    Ok((pin, manifest))
}
/// Select the signature authority for the existing complete release verifier.
/// Already published private cache entries can be used offline after expiry;
/// signatures, known revocations and every artifact are still reverified.
pub(crate) fn signer(
    target: &Target,
    config: &Policy,
    root: &Root,
    raw: &[u8],
    signature: &[u8],
) -> Result<(format::Trust, Vec<u8>)> {
    let manifest = ReleaseManifest::parse(raw)?;
    let id = format::digest(raw);
    let identity = &manifest.identity;
    let path = pin_path(target, &id);
    let proof = root_proof(root, Path::new(AUTHORITY_FILE))?;
    let input_authority =
        proof.verify_archived(policy(config)?, &identity.distro, &identity.channel)?;
    let (mut known, mut current) = saved_authority(target, identity, config)?;
    let offline = root.location()?.canonicalize()?
        == target
            .state_dir
            .join("releases")
            .join(&id)
            .canonicalize()
            .unwrap_or_default();
    if !offline && input_authority.delegation().version > known.floor.authority_version {
        accept(target, identity, config, proof.clone())?;
        (known, current) = saved_authority(target, identity, config)?;
    }
    ensure!(
        input_authority.delegation().root_version <= known.floor.root_version
            && input_authority.delegation().index_epoch <= known.floor.index_epoch
            && input_authority.delegation().version <= known.floor.authority_version
            && (input_authority.delegation().version != known.floor.authority_version
                || input_authority.digest() == known.floor.authority_digest),
        "cached/media authority differs from latest accepted authority"
    );
    let signer = if offline && path.try_exists()? {
        let pin: Pin = load(&path, 4 * 1024 * 1024)?;
        ensure!(
            pin.schema_version == 2
                && pin.id == id
                && pin.source == policy(config)?.feed_id(identity)?
                && pin.manifest_json.as_bytes() == raw
                && pin.signature == signature,
            "cached pin/manifest mismatch"
        );
        admission(target, &pin, identity, config, true)?;
        verify_proof(&pin, identity, config)?.3
    } else {
        current.require_current(now()?)?;
        current.verify(SigningRole::Release, raw, signature)?
    };
    ensure!(
        !known.revoked.contains(&signer.id()?),
        "release signer has been revoked"
    );
    Ok((signer, serde_json::to_vec(&known.proof)?))
}
pub(super) fn check(
    sys: &System,
    identity: &Identity,
    config: &Policy,
    source: &str,
    recipe: &Path,
) -> Result<UpdateStatus> {
    let mut state = record(&sys.target, source, identity, config)?;
    state.view.phase = "checking".into();
    state.view.last_error = None;
    state.view.available = None;
    persist(&sys.target, &mut state)?;
    let result = (|| -> Result<()> {
        let base = &config.source.as_ref().unwrap().index;
        let bytes = metadata(
            &sys.target,
            recipe,
            config,
            &url(base, AUTHORITY_FILE)?,
            "",
            MAX_AUTHORITY as u64,
        )?;
        let proof = AuthorityProof::parse(&bytes)?;
        let authority = accept(&sys.target, identity, config, proof.clone())?;
        let mut floor = floor(&sys.target, source)?;
        floor.authority(&authority)?;
        let path = floor_path(&sys.target, source)?;
        root_dir(path.parent().unwrap())?;
        save(&path, &floor)?;
        let raw = metadata(
            &sys.target,
            recipe,
            config,
            base,
            "",
            format::MAX_MANIFEST as u64,
        )?;
        let sig = metadata(
            &sys.target,
            recipe,
            config,
            &url(
                base,
                &format!("index-signatures/{}.sig", format::digest(&raw)),
            )?,
            "",
            64,
        )?;
        let at = now()?;
        let index = ReleaseIndexV2::authenticate(&raw, &sig, &authority, policy(config)?, at)?;
        floor.index(&index, &raw, at)?;
        save(&path, &floor)?;
        state.view.available = None;
        let selected = index.releases.iter().find(|e| {
            let i = &e.release.identity;
            i.distro == identity.distro
                && i.architecture == identity.architecture
                && i.init == identity.init
                && i.libc == identity.libc
                && i.flavour == identity.flavour
        });
        let Some(selected) = selected else {
            state.view.phase = "up_to_date".into();
            return Ok(());
        };
        let e = &selected.release;
        let manifest_bytes = metadata(
            &sys.target,
            recipe,
            config,
            &url(base, &e.manifest.path)?,
            &e.manifest.sha256,
            e.manifest.size,
        )?;
        let signature = metadata(
            &sys.target,
            recipe,
            config,
            &url(base, &e.signature.path)?,
            &e.signature.sha256,
            e.signature.size,
        )?;
        authority.verify(SigningRole::Release, &manifest_bytes, &signature)?;
        let manifest = ReleaseManifest::parse(&manifest_bytes)?;
        manifest.matches(identity)?;
        manifest
            .compatibility
            .check(crate::release::RECAB_VERSION)?;
        selected.for_manifest(&manifest, policy(config)?)?;
        floor.release(manifest.identity.sequence, &e.manifest.sha256)?;
        save(&path, &floor)?;
        if manifest.identity.sequence <= identity.sequence {
            state.view.phase = "up_to_date".into();
            return Ok(());
        }
        let pin = Pin {
            schema_version: 2,
            source: source.into(),
            id: e.manifest.sha256.clone(),
            manifest_json: String::from_utf8(manifest_bytes)?,
            signature,
            base_url: base.clone(),
            production: Some(Proof {
                authority: proof,
                index: String::from_utf8(raw)?,
                signature: sig,
                accepted_at: at,
                completed: false,
            }),
        };
        root_dir(pin_path(&sys.target, &pin.id).parent().unwrap())?;
        save(&pin_path(&sys.target, &pin.id), &pin)?;
        state.view.available = Some(summarize(&pin.id, &manifest));
        state.view.phase = "available".into();
        Ok(())
    })();
    if let Err(e) = &result {
        state.view.phase = "check_failed".into();
        state.view.last_error = Some(format!("{e:#}").chars().take(2048).collect());
    }
    persist(&sys.target, &mut state)?;
    result?;
    view(sys)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema_version: u32,
    layout: String,
    verified_parts: usize,
}
fn assembly(target: &Target, id: &str, artifact: &Artifact) -> PathBuf {
    directory(target)
        .join("assembly")
        .join(id)
        .join(format::digest(artifact.path.as_bytes()))
}
fn prefix(file: &mut fs::File, parts: &[protocol::TransportPart]) -> Result<usize> {
    for (n, part) in parts.iter().enumerate() {
        if file.metadata()?.len() < part.offset + part.size {
            return Ok(n);
        }
        file.seek(SeekFrom::Start(part.offset))?;
        if crate::generation::hash_reader(&mut (&mut *file).take(part.size))? != part.sha256 {
            return Ok(n);
        }
    }
    Ok(parts.len())
}
fn space(path: &Path, needed: u64) -> Result<()> {
    let file = fs::File::open(path)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::fstatvfs(file.as_raw_fd(), &mut st) } == 0,
        "cannot measure update workspace capacity"
    );
    let free = st.f_bavail.saturating_mul(st.f_frsize);
    ensure!(
        free >= needed,
        "update workspace {} needs {needed} free bytes, has {free}",
        path.display()
    );
    Ok(())
}
pub(super) fn download(
    target: &Target,
    recipe: &Path,
    config: &Policy,
    pin: &Pin,
    manifest: &ReleaseManifest,
    transfer: &Path,
    state: &mut Record,
) -> Result<()> {
    let entry = admission(target, pin, &manifest.identity, config, false)?;
    let mut needed = 0;
    let mut largest = 0;
    for artifact in manifest.artifacts.all() {
        if matches_file(&transfer.join(&artifact.path), artifact)? {
            continue;
        }
        let parts = &entry.parts[&artifact.path];
        largest = largest.max(parts.iter().map(|p| p.size).max().unwrap());
        let path = assembly(target, &pin.id, artifact);
        let verified = if path.try_exists()? {
            prefix(&mut levitate_recipe::fsx::open_regular(&path)?, parts)?
        } else {
            0
        };
        let bytes = parts.iter().take(verified).map(|p| p.size).sum::<u64>();
        needed += artifact.size - bytes;
    }
    let overhead = largest.saturating_mul(3).saturating_add(64 * 1024 * 1024);
    // The existing full validator freezes a private copy before publishing its
    // cache. Reserve that complete set as well as unfinished transport bytes.
    let frozen = manifest.artifacts.all().iter().map(|a| a.size).sum::<u64>();
    space(
        &target.state_dir,
        needed.saturating_add(frozen).saturating_add(overhead),
    )?;
    space(&std::env::temp_dir(), overhead)?;
    target_root::write_durable(
        &transfer.join(AUTHORITY_FILE),
        &serde_json::to_vec(&pin.production.as_ref().unwrap().authority)?,
        Some(0o600),
    )?;
    for artifact in manifest.artifacts.all() {
        let destination = transfer.join(&artifact.path);
        let parts = &entry.parts[&artifact.path];
        if matches_file(&destination, artifact)? {
            state.view.download.as_mut().unwrap().reused_files += 1;
        } else {
            let path = assembly(target, &pin.id, artifact);
            root_dir(path.parent().unwrap())?;
            let checkpoint = path.with_extension("json");
            let layout = format::digest(&serde_json::to_vec(parts)?);
            let mut out = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)?;
            ensure!(
                out.metadata()?.is_file() && out.metadata()?.nlink() == 1,
                "unsafe update assembly inode"
            );
            let resumed = out.metadata()?.len() > 0 || checkpoint.try_exists()?;
            let count = prefix(&mut out, parts)?;
            let verified_bytes = parts.iter().take(count).map(|p| p.size).sum::<u64>();
            out.set_len(verified_bytes)?;
            out.sync_all()?;
            let p = state.view.download.as_mut().unwrap();
            if resumed {
                p.accounting_complete = false;
            }
            p.reused_parts += count as u32;
            p.verified_parts += count as u32;
            save(
                &checkpoint,
                &Checkpoint {
                    schema_version: 1,
                    layout: layout.clone(),
                    verified_parts: count,
                },
            )?;
            for (n, part) in parts.iter().enumerate().skip(count) {
                let mut object =
                    acquire(target, recipe, config, &part.url, &part.sha256, part.size)?;
                ensure!(
                    object.file.metadata()?.len() == part.size
                        && crate::generation::hash_reader(&mut object.file)? == part.sha256,
                    "acquired part digest differs"
                );
                object.file.seek(SeekFrom::Start(0))?;
                out.seek(SeekFrom::Start(part.offset))?;
                ensure!(
                    std::io::copy(&mut (&mut object.file).take(part.size), &mut out)? == part.size,
                    "part assembly was truncated"
                );
                out.sync_all()?;
                save(
                    &checkpoint,
                    &Checkpoint {
                        schema_version: 1,
                        layout: layout.clone(),
                        verified_parts: n + 1,
                    },
                )?;
                let p = state.view.download.as_mut().unwrap();
                p.producer_write_bytes += object.write_bytes;
                p.producer_cancelled_write_bytes += object.cancelled;
                p.assembly_write_bytes += part.size;
                p.verified_parts += 1;
                persist(target, state)?;
            }
            out.seek(SeekFrom::Start(0))?;
            ensure!(
                out.metadata()?.len() == artifact.size
                    && crate::generation::hash_reader(&mut out)? == artifact.sha256,
                "assembled artifact differs from release manifest"
            );
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::rename(&path, &destination)?;
            target_root::sync_dir(destination.parent().unwrap())?;
            fs::remove_file(checkpoint)?;
            target_root::sync_dir(path.parent().unwrap())?;
        }
        let p = state.view.download.as_mut().unwrap();
        p.verified_files += 1;
        p.verified_bytes += artifact.size;
        persist(target, state)?;
    }
    Ok(())
}
pub(super) fn complete(target: &Target, pin: &Pin) -> Result<()> {
    let mut verified: Pin = load(&pin_path(target, &pin.id), 4 * 1024 * 1024)?;
    verified
        .production
        .as_mut()
        .context("production receipt missing")?
        .completed = true;
    save(&pin_path(target, &pin.id), &verified)
}

pub(super) fn cache_metadata(
    target: &Target,
    config: &Policy,
    pin: &Pin,
    cache: &Path,
) -> Result<()> {
    let manifest = ReleaseManifest::parse(pin.manifest_json.as_bytes())?;
    admission(target, pin, &manifest.identity, config, true)?;
    ensure!(
        fs::read(cache.join(format::MANIFEST))? == pin.manifest_json.as_bytes(),
        "cached manifest identity differs from pin"
    );
    let (known, _) = saved_authority(target, &manifest.identity, config)?;
    target_root::write_durable(
        &cache.join(AUTHORITY_FILE),
        &serde_json::to_vec(&known.proof)?,
        Some(0o600),
    )?;
    target_root::write_durable(&cache.join(format::SIGNATURE), &pin.signature, Some(0o600))?;
    Ok(())
}
