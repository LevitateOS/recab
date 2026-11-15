//! `recab update`: install a release into the other root and start it at the
//! next boot, once (docs/release-engineering.md §6a and §10).
//!
//! 1. `recupdate fetch --json` downloads and verifies the release into
//!    `/var/cache/recupdate/<serial>/`; recab checks `boot.tar.zst` against
//!    its `release.json` and unpacks the kernel and initramfs.
//! 2. `copy`: the other root becomes a copy of the booted root. Its boot entry
//!    is then removed again, so the other root cannot be started until step 4
//!    has written it.
//! 3. `recipe --system apply-release <folder>` writes the release into the
//!    other root (Recipe finds it through `recab status --json`).
//! 4. The other root's kernel, initramfs and entry are written; the booted
//!    root's entry and files are not touched.
//! 5. `boot-other`: the next start uses the other root once. The existing
//!    rule decides the rest: if it finishes starting it becomes the default,
//!    otherwise the machine starts the previous root again.
//!
//! Nothing before step 5 touches the booted root, its entry or the boot
//! default. A failure leaves the other root without a boot entry; the next
//! `recab update` or `recab copy` overwrites it.
//!
//! The programs run are found on `PATH`; `RECAB_RECUPDATE_BIN` and
//! `RECAB_RECIPE_BIN` name others (stand-ins). `RECUPDATE_CACHE_DIR`, which
//! recupdate reads, is read here too so both look in the same folder.

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ab::{copy_locked, pick_other, Running};
use crate::entry::{INITRD_FILE, KERNEL_FILE};
use crate::error::{classified, or_class, refuse, ErrorClass};
use crate::target::require_root;
use crate::Slot;

/// The number of steps `update` reports.
const STEPS: u32 = 5;
/// Where recupdate puts a fetched release, one folder per serial.
const RECUPDATE_CACHE: &str = "/var/cache/recupdate";
/// Where the kernel and initramfs of `boot.tar.zst` are unpacked.
const BOOT_FILES_DIR: &str = "/run/recab/boot-files";

/// What `recab update` installed.
#[derive(Debug, Clone, Serialize)]
pub struct Updated {
    pub version: String,
    pub serial: u64,
    /// The root that holds the release and starts at the next boot, once.
    pub root: Slot,
    /// The root that keeps running until the reboot (and starts again if
    /// the updated one does not finish starting).
    pub previous: Slot,
}

/// How `update` reports: human lines, or the JSON lines of §10 on stdout.
#[derive(Clone, Copy)]
pub struct Reporter {
    pub json: bool,
}

impl Reporter {
    fn step(self, n: u32, text: &str) {
        if self.json {
            println!(
                "{}",
                serde_json::json!({"event": "step", "n": n, "of": STEPS, "text": text})
            );
        } else {
            println!("Step {n} of {STEPS}: {text}");
        }
    }

    /// The last line of a finished update.
    pub fn done(self, updated: &Updated) {
        if self.json {
            println!(
                "{}",
                serde_json::json!({
                    "event": "done",
                    "restart_needed": true,
                    "version": updated.version,
                    "serial": updated.serial,
                })
            );
        } else {
            println!(
                "Root {} holds release {} (serial {}) and starts at the next boot, once. Reboot when ready. \
If it finishes starting it becomes the boot default; if it does not, the machine starts root {} again.",
                updated.root, updated.version, updated.serial, updated.previous
            );
        }
    }

    /// The last line of a failed update (JSON only; the text error is
    /// printed on stderr as for every command).
    pub fn failed(self, err: &anyhow::Error) {
        if self.json {
            let line = format!("{err:#}").replace('\n', " ");
            println!("{}", serde_json::json!({"event": "failed", "error": line}));
        }
    }
}

/// A program to run: the override variable's value, else `name` on `PATH`.
fn program(variable: &str, name: &str) -> OsString {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| name.into())
}

