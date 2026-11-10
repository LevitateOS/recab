//! Read-only checks of the actual root, kernel and installed initramfs.
use crate::Slot;
use crate::error::refuse;
use crate::filesystem::Root;
use anyhow::{Context, Result};
use distro_profile::{InitSystem, os_release_fact, os_release_value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) struct Checked {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub kernel_digest: String,
    pub initrd_digest: String,
    pub release: String,
    pub architecture: String,
    pub init: InitSystem,
}
fn text(root: &Root, p: &str) -> Result<String> {
    root.optional_text(Path::new(p))?
        .ok_or_else(|| refuse(format!("candidate is missing {p}")))
}
fn relative(p: &str) -> Result<&Path> {
    let path = Path::new(p);
    if p.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(refuse("unsafe system metadata path"));
    }
    Ok(path)
}
pub(crate) fn kernel(path: &Path) -> Result<(u16, String)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    if size < 4096 || size > 512 * 1024 * 1024 {
        return Err(refuse("invalid kernel image size"));
    }
    let mut header = [0u8; 4096];
    file.read_exact(&mut header)?;
    if &header[..2] != b"MZ" {
        return Err(refuse("kernel lacks the UEFI stub"));
    }
    let pe = u32::from_le_bytes(header[0x3c..0x40].try_into()?) as u64;
    file.seek(SeekFrom::Start(pe))?;
    let mut coff = [0u8; 94];
    file.read_exact(&mut coff)?;
    if &coff[..4] != b"PE\0\0" || u16::from_le_bytes(coff[92..94].try_into()?) != 10 {
        return Err(refuse("kernel is not a UEFI application"));
    }
    let pe_machine = u16::from_le_bytes(coff[4..6].try_into()?);
    let (machine, release) = if &header[0x202..0x206] == b"HdrS"
        && u16::from_le_bytes(header[0x236..0x238].try_into()?) & 1 != 0
    {
        let offset = u16::from_le_bytes(header[0x20e..0x210].try_into()?) as u64 + 0x200;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = [0u8; 256];
        file.read_exact(&mut bytes)?;
        (
            62,
            bytes
                .split(|b| *b == 0 || b.is_ascii_whitespace())
                .next()
                .unwrap()
                .to_vec(),
        )
    } else if &header[0x38..0x3c] == b"ARM\x64" {
        file.seek(SeekFrom::Start(0))?;
        let mut carry = Vec::new();
        let mut found = None;
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            carry.extend_from_slice(&buffer[..n]);
            if let Some(at) = carry.windows(14).position(|w| w == b"Linux version ") {
                let after = &carry[at + 14..];
                if let Some(end) = after
                    .iter()
                    .position(|b| b.is_ascii_whitespace() || *b == 0)
                {
                    found = Some(after[..end].to_vec());
                    break;
                }
            }
            if carry.len() > 256 {
                carry.drain(..carry.len() - 256);
            }
        }
        (183, found.context("ARM64 kernel has no release banner")?)
    } else {
        return Err(refuse(
            "kernel is not a supported x86_64 bzImage or ARM64 Image",
        ));
    };
    if (machine == 62 && pe_machine != 0x8664) || (machine == 183 && pe_machine != 0xaa64) {
        return Err(refuse("kernel UEFI architecture is inconsistent"));
    }
    let release = String::from_utf8(release)?;
    if release.is_empty()
        || release.len() > 64
        || !release
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+_-".contains(&b))
    {
        return Err(refuse("invalid kernel release"));
    }
    Ok((machine, release))
}

