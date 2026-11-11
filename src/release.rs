//! Release verification/import. No candidate or boot entry is changed here.
//! Every artifact is hashed while copied into a private frozen input set. All
//! subsequent parsing, EROFS reads and consumers use that set, never the source.
use crate::{error::refuse, filesystem::Root, stage::Mounted, target::Target};

pub const RECAB_VERSION: &str = env!("CARGO_PKG_VERSION");
use anyhow::{ensure, Context, Result};
use distro_contract::release::{
    self as contract, Architecture, Artifact, Identity, Policy, ReleaseManifest, SystemInventory,
    Trust,
};
use distro_profile::{os_release_fact, os_release_value};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportedRelease {
    pub id: String,
    pub manifest: ReleaseManifest,
    pub authentication: String,
    pub cache: PathBuf,
    pub payload_bytes_copied: u64,
}
/// A verified set whose named inputs are kept on a private read-only bind mount.
/// Retain this guard until the last consumer has finished using its paths.
pub struct VerifiedRelease {
    manifest: ReleaseManifest,
    bytes: Vec<u8>,
    authentication: String,
    copied: u64,
    readonly: Option<Mounted>,
    directory: Option<tempfile::TempDir>,
}
impl Drop for VerifiedRelease {
    fn drop(&mut self) {
        drop(self.readonly.take());
    }
}
impl VerifiedRelease {
    pub fn manifest(&self) -> &ReleaseManifest {
        &self.manifest
    }
    pub fn rootfs(&self) -> PathBuf {
        self.member(&self.manifest.artifacts.rootfs)
    }
    pub fn kernel(&self) -> PathBuf {
        self.member(&self.manifest.artifacts.kernel)
    }
    pub fn initramfs(&self) -> PathBuf {
        self.member(&self.manifest.artifacts.initramfs)
    }
    pub fn inventory_path(&self) -> PathBuf {
        self.member(&self.manifest.artifacts.inventory)
    }
    pub fn signature_path(&self) -> Option<PathBuf> {
        let path = self
            .directory
            .as_ref()
            .unwrap()
            .path()
            .join(contract::SIGNATURE);
        path.is_file().then_some(path)
    }
    pub fn id(&self) -> String {
        contract::digest(&self.bytes)
    }
    pub fn manifest_path(&self) -> PathBuf {
        self.directory
            .as_ref()
            .unwrap()
            .path()
            .join(contract::MANIFEST)
    }
    fn member(&self, a: &Artifact) -> PathBuf {
        self.directory.as_ref().unwrap().path().join(&a.path)
    }
}
fn read(root: &Root, path: &Path, bound: u64) -> Result<Vec<u8>> {
    let file = root.entry(path, false)?.open()?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= bound,
        "release member exceeds bound or is not a regular file"
    );
    let mut bytes = Vec::new();
    file.take(bound + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= bound, "release member exceeds bound");
    Ok(bytes)
}
fn read_policy(root: &Root) -> Result<Policy> {
    let file = root
        .entry(Path::new(contract::SOURCE_CONFIG), false)?
        .open()
        .context("release policy must be a regular slot-owned file")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "release policy must be root-owned and not group/world-writable"
    );
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "release policy exceeds bound");
    let policy: Policy = serde_json::from_slice(&bytes)?;
    policy.validate()?;
    Ok(policy)
}
fn copy(root: &Root, artifact: &Artifact, destination: &Path) -> Result<()> {
    let mut source = root.entry(Path::new(&artifact.path), false)?.open()?;
    ensure!(
        source.metadata()?.len() == artifact.size,
        "release artifact size mismatch: {}",
        artifact.path
    );
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .context("release size overflow")?;
        ensure!(
            size <= artifact.size,
            "release artifact grew while reading: {}",
            artifact.path
        );
        hash.update(&buffer[..n]);
        output.write_all(&buffer[..n])?;
    }
    ensure!(
        size == artifact.size && format!("{:x}", hash.finalize()) == artifact.sha256,
        "release artifact digest mismatch: {}",
        artifact.path
    );
    output.sync_all()?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o400))?;
    Ok(())
}
/// Verify a complete local/media bundle. `trust` must come from installed
/// configuration, never the incoming manifest. Unsigned local use is explicit.
/// This is also the builder's final pre-publication validator.
pub fn verify_bundle(
    manifest: &Path,
    trust: Option<&Trust>,
    trusted_local: bool,
    expected: Option<&Identity>,
) -> Result<VerifiedRelease> {
    verify_in(
        manifest,
        Verification {
            trust,
            trusted_local,
            expected,
            allow_flavour: false,
            cache_parent: None,
            produced: None,
            delegated: None,
        },
    )
}
struct Verification<'a> {
    trust: Option<&'a Trust>,
    trusted_local: bool,
    expected: Option<&'a Identity>,
    allow_flavour: bool,
    cache_parent: Option<&'a Path>,
    produced: Option<(&'a Path, &'a contract::RootfsProvenance)>,
    delegated: Option<(Option<&'a Target>, &'a Policy)>,
}