/// Run `command` with its stdout read line by line. A JSON `progress` line
/// is passed through (JSON) or dropped (text); a JSON `step` line is shown
/// indented (text) or dropped (JSON, where recab's own steps count); every
/// other JSON object goes to `event`; any other line goes to stdout (text)
/// or stderr (JSON). Stderr is recab's. Fails when the program does: with
/// its `failed` line's error when it printed one.
fn run(
    report: Reporter,
    mut command: Command,
    mut event: impl FnMut(&serde_json::Value),
) -> Result<()> {
    let shown = format!("{command:?}");
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {shown}"))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut failure = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.with_context(|| format!("reading the output of {shown}"))?;
        let parsed = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .filter(serde_json::Value::is_object);
        match parsed {
            Some(value) => match value["event"].as_str() {
                Some("progress") => {
                    if report.json {
                        println!("{line}");
                    }
                }
                Some("step") => {
                    if !report.json {
                        if let Some(text) = value["text"].as_str() {
                            println!("  {text}");
                        }
                    }
                }
                Some("failed") => {
                    failure = value["error"].as_str().map(str::to_string);
                }
                _ => event(&value),
            },
            None if report.json => eprintln!("{line}"),
            None => println!("{line}"),
        }
    }
    let status = child
        .wait()
        .with_context(|| format!("waiting for {shown}"))?;
    if status.success() {
        return Ok(());
    }
    Err(anyhow::anyhow!(match failure {
        Some(error) => format!("{shown} failed: {error}"),
        None => format!("{shown} failed ({status})"),
    }))
}

/// The release `recupdate fetch` left in its cache.
struct Fetched {
    version: String,
    serial: u64,
    folder: PathBuf,
}

/// Step 1: `recupdate fetch --json`; its `done` line names the release.
fn fetch(report: Reporter) -> Result<Fetched> {
    let mut command = Command::new(program("RECAB_RECUPDATE_BIN", "recupdate"));
    command.args(["fetch", "--json"]);
    let mut done = None;
    run(report, command, |value| {
        if value["event"] == "done" {
            done = Some(value.clone());
        }
    })?;
    let done = done.ok_or_else(|| {
        refuse("recupdate fetch finished without a 'done' line naming the release")
    })?;
    let version = done["version"]
        .as_str()
        .ok_or_else(|| refuse("recupdate fetch's 'done' line has no version"))?
        .to_string();
    let serial = done["serial"]
        .as_u64()
        .ok_or_else(|| refuse("recupdate fetch's 'done' line has no serial"))?;
    let cache = std::env::var_os("RECUPDATE_CACHE_DIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(RECUPDATE_CACHE), PathBuf::from);
    Ok(Fetched {
        version,
        serial,
        folder: cache.join(serial.to_string()),
    })
}

/// The SHA-256 of a file, in lower-case hex.
fn sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("opening '{}'", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading '{}'", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The unpacked kernel and initramfs; the folder is removed when dropped.
struct BootFiles {
    dir: PathBuf,
}

impl BootFiles {
    fn kernel(&self) -> PathBuf {
        self.dir.join(KERNEL_FILE)
    }

    fn initrd(&self) -> PathBuf {
        self.dir.join(INITRD_FILE)
    }
}

impl Drop for BootFiles {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Check `boot.tar.zst` against the fetched `release.json` and unpack it.
/// The archive holds exactly two regular files at its top level, `vmlinuz`
/// and `initramfs.img` (a `./` prefix and a `.` directory entry are
/// allowed); anything else is refused.
fn unpack_boot(fetched: &Fetched) -> Result<BootFiles> {
    let manifest_path = fetched.folder.join("release.json");
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(&manifest_path)
            .with_context(|| format!("reading '{}'", manifest_path.display()))?,
    )
    .with_context(|| format!("'{}' is not JSON", manifest_path.display()))?;
    if manifest["serial"].as_u64() != Some(fetched.serial) {
        return Err(refuse(format!(
            "'{}' is not release serial {} that recupdate fetched",
            manifest_path.display(),
            fetched.serial
        )));
    }
    let expected = manifest["boot"]["sha256"]
        .as_str()
        .ok_or_else(|| refuse(format!("'{}' has no boot.sha256", manifest_path.display())))?
        .to_ascii_lowercase();
    let archive = fetched.folder.join("boot.tar.zst");
    let actual = sha256(&archive)?;
    if actual != expected {
        return Err(refuse(format!(
            "'{}' has SHA-256 {actual}, but release.json says {expected}",
            archive.display()
        )));
    }

    let files = BootFiles {
        dir: PathBuf::from(BOOT_FILES_DIR),
    };
    match fs::remove_dir_all(&files.dir) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            return Err(
                anyhow::Error::from(err).context(format!("removing '{}'", files.dir.display()))
            );
        }
        _ => {}
    }
    fs::create_dir_all(&files.dir)
        .with_context(|| format!("creating '{}'", files.dir.display()))?;
    let decoder = zstd::stream::read::Decoder::new(
        fs::File::open(&archive).with_context(|| format!("opening '{}'", archive.display()))?,
    )
    .with_context(|| format!("reading '{}'", archive.display()))?;
    let mut tar = tar::Archive::new(decoder);
    let mut found = Vec::new();
    for entry in tar
        .entries()
        .with_context(|| format!("reading '{}'", archive.display()))?
    {
        let mut entry = entry.with_context(|| format!("reading '{}'", archive.display()))?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let name = path.trim_start_matches("./").trim_end_matches('/');
        let kind = entry.header().entry_type();
        if (name.is_empty() || name == ".") && kind.is_dir() {
            continue;
        }
        if !(kind.is_file() && [KERNEL_FILE, INITRD_FILE].contains(&name))
            || found.contains(&name.to_string())
        {
            return Err(refuse(format!(
                "'{}' holds '{path}'; it must hold exactly the two files {KERNEL_FILE} and {INITRD_FILE}",
                archive.display()
            )));
        }
        let dest = files.dir.join(name);
        let mut out =
            fs::File::create(&dest).with_context(|| format!("creating '{}'", dest.display()))?;
        std::io::copy(&mut entry, &mut out)
            .with_context(|| format!("unpacking '{name}' from '{}'", archive.display()))?;
        out.sync_all()?;
        found.push(name.to_string());
    }
    for name in [KERNEL_FILE, INITRD_FILE] {
        if !found.iter().any(|f| f == name) {
            return Err(refuse(format!(
                "'{}' has no {name}; it must hold exactly the two files {KERNEL_FILE} and {INITRD_FILE}",
                archive.display()
            )));
        }
    }
    Ok(files)
}