pub(crate) fn check(
    root: &Path,
    source: &Path,
    slot: Slot,
    uuid: &str,
    state_uuid: &str,
    esp_uuid: &str,
    kernel_path: &Path,
    initrd_path: &Path,
    known_kernel: &str,
    change_flavour: bool,
) -> Result<Checked> {
    let opened = Root::open(root)?;
    let old = Root::open(source)?;
    let profile = text(&opened, "etc/os-release")?;
    let active = text(&old, "etc/os-release")?;
    for field in ["ID", "REC_INIT", "REC_LIBC"] {
        if os_release_value(&profile, field).is_none()
            || os_release_value(&profile, field) != os_release_value(&active, field)
        {
            return Err(refuse(format!(
                "candidate {field} does not match the installed system"
            )));
        }
    }
    if !change_flavour
        && os_release_value(&profile, "REC_FLAVOUR") != os_release_value(&active, "REC_FLAVOUR")
    {
        return Err(refuse("candidate flavour differs from the active system"));
    }
    let init: InitSystem = os_release_fact(&profile, "REC_INIT")?;
    let libc = os_release_value(&profile, "REC_LIBC").context("candidate libc is missing")?;
    if !matches!(libc.as_str(), "glibc" | "musl") {
        return Err(refuse("unsupported candidate libc"));
    }
    let (machine, release) = kernel(kernel_path)?;
    crate::elf::executable(&opened, Path::new("sbin/init"), machine, &libc)?;
    crate::elf::executable(
        &opened,
        Path::new(match init {
            InitSystem::Systemd => "usr/lib/systemd/systemd",
            InitSystem::Openrc => "sbin/openrc-init",
        }),
        machine,
        &libc,
    )?;
    crate::elf::executable(&opened, Path::new("bin/sh"), machine, &libc)?;
    for dir in [
        "etc",
        "usr",
        "usr/lib/recab",
        "boot",
        "var",
        "root",
        "run",
        "tmp",
        "dev",
        "proc",
        "sys",
    ] {
        if !opened
            .metadata(Path::new(dir))?
            .is_some_and(|m| m.is_dir() && m.uid() == 0)
        {
            return Err(refuse(format!(
                "candidate needs real root-owned directory /{dir}"
            )));
        }
    }
    if opened.metadata(Path::new("tmp"))?.unwrap().mode() & 0o1777 != 0o1777 {
        return Err(refuse("candidate /tmp needs mode 1777"));
    }
    let home = opened
        .metadata(Path::new("home"))?
        .context("candidate /home is missing")?;
    if home.file_type().is_symlink() {
        let link = std::fs::read_link(opened.entry(Path::new("home"), false)?.path())?;
        if link != Path::new("/var/home") && link != Path::new("var/home") {
            return Err(refuse("candidate homes must use shared /var/home"));
        }
    } else if !home.is_dir() || home.uid() != 0 {
        return Err(refuse(
            "candidate /home must be a root-owned directory or shared-state link",
        ));
    }
    let machine_id = text(&opened, "etc/machine-id")?;
    if machine_id.trim().len() != 32
        || !machine_id.trim().bytes().all(|b| b.is_ascii_hexdigit())
        || machine_id.trim().bytes().all(|b| b == b'0')
    {
        return Err(refuse(
            "machine identity must be seeded before finalization",
        ));
    }
    runtime(&opened, machine, &libc)?;
    // The retained fallback must also understand the state this writer saves.
    // Updating only the candidate's reader would strand rollback on old code.
    runtime(&old, machine, &libc).context("retained fallback needs a compatible recab runtime")?;
    accounts(&opened)?;
    mounts(&opened, slot, uuid, state_uuid, esp_uuid)?;
    ssh(&opened, init)?;
    receipts(&opened)?;
    let archive = recinit::installed::inspect_installed(initrd_path)?;
    if archive.kernel_release != release
        || archive.machines.is_empty()
        || archive.machines.iter().any(|m| *m != machine)
    {
        return Err(refuse(
            "initramfs architecture/release does not match kernel",
        ));
    }
    let modules_path = format!("usr/lib/modules/{release}");
    let module_entry = opened.entry(relative(&modules_path)?, false)?;
    if !module_entry.metadata()?.is_some_and(|m| m.is_dir()) {
        return Err(refuse("candidate has no matching kernel module tree"));
    }
    let modules_digest = recinit::installed::modules_digest(&module_entry.path())?;
    let kernel_digest = crate::generation::hash_file(kernel_path)?;
    let initrd_digest = crate::generation::hash_file(initrd_path)?;
    if let Some(binding) = opened.optional_text(Path::new("usr/lib/recab/kernel.json"))? {
        let binding: serde_json::Value = serde_json::from_str(&binding)?;
        if binding["version"] != 1
            || binding["release"] != release
            || binding["kernel_digest"] != kernel_digest
            || binding["modules_digest"] != modules_digest
        {
            return Err(refuse(
                "kernel does not match the producer's module binding",
            ));
        }
    } else {
        let source_modules = old.entry(relative(&modules_path)?, false)?;
        if kernel_digest != known_kernel
            || recinit::installed::modules_digest(&source_modules.path())? != modules_digest
        {
            return Err(refuse(
                "changed kernel/modules require the producer's kernel binding",
            ));
        }
    }
    modules(&opened, &modules_path, &release, machine, &archive.files)?;
    Ok(Checked {
        kernel: kernel_path.into(),
        initrd: initrd_path.into(),
        kernel_digest,
        initrd_digest,
        release,
        architecture: if machine == 62 { "x86_64" } else { "aarch64" }.into(),
        init,
    })
}