fn verify_in(manifest: &Path, verification: Verification<'_>) -> Result<VerifiedRelease> {
    let Verification {
        trust,
        trusted_local,
        expected,
        allow_flavour,
        cache_parent,
        produced,
        delegated,
    } = verification;
    crate::system::require_root("verify release")?;
    let parent = manifest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent)?;
    let input = Root::open(&parent)?;
    let bytes = read(
        &input,
        Path::new(manifest.file_name().context("manifest filename missing")?),
        contract::MAX_MANIFEST as u64,
    )?;
    let delegation = if let Some((target, policy)) = delegated {
        let mut proof = read(
            &input,
            Path::new(contract::production::AUTHORITY_FILE),
            contract::production::MAX_AUTHORITY as u64,
        )?;
        let signature = read(&input, Path::new(contract::SIGNATURE), 64)?;
        let signer = if let Some(target) = target {
            let authenticated =
                crate::update::production::signer(target, policy, &input, &bytes, &signature)?;
            proof = authenticated.1;
            authenticated.0
        } else {
            let identity = ReleaseManifest::parse(&bytes)?.identity;
            contract::production::AuthorityProof::parse(&proof)?
                .verify(
                    policy.production.as_ref().unwrap(),
                    &identity.distro,
                    &identity.channel,
                    crate::update::production::now()?,
                )?
                .verify(
                    contract::production::SigningRole::Release,
                    &bytes,
                    &signature,
                )?
        };
        Some((signer, proof))
    } else {
        None
    };
    let trust = delegation.as_ref().map(|v| &v.0).or(trust);
    let signature = if let Some(trust) = trust {
        let signature = read(&input, Path::new(contract::SIGNATURE), 64)?;
        contract::verify_signature(&bytes, &signature, trust)?;
        Some(signature)
    } else {
        ensure!(
            trusted_local,
            "no distribution release key configured; explicitly trust local media with --trusted-local, or configure signed releases"
        );
        None
    };
    let parsed = ReleaseManifest::parse(&bytes)?;
    parsed.compatibility.check(RECAB_VERSION)?;
    if let Some(expected) = expected {
        let mut expected = expected.clone();
        if allow_flavour {
            expected.flavour = parsed.identity.flavour;
        }
        parsed.matches(&expected)?;
    }
    crate::filesystem::private_mounts()?;
    let target = Target::running()?;
    let directory = match cache_parent {
        Some(parent) => tempfile::Builder::new()
            .prefix(".import-")
            .tempdir_in(parent)?,
        None => tempfile::Builder::new()
            .prefix("recab-release-")
            .tempdir()?,
    };
    let mut copied = 0;
    for artifact in parsed.artifacts.all() {
        copy(&input, artifact, &directory.path().join(&artifact.path))?;
        copied += artifact.size;
    }
    fs::write(directory.path().join(contract::MANIFEST), &bytes)?;
    if let Some(signature) = signature {
        fs::write(directory.path().join(contract::SIGNATURE), signature)?;
    }
    if let Some((_, proof)) = &delegation {
        fs::write(
            directory.path().join(contract::production::AUTHORITY_FILE),
            proof,
        )?;
    }
    crate::filesystem::flush(&Root::open(directory.path())?)?;
    // Freeze names as well as bytes. The path-only mount/recstrap interfaces
    // cannot reopen a different mutable source after validation.
    let bind = Mounted::new(&target, &["--bind"], directory.path(), directory.path())?;
    target_root::run(
        std::process::Command::new(&target.programs.mount)
            .args(["-o", "remount,bind,ro"])
            .arg(directory.path()),
    )?;
    let verified = VerifiedRelease {
        manifest: parsed,
        bytes,
        authentication: if delegation.is_some() {
            "delegated"
        } else if trust.is_some() {
            "distribution_signature"
        } else {
            "trusted_local"
        }
        .into(),
        copied,
        readonly: Some(bind),
        directory: Some(directory),
    };
    if let Some((root, proof)) = produced {
        ensure!(
            proof.schema_version == 1
                && proof.rootfs_sha256 == verified.manifest.artifacts.rootfs.sha256
                && contract::digest(&serde_json::to_vec(&proof.inventory)?)
                    == verified.manifest.artifacts.inventory.sha256,
            "image-writer provenance differs from frozen release set"
        );
        validate_root(root, &verified, true)?;
    } else {
        validate_set(&target, &verified)?;
    }
    Ok(verified)
}
/// Producer-only validation requires the actual image-writer provenance. The
/// consumer always reads the real mounted EROFS filesystem instead.
pub fn verify_produced_bundle(
    root: &Path,
    manifest: &Path,
    trust: Option<&Trust>,
    trusted_local: bool,
    proof: &contract::RootfsProvenance,
) -> Result<VerifiedRelease> {
    let policy = read_policy(&Root::open(root)?)?;
    verify_in(
        manifest,
        Verification {
            trust,
            trusted_local,
            expected: None,
            allow_flavour: false,
            cache_parent: None,
            produced: Some((root, proof)),
            delegated: policy.production.as_ref().map(|_| (None, &policy)),
        },
    )
}
fn validate_set(target: &Target, verified: &VerifiedRelease) -> Result<()> {
    let point = tempfile::Builder::new()
        .prefix("recab-release-root-")
        .tempdir()?;
    let mount = Mounted::new(
        target,
        &["-t", "erofs", "-o", "ro,loop,nosuid,nodev,noexec"],
        &verified.rootfs(),
        point.path(),
    )?;
    let result = validate_root(point.path(), verified, false);
    mount.unmount()?;
    result
}
fn validate_root(path: &Path, verified: &VerifiedRelease, produced: bool) -> Result<()> {
    let manifest = verified.manifest();
    let inventory =
        SystemInventory::parse(&fs::read(verified.member(&manifest.artifacts.inventory))?)?;
    let root = Root::open(path)?;
    ensure!(
        root.metadata(Path::new(crate::merge::BASELINE))?.is_none(),
        "release images must not contain an installed personalized baseline"
    );
    let identity = root
        .optional_text(Path::new("etc/os-release"))?
        .context("release has no os-release")?;
    ensure!(
        os_release_value(&identity, "ID").as_deref() == Some(manifest.identity.distro.as_str())
            && os_release_value(&identity, "VERSION_ID").as_deref()
                == Some(manifest.identity.version.as_str())
            && os_release_fact::<distro_profile::InitSystem>(&identity, "REC_INIT")?
                == manifest.identity.init
            && os_release_fact::<distro_profile::Libc>(&identity, "REC_LIBC")?
                == manifest.identity.libc
            && os_release_fact::<distro_profile::Flavour>(&identity, "REC_FLAVOUR")?
                == manifest.identity.flavour,
        "release rootfs identity differs from manifest"
    );
    let policy = read_policy(&root)?;
    ensure!(
        policy.production.is_none() || verified.authentication == "delegated",
        "delegated release cannot use unsigned/local or legacy-key authentication"
    );
    ensure!(
        policy.channel == manifest.identity.channel
            && policy.sequence == manifest.identity.sequence,
        "release baseline configuration differs from manifest"
    );
    let observed = if produced {
        contract::SystemInventory::from_target(levitate_recipe::target::inspect(path)?)?.entries
    } else {
        let actual = crate::Inventory::capture_candidate(path)?;
        let mut observed = BTreeMap::new();
        for node in &actual.entries {
            let path = std::str::from_utf8(&node.path)?;
            let mut entry = crate::change::logical(node)?;
            if !contract::inventory_path(path, &entry.kind) {
                continue;
            }
            if !inventory
                .entries
                .get(path)
                .is_some_and(|e| e.xattrs.contains_key("security.selinux"))
            {
                entry.xattrs.remove("security.selinux");
            }
            observed.insert(path.to_string(), entry);
        }
        observed
    };
    ensure!(
        observed == inventory.entries,
        "release rootfs does not match its intended system inventory"
    );
    crate::candidate::release(path, &verified.kernel(), &verified.initramfs(), manifest)?;
    Ok(())
}
pub(crate) fn installed(target: &Target) -> Result<(Identity, Policy)> {
    let root = Root::open(&target.root)?;
    let os = root
        .optional_text(Path::new("etc/os-release"))?
        .context("installed identity missing")?;
    let policy = read_policy(&root).context(
        "installed release policy missing/unsafe; rebuild or explicitly adopt a compatible image",
    )?;
    let architecture = match std::env::consts::ARCH {
        "x86_64" => Architecture::X86_64,
        "aarch64" => Architecture::Aarch64,
        _ => return Err(refuse("unsupported release architecture")),
    };
    let identity = Identity {
        distro: os_release_value(&os, "ID").context("installed distro missing")?,
        architecture,
        init: os_release_fact(&os, "REC_INIT")?,
        libc: os_release_fact(&os, "REC_LIBC")?,
        flavour: os_release_fact(&os, "REC_FLAVOUR")?,
        channel: policy.channel.clone(),
        sequence: policy.sequence,
        version: os_release_value(&os, "VERSION_ID").unwrap_or_default(),
    };
    Ok((identity, policy))
}
pub(crate) fn for_target(
    target: &Target,
    manifest: &Path,
    trusted_local: bool,
    allow_flavour: bool,
) -> Result<VerifiedRelease> {
    let (identity, policy) = installed(target)?;
    verify_in(
        manifest,
        Verification {
            trust: policy.trust.as_ref(),
            trusted_local,
            expected: Some(&identity),
            allow_flavour,
            cache_parent: None,
            produced: None,
            delegated: policy.production.as_ref().map(|_| (Some(target), &policy)),
        },
    )
}
/// Import a verified release into a content-addressed cache. The installed
/// preparation/ledger and boot selection are left byte-for-byte unchanged.
pub fn import(
    manifest: &Path,
    trusted_local: bool,
    root: Option<&Path>,
) -> Result<ImportedRelease> {
    import_inner(manifest, trusted_local, root)
        .map_err(|e| crate::error::or_class(e, crate::ErrorClass::StateOrDetection))
}
fn import_inner(
    manifest: &Path,
    trusted_local: bool,
    root: Option<&Path>,
) -> Result<ImportedRelease> {
    let target = match root {
        Some(root) => Target::installed(root)?,
        None => Target::running()?,
    };
    let _lock = crate::system::lock(&target, "release import")?;
    import_locked(&target, manifest, trusted_local)
}
pub(crate) fn import_locked(
    target: &Target,
    manifest: &Path,
    trusted_local: bool,
) -> Result<ImportedRelease> {
    import_locked_options(target, manifest, trusted_local, false)
}
pub(crate) fn import_locked_options(
    target: &Target,
    manifest: &Path,
    trusted_local: bool,
    allow_flavour: bool,
) -> Result<ImportedRelease> {
    let parent = target.state_dir.join("releases");
    fs::create_dir_all(&parent)?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    let (identity, policy) = installed(target)?;
    let mut verified = verify_in(
        manifest,
        Verification {
            trust: policy.trust.as_ref(),
            trusted_local,
            expected: Some(&identity),
            allow_flavour,
            cache_parent: Some(&parent),
            produced: None,
            delegated: policy.production.as_ref().map(|_| (Some(target), &policy)),
        },
    )?;
    let cache = parent.join(verified.id());
    if cache.exists() {
        let existing = Root::open(&cache)?;
        ensure!(
            read(
                &existing,
                Path::new(contract::MANIFEST),
                contract::MAX_MANIFEST as u64
            )? == verified.bytes,
            "cached release identity collision"
        );
        if let Some(trust) = &policy.trust {
            contract::verify_signature(
                &verified.bytes,
                &read(&existing, Path::new(contract::SIGNATURE), 64)?,
                trust,
            )?;
        }
        for artifact in verified.manifest.artifacts.all() {
            let mut file = existing.entry(Path::new(&artifact.path), false)?.open()?;
            ensure!(
                file.metadata()?.len() == artifact.size
                    && crate::generation::hash_reader(&mut file)? == artifact.sha256,
                "cached release artifact is altered"
            );
        }
    } else {
        // Move the very files just verified; no second full-rootfs cache copy.
        verified.readonly.take().unwrap().unmount()?;
        fs::rename(verified.directory.as_ref().unwrap().path(), &cache)?;
        target_root::sync_dir(&parent)?;
    }
    Ok(ImportedRelease {
        id: verified.id(),
        manifest: verified.manifest.clone(),
        authentication: verified.authentication.clone(),
        cache,
        payload_bytes_copied: verified.copied,
    })
}