/// A failure after the other root was changed: the booted root, its entry
/// and the boot default are as they were.
fn other_left(to: Slot, err: anyhow::Error) -> anyhow::Error {
    classified(
        ErrorClass::PartialUpdate,
        format!(
            "{err:#}; nothing on the booted root, its boot entry or the boot default changed. \
Root {to} is half-written and has no boot entry, so it cannot be started; \
'recab update' again, or 'recab copy', overwrites it"
        ),
    )
}

/// `recab update`: see the module documentation.
pub fn update(report: Reporter) -> Result<Updated> {
    require_root("update")?;
    let running = Running::read()?;
    let _lock = running.target.lock()?;
    let (from, to) = (running.booted, running.other());
    if !running.esp.firmware_writable() {
        return Err(classified(
            ErrorClass::BackendUnavailable,
            "the firmware's boot variables cannot be changed here, so the updated root could not be picked for the next boot",
        ));
    }

    report.step(1, "Downloading and checking the release");
    let fetched = fetch(report).map_err(|err| or_class(err, ErrorClass::StateOrDetection))?;
    let boot = unpack_boot(&fetched).map_err(|err| or_class(err, ErrorClass::StateOrDetection))?;

    report.step(2, "Making the other root a copy of the booted root");
    copy_locked(&running, report.json)?;
    running
        .esp
        .remove_entry(to)
        .map_err(|err| other_left(to, err))?;

    report.step(3, "Applying the release to the other root");
    let mut command = Command::new(program("RECAB_RECIPE_BIN", "recipe"));
    command
        .args(["--system", "apply-release"])
        .arg(&fetched.folder);
    run(report, command, |_| {}).map_err(|err| other_left(to, err))?;

    report.step(4, "Writing the other root's boot files");
    let to_uuid = running.record.uuid(to).to_string();
    running
        .esp
        .install_boot_files(from, to, &to_uuid, &boot.kernel(), &boot.initrd())
        .map_err(|err| other_left(to, err))?;
    drop(boot);

    report.step(5, "Starting the other root at the next boot, once");
    pick_other(&running).map_err(|err| {
        classified(
            ErrorClass::PartialUpdate,
            format!(
                "root {to} holds release {} and can be started, but it could not be picked for the next boot ({err:#}); run 'recab boot-other'",
                fetched.version
            ),
        )
    })?;
    Ok(Updated {
        version: fetched.version,
        serial: fetched.serial,
        root: to,
        previous: from,
    })
}