fn runtime(root: &Root, machine: u16, libc: &str) -> Result<()> {
    let runtime: serde_json::Value =
        serde_json::from_str(&text(root, "usr/lib/recab/runtime.json")?)?;
    if runtime["version"] != 1
        || runtime["state_major"] != 2
        || runtime["state_minor_min"].as_u64().is_none_or(|n| n > 3)
        || runtime["state_minor_max"].as_u64().is_none_or(|n| n < 3)
        || runtime["boot_protocol"] != 3
    {
        return Err(refuse(
            "candidate recab runtime cannot read the finalized state/boot protocol",
        ));
    }
    let binary = relative(
        runtime["binary"]
            .as_str()
            .context("runtime binary path missing")?,
    )?;
    let mut executable = root
        .open_resolved(binary)?
        .context("recab runtime binary missing")?;
    if runtime["binary_digest"] != crate::generation::hash_reader(&mut executable)? {
        return Err(refuse(
            "recab runtime differs from its producer compatibility record",
        ));
    }
    crate::elf::executable(root, binary, machine, libc)?;
    Ok(())
}

fn mounts(root: &Root, slot: Slot, uuid: &str, state: &str, esp: &str) -> Result<()> {
    let immutable = text(root, recinit::installed::IMMUTABLE_CONFIG)?;
    if immutable.split_whitespace().collect::<Vec<_>>()
        != [slot.to_string(), format!("PARTUUID={state}")]
    {
        return Err(refuse("candidate immutable slot/state identity is wrong"));
    }
    let fstab = text(root, "etc/fstab")?;
    crate::immutable::check(&root.location()?, &fstab)?;
    let mut counts = [0; 5];
    for line in fstab.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let f = line.split_whitespace().collect::<Vec<_>>();
        if f.is_empty() {
            continue;
        }
        if f.len() < 6 {
            return Err(refuse("incomplete fstab record"));
        }
        match f[1] {
            "/" => {
                counts[0] += 1;
                if f[0] != format!("PARTUUID={uuid}")
                    || f[2] != "ext4"
                    || !f[3].split(',').any(|o| o == "ro")
                    || f[3].split(',').any(|o| o == "rw")
                    || f[5] != "0"
                {
                    return Err(refuse("candidate root fstab is not immutable"));
                }
            }
            "/var" => {
                counts[1] += 1;
                if f[0] != format!("PARTUUID={state}")
                    || f[2] != "ext4"
                    || f[3].split(',').any(|o| o == "ro")
                {
                    return Err(refuse("candidate state fstab is wrong"));
                }
            }
            "/root" => {
                counts[2] += 1;
                if f[0] != "/var/roothome" || !f[3].split(',').any(|o| o == "bind") {
                    return Err(refuse("root home must use shared state"));
                }
            }
            "/tmp" => {
                counts[3] += 1;
                if f[2] != "tmpfs" {
                    return Err(refuse("/tmp must be runtime tmpfs"));
                }
            }
            "/boot" => {
                counts[4] += 1;
                if f[2] != "vfat" || f[0] != format!("PARTUUID={esp}") {
                    return Err(refuse("ESP fstab must use vfat"));
                }
            }
            p if ["/run", "/dev", "/proc", "/sys", "/home"]
                .iter()
                .any(|prefix| p == *prefix || p.starts_with(&format!("{prefix}/"))) =>
            {
                return Err(refuse("unexpected runtime mount in fstab"));
            }
            _ => {}
        }
    }
    if counts != [1, 1, 1, 1, 1] {
        return Err(refuse(
            "candidate must have exactly one root/state/home/tmp/ESP fstab record",
        ));
    }
    Ok(())
}

