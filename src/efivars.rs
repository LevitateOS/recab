//! systemd-boot's firmware variables, read and written as bootctl does: each
//! is 4 bytes of attributes and a NUL-terminated UTF-16LE entry id, in
//! efivarfs (or, in a scratch tree, a directory of plain files). Changing
//! them needs no `bootctl`.

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::error::{classified, or_class, ErrorClass};
use crate::target::Firmware;

/// systemd-boot's vendor GUID, which names its EFI variables.
const LOADER_VENDOR_GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
/// EFI variable attributes: non-volatile, boot-service and runtime access.
const ATTRIBUTES: u32 = 0x7;
/// `FS_IMMUTABLE_FL`, which efivarfs sets on every variable.
const FS_IMMUTABLE_FL: libc::c_int = 0x10;
/// `EFIVARFS_MAGIC`, the filesystem type of real firmware variables.
const EFIVARFS_MAGIC: i64 = 0xde5e81e4;

/// The variables recab reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Var {
    /// The entry booted by default (over loader.conf's `default`).
    Default,
    /// The entry booted once, at the next boot; systemd-boot removes it then.
    OneShot,
    /// The entry this boot started (set by systemd-boot).
    Selected,
}

impl Var {
    pub fn name(self) -> &'static str {
        match self {
            Var::Default => "LoaderEntryDefault",
            Var::OneShot => "LoaderEntryOneShot",
            Var::Selected => "LoaderEntrySelected",
        }
    }
}

/// This machine's loader variables, or none (a system installed under a
/// root, whose firmware recab cannot see).
#[derive(Clone)]
pub(crate) struct EfiVars {
    firmware: Option<Firmware>,
}

impl EfiVars {
    pub fn new(firmware: Option<Firmware>) -> Self {
        Self { firmware }
    }

    fn path(firmware: &Firmware, var: Var) -> PathBuf {
        firmware.efivars.join(format!("{}-{LOADER_VENDOR_GUID}", var.name()))
    }

    /// The variable's entry id; `None` when it is unset, empty, or there is
    /// no firmware to read (systemd-boot treats an empty value as unset).
    pub fn get(&self, var: Var) -> Result<Option<String>> {
        let Some(firmware) = &self.firmware else { return Ok(None) };
        let path = Self::path(firmware, var);
        let context = || format!("reading the EFI variable '{}'", path.display());
        let Some(bytes) = target_root::read_optional_bytes(&path).with_context(context)? else {
            return Ok(None);
        };
        let units: Vec<u16> = bytes
            .get(4..)
            .with_context(context)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|unit| *unit != 0)
            .collect();
        let value = String::from_utf16(&units).map_err(|_| anyhow!("not a UTF-16 string")).with_context(context)?;
        Ok((!value.is_empty()).then_some(value))
    }

    /// Changing a variable needs a UEFI system with its variables mounted.
    pub fn can_change(&self) -> Result<()> {
        let firmware = self.firmware.as_ref().ok_or_else(|| {
            classified(
                ErrorClass::BackendUnavailable,
                "the firmware variables of a system installed under a root are not this machine's",
            )
        })?;
        for (dir, missing) in [
            (&firmware.efi_dir, "this host does not appear to be booted in UEFI mode"),
            (&firmware.efivars, "mount efivarfs or boot under UEFI"),
        ] {
            if !dir.is_dir() {
                return Err(classified(
                    ErrorClass::BackendUnavailable,
                    format!("changing the boot selection needs '{}', which is missing: {missing}", dir.display()),
                ));
            }
        }
        Ok(())
    }

    /// Set the variable to `value`, or remove it with `None`. A failure is the
    /// backend's (exit 20).
    pub fn set(&self, var: Var, value: Option<&str>) -> Result<()> {
        self.write(var, value).map_err(|err| or_class(err, ErrorClass::BackendUnavailable))
    }

    fn write(&self, var: Var, value: Option<&str>) -> Result<()> {
        self.can_change()?;
        let firmware = self.firmware.as_ref().expect("can_change checked it");
        let path = Self::path(firmware, var);
        let context = || format!("writing the EFI variable '{}'", path.display());
        let efivarfs = is_efivarfs(&firmware.efivars).with_context(context)?;
        // efivarfs marks variables immutable: clear that before changing one.
        if efivarfs && path.exists() {
            clear_immutable(&path).with_context(context)?;
        }
        let Some(value) = value else {
            return match fs::remove_file(&path) {
                Err(err) if err.kind() != ErrorKind::NotFound => Err(anyhow!(err)).with_context(context),
                _ => Ok(()),
            };
        };
        let mut bytes = ATTRIBUTES.to_le_bytes().to_vec();
        for unit in value.encode_utf16().chain([0]) {
            bytes.extend(unit.to_le_bytes());
        }
        if !efivarfs {
            return fs::write(&path, &bytes).with_context(context);
        }
        // efivarfs replaces a variable with each complete write; it is never
        // truncated.
        let mut file =
            fs::OpenOptions::new().write(true).create(true).truncate(false).open(&path).with_context(context)?;
        let written = file.write(&bytes).with_context(context)?;
        if written != bytes.len() {
            return Err(anyhow!("short write ({written} of {} bytes)", bytes.len())).with_context(context);
        }
        Ok(())
    }
}

/// Whether `dir` is efivarfs (else a directory of plain files, as in a
/// scratch tree).
fn is_efivarfs(dir: &Path) -> std::io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: statfs writes one statfs struct this function owns.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stat.f_type as i64 == EFIVARFS_MAGIC)
}

/// Clear `FS_IMMUTABLE_FL` on a variable.
fn clear_immutable(path: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = fs::File::open(path)?;
    let mut flags: libc::c_int = 0;
    // SAFETY: FS_IOC_GETFLAGS/SETFLAGS read and write one int on an open fd.
    unsafe {
        if libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if flags & FS_IMMUTABLE_FL != 0 {
            flags &= !FS_IMMUTABLE_FL;
            if libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &flags) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}