fn accounts(root: &Root) -> Result<()> {
    let mut tables = BTreeMap::new();
    for (file, fields) in [("passwd", 7), ("shadow", 9), ("group", 4), ("gshadow", 4)] {
        let p = format!("etc/{file}");
        if file == "gshadow" && root.metadata(Path::new(&p))?.is_none() {
            tables.insert(file, BTreeMap::new());
            continue;
        }
        let m = root
            .metadata(Path::new(&p))?
            .context("account file missing")?;
        if !m.is_file()
            || m.uid() != 0
            || m.mode() & 0o022 != 0
            || ((file == "shadow" || file == "gshadow") && m.mode() & 0o007 != 0)
        {
            return Err(refuse("unsafe account file ownership/type/mode"));
        }
        let mut table = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for line in text(root, &p)?.lines() {
            let f = line.split(':').map(str::to_string).collect::<Vec<_>>();
            if f.len() != fields || f[0].is_empty() || table.contains_key(&f[0]) {
                return Err(refuse("malformed or duplicate account record"));
            }
            if matches!(file, "passwd" | "group") {
                let id = f[2]
                    .parse::<u32>()
                    .map_err(|_| refuse("invalid account id"))?;
                if !ids.insert(id) {
                    return Err(refuse("duplicate numeric account identity"));
                }
            }
            table.insert(f[0].clone(), f);
        }
        tables.insert(file, table);
    }
    let passwd = &tables["passwd"];
    let groups = &tables["group"];
    if passwd.get("root").is_none_or(|r| r[2] != "0") {
        return Err(refuse("candidate has no root account"));
    }
    let gids = groups
        .values()
        .map(|g| g[2].parse::<u32>())
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    for (name, p) in passwd {
        if !gids.contains(&p[3].parse()?) || (p[1] == "x" && !tables["shadow"].contains_key(name)) {
            return Err(refuse("incomplete user/group/shadow identity"));
        }
    }
    if tables["shadow"].keys().any(|n| !passwd.contains_key(n))
        || tables["gshadow"].keys().any(|n| !groups.contains_key(n))
    {
        return Err(refuse("orphan shadow identity"));
    }
    for (name, g) in groups {
        for member in g[3].split(',').filter(|s| !s.is_empty()) {
            if !passwd.contains_key(member) {
                return Err(refuse("group contains unknown user"));
            }
        }
        if tables["gshadow"].is_empty() {
            continue;
        }
        if !tables["gshadow"].contains_key(name) {
            return Err(refuse("group shadow identity is missing"));
        }
        let gs = &tables["gshadow"][name];
        if g[3]
            .split(',')
            .filter(|s| !s.is_empty())
            .collect::<BTreeSet<_>>()
            != gs[3]
                .split(',')
                .filter(|s| !s.is_empty())
                .collect::<BTreeSet<_>>()
        {
            return Err(refuse("group and gshadow membership differ"));
        }
        for admin in gs[2].split(',').filter(|s| !s.is_empty()) {
            if !passwd.contains_key(admin) {
                return Err(refuse("gshadow contains unknown administrator"));
            }
        }
    }
    Ok(())
}
fn ssh(root: &Root, init: InitSystem) -> Result<()> {
    let enabled = match init {
        InitSystem::Systemd => root
            .metadata(Path::new(
                "etc/systemd/system/multi-user.target.wants/sshd.service",
            ))?
            .is_some(),
        InitSystem::Openrc => root
            .metadata(Path::new("etc/runlevels/default/sshd"))?
            .is_some(),
    };
    if enabled {
        let key = root
            .metadata(Path::new("etc/ssh/ssh_host_ed25519_key"))?
            .context("SSH identity must be seeded before enabling sshd")?;
        if !key.is_file() || key.uid() != 0 || key.mode() & 0o077 != 0 || key.len() == 0 {
            return Err(refuse("unsafe or missing SSH host key"));
        }
        if root
            .open_resolved(Path::new("etc/ssh/ssh_host_ed25519_key.pub"))?
            .is_none()
        {
            return Err(refuse("SSH public host key is missing"));
        }
    }
    Ok(())
}
fn receipts(root: &Root) -> Result<()> {
    if let Some(text) = root.optional_text(Path::new(levitate_recipe::system::RECEIPT))? {
        let receipt = levitate_recipe::system::Receipt::parse(Some(&text))?;
        for package in receipt.packages.values() {
            for path in package.files.keys() {
                if root.metadata(Path::new(path))?.is_none() {
                    return Err(refuse(format!(
                        "selected system package file is missing: {path}"
                    )));
                }
            }
        }
        return Ok(());
    }
    let profile = Path::new("usr/lib/recipe/profile");
    if root.metadata(profile)?.is_none() {
        return Ok(());
    } // Base image: system package selection arrives in P06.
    if root
        .metadata(profile)?
        .is_some_and(|m| m.file_type().is_symlink())
    {
        let target = std::fs::read_link(root.entry(profile, false)?.path())?;
        if target.is_absolute()
            || target
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(refuse(
                "system Recipe profile must remain within its generation",
            ));
        }
    }
    let receipt = text(root, "usr/lib/recipe/profile/.installed.json")?;
    let receipt: serde_json::Value = serde_json::from_str(&receipt)?;
    if receipt["schema_version"] != 1 {
        return Err(refuse("unsupported system Recipe receipt"));
    }
    let packages = receipt["packages"]
        .as_object()
        .context("invalid Recipe package receipt")?;
    let mut seen = BTreeSet::new();
    for (name, package) in packages {
        if package["name"] != *name {
            return Err(refuse("Recipe package identity differs from receipt"));
        }
        for p in package["files"]
            .as_array()
            .context("invalid Recipe file receipt")?
        {
            let p = p.as_str().context("invalid Recipe path")?;
            relative(p)?;
            if !seen.insert(p) {
                return Err(refuse("duplicate system Recipe file ownership"));
            }
            if root.open_resolved(&profile.join(p))?.is_none() {
                return Err(refuse("system Recipe receipt names a missing file"));
            }
        }
    }
    Ok(())
}

fn modules(
    root: &Root,
    base: &str,
    release: &str,
    machine: u16,
    archive: &BTreeMap<String, String>,
) -> Result<()> {
    for metadata in ["modules.dep", "modules.builtin"] {
        let p = format!("{base}/{metadata}");
        let file = root
            .open_resolved(relative(&p)?)?
            .context("module metadata missing")?;
        let mut file = file;
        if archive.get(&p) != Some(&crate::generation::hash_reader(&mut file)?) {
            return Err(refuse("initramfs module metadata differs from candidate"));
        }
    }
    let mut paths = BTreeSet::new();
    for line in text(root, &format!("{base}/modules.dep"))?.lines() {
        let (file, deps) = line
            .split_once(':')
            .context("malformed module dependencies")?;
        for p in std::iter::once(file).chain(deps.split_whitespace()) {
            relative(p)?;
            paths.insert(p.to_string());
        }
    }
    let mut expanded = BTreeMap::new();
    for p in paths {
        let mut file = root
            .open_resolved(relative(&format!("{base}/{p}"))?)?
            .context("module dependency is missing")?;
        let name = p
            .strip_suffix(".xz")
            .or_else(|| p.strip_suffix(".gz"))
            .or_else(|| p.strip_suffix(".zst"))
            .unwrap_or(&p);
        if !name.ends_with(".ko") {
            return Err(refuse("unsupported kernel module file"));
        }
        let (digest, arch, version) = if name == p {
            module_stream(&mut file, release)?
        } else {
            let program = if p.ends_with(".xz") {
                "xz"
            } else if p.ends_with(".gz") {
                "gzip"
            } else {
                "zstd"
            };
            let mut child = Command::new(program)
                .arg("-dc")
                .stdin(Stdio::from(file))
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?;
            let result = module_stream(&mut child.stdout.take().unwrap(), release);
            if result.is_err() {
                let _ = child.kill();
            }
            let status = child.wait()?;
            if !status.success() {
                return Err(refuse("kernel module decompression failed"));
            }
            result?
        };
        if arch != machine || !version {
            return Err(refuse(
                "kernel module architecture/vermagic does not match kernel",
            ));
        }
        expanded.insert(format!("{base}/{name}"), digest);
    }
    for (p, hash) in archive.iter().filter(|(p, _)| p.ends_with(".ko")) {
        if expanded.get(p) != Some(hash) {
            return Err(refuse(
                "initramfs module does not match candidate module tree",
            ));
        }
    }
    Ok(())
}
fn module_stream(input: &mut impl Read, release: &str) -> Result<(String, u16, bool)> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    let mut prefix = Vec::new();
    let mut carry = Vec::new();
    let mut version = false;
    let needle = format!("vermagic={release}").into_bytes();
    let mut bytes = 0usize;
    let mut buffer = [0u8; 65536];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n;
        if bytes > 64 * 1024 * 1024 {
            return Err(refuse("kernel module exceeds 64 MiB"));
        }
        hash.update(&buffer[..n]);
        if prefix.len() < 64 {
            prefix.extend_from_slice(&buffer[..n.min(64 - prefix.len())]);
        }
        carry.extend_from_slice(&buffer[..n]);
        version |= carry.windows(needle.len() + 1).any(|w| {
            &w[..needle.len()] == needle
                && (w[needle.len()] == 0 || w[needle.len()].is_ascii_whitespace())
        });
        if carry.len() > 128 {
            carry.drain(..carry.len() - 128);
        }
    }
    if prefix.len() < 64 || &prefix[..4] != b"\x7fELF" || prefix[4] != 2 || prefix[5] != 1 {
        return Err(refuse("kernel module is not ELF64"));
    }
    Ok((
        format!("{:x}", hash.finalize()),
        u16::from_le_bytes([prefix[18], prefix[19]]),
        version,
    ))
}

/// Release-set validation before any candidate mutation. Personalization and
/// slot identities are checked later by the shared finalizer.
pub(crate) fn release(root:&Path,kernel_path:&Path,initrd_path:&Path,manifest:&distro_contract::release::ReleaseManifest)->Result<()> {
    use anyhow::ensure;
    let root=Root::open(root)?;
    let (machine,release)=kernel(kernel_path)?;
    ensure!(machine==manifest.identity.architecture.machine() && release==manifest.kernel.release,"release kernel architecture/release differs from manifest");
    let libc=manifest.identity.libc.as_str();
    crate::elf::executable(&root,Path::new("sbin/init"),machine,libc)?;
    crate::elf::executable(&root,Path::new("bin/sh"),machine,libc)?;
    runtime(&root,machine,libc)?;
    let archive=recinit::installed::inspect_installed(initrd_path)?;
    ensure!(archive.kernel_release==release && !archive.machines.is_empty() && archive.machines.iter().all(|m|*m==machine),"release initramfs does not match kernel");
    let base=format!("usr/lib/modules/{release}");
    let entry=root.entry(relative(&base)?,false)?;
    ensure!(recinit::installed::modules_digest(&entry.path())?==manifest.kernel.modules_sha256,"release module tree differs from manifest");
    let binding:serde_json::Value=serde_json::from_str(&text(&root,"usr/lib/recab/kernel.json")?)?;
    ensure!(binding["version"]==1 && binding["release"]==release && binding["kernel_digest"]==manifest.artifacts.kernel.sha256 && binding["modules_digest"]==manifest.kernel.modules_sha256,"release kernel/module binding differs from manifest");
    modules(&root,&base,&release,machine,&archive.files)?;
    Ok(())
}
