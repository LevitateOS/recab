//! Filesystem access confined to an opened system root. Parent directory
//! descriptors stay alive through each operation; symlinks are never parents
//! of writes, and reads that follow links resolve them inside this root.

use anyhow::{Context, Result, bail};
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub(crate) struct Root(File);
pub(crate) struct Entry {
    parent: File,
    name: OsString,
}

impl Root {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("opening system root '{}'", path.display()))?;
        Ok(Self(file))
    }

    pub fn entry(&self, relative: &Path, create_parents: bool) -> Result<Entry> {
        let mut parts = Vec::new();
        for part in relative.components() {
            match part {
                Component::Normal(name) => parts.push(name),
                _ => bail!(
                    "system path must be relative without '.' or '..': '{}'",
                    relative.display()
                ),
            }
        }
        let name = parts
            .pop()
            .context("a system entry needs a name")?
            .to_os_string();
        let mut parent = self.0.try_clone()?;
        for part in parts {
            let name = CString::new(part.as_bytes())?;
            if create_parents {
                // SAFETY: name is NUL terminated and parent owns the descriptor.
                let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) };
                if rc < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            // SAFETY: the result is either an error or a new owned descriptor.
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "opening parent of '{}' without following links",
                        relative.display()
                    )
                });
            }
            parent = unsafe { File::from_raw_fd(fd) };
        }
        Ok(Entry { parent, name })
    }

    pub fn metadata(&self, relative: &Path) -> Result<Option<fs::Metadata>> {
        match self
            .entry(relative, false)
            .and_then(|entry| entry.metadata())
        {
            Err(err) if missing(&err) => Ok(None),
            value => value,
        }
    }

    pub fn file_type(&self, relative: &Path) -> Result<Option<u32>> {
        match self
            .entry(relative, false)
            .and_then(|entry| entry.file_type())
        {
            Err(err) if missing(&err) => Ok(None),
            value => value,
        }
    }

    pub fn names(&self, relative: &Path) -> Result<Vec<OsString>> {
        let dir = if relative.as_os_str().is_empty() {
            self.0.try_clone()?
        } else {
            self.entry(relative, false)?.open_directory()?
        };
        let mut names = fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    pub fn open_resolved(&self, relative: &Path) -> Result<Option<File>> {
        #[repr(C)]
        struct How {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        let name = CString::new(relative.as_os_str().as_bytes())?;
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("invalid system path '{}'", relative.display());
        }
        // Ordinary package files need only descriptor-relative, no-follow opens.
        // Keep the pinned parent alive from the metadata check through the open.
        // Symlink resolution below retains the kernel's in-root constraints.
        match self.entry(relative, false) {
            Ok(entry) => match entry.file_type()? {
                None => return Ok(None),
                Some(libc::S_IFREG) => return entry.open().map(Some),
                _ => {}
            },
            Err(err) if missing(&err) => return Ok(None),
            Err(_) => {} // A symlink parent still uses constrained resolution below.
        }
        // RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS; absolute symlink targets
        // remain inside this root rather than reaching the host filesystem.
        let how = How {
            flags: (libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: 0x10 | 0x02,
        };
        // A concurrent rename can make the kernel unable to prove that a
        // symlink's '..' stays inside this root. openat2(2) explicitly permits
        // retrying EAGAIN. Retain every resolution flag and bound the retries;
        // never fall back to an unconstrained path open.
        let fd = {
            let mut fd = -1;
            for attempt in 0..32 {
                fd = unsafe {
                    libc::syscall(
                        libc::SYS_openat2,
                        self.0.as_raw_fd(),
                        name.as_ptr(),
                        &how,
                        std::mem::size_of::<How>(),
                    )
                };
                if fd >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
                    break;
                }
                if attempt < 31 {
                    std::thread::yield_now();
                }
            }
            fd
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return if err.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(err).with_context(|| {
                    format!(
                        "resolving system path '{}' within its root",
                        relative.display()
                    )
                })
            };
        }
        let file = unsafe { File::from_raw_fd(fd as i32) };
        if !file.metadata()?.is_file() {
            bail!("'{}' is not a regular system file", relative.display());
        }
        Ok(Some(file))
    }

    pub fn optional_text(&self, relative: &Path) -> Result<Option<String>> {
        let Some(file) = self.open_resolved(relative)? else {
            return Ok(None);
        };
        let mut text = String::new();
        file.take(64 * 1024 * 1024 + 1).read_to_string(&mut text)?;
        if text.len() > 64 * 1024 * 1024 {
            bail!("system text file exceeds 64 MiB");
        }
        Ok(Some(text))
    }

    pub fn resolved_path(&self, file: &File) -> Result<PathBuf> {
        let base = fs::read_link(format!("/proc/self/fd/{}", self.0.as_raw_fd()))?;
        let location = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        Ok(location.strip_prefix(base)?.to_path_buf())
    }

    pub fn location(&self) -> Result<PathBuf> {
        Ok(fs::read_link(format!(
            "/proc/self/fd/{}",
            self.0.as_raw_fd()
        ))?)
    }

    pub fn device(&self) -> Result<u64> {
        Ok(self.0.metadata()?.dev())
    }

    pub fn root_entry(&self) -> Result<Entry> {
        Ok(Entry {
            parent: self.0.try_clone()?,
            name: ".".into(),
        })
    }
}

impl Entry {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.parent.as_raw_fd())).join(&self.name)
    }
    /// Keep the parent pinned while a child process resolves this path.
    pub fn child_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.parent.as_raw_fd()
        ))
        .join(&self.name)
    }
    pub fn metadata(&self) -> Result<Option<fs::Metadata>> {
        Ok(target_root::metadata(&self.path())?)
    }
    pub fn file_type(&self) -> Result<Option<u32>> {
        let name = CString::new(self.name.as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the name and writable stat buffer remain valid for this call.
        let rc = unsafe {
            libc::fstatat(
                self.parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            return if err.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(err.into())
            };
        }
        // SAFETY: fstatat initialized the complete stat structure on success.
        Ok(Some(unsafe { stat.assume_init() }.st_mode & libc::S_IFMT))
    }
    pub fn open(&self) -> Result<File> {
        let file = self.open_at(libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
        if !file.metadata()?.is_file() {
            bail!("system payload is not a regular file");
        }
        Ok(file)
    }
    fn open_directory(&self) -> Result<File> {
        self.open_at(libc::O_RDONLY | libc::O_DIRECTORY, 0)
    }
    pub fn write(&self, bytes: &[u8], mode: u32) -> Result<()> {
        let mut file = self.open_at(libc::O_WRONLY | libc::O_CREAT | libc::O_NONBLOCK, mode)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            bail!("system write target must be a regular file without hardlink aliases");
        }
        file.set_len(0)?;
        file.write_all(bytes)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        Ok(())
    }
    fn open_at(&self, flags: i32, mode: u32) -> Result<File> {
        let name = CString::new(self.name.as_bytes())?;
        // SAFETY: the parent owns its descriptor and name is NUL terminated.
        let fd = unsafe {
            libc::openat(
                self.parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("opening system entry {:?} (flags={flags:#x})", self.name)
            });
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub fn read_link(&self) -> Result<PathBuf> {
        let name = CString::new(self.name.as_bytes())?;
        let mut bytes = vec![0u8; 4096];
        // SAFETY: the name and output buffer remain valid for this call.
        let n = unsafe {
            libc::readlinkat(
                self.parent.as_raw_fd(),
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if n as usize == bytes.len() {
            bail!("system symlink target exceeds 4095 bytes");
        }
        bytes.truncate(n as usize);
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }
    pub fn symlink(&self, target: &Path) -> Result<()> {
        let name = CString::new(self.name.as_bytes())?;
        let target = CString::new(target.as_os_str().as_bytes())?;
        // SAFETY: both strings are NUL terminated; the parent remains pinned.
        let rc =
            unsafe { libc::symlinkat(target.as_ptr(), self.parent.as_raw_fd(), name.as_ptr()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    pub fn remove_file(&self) -> Result<()> {
        let name = CString::new(self.name.as_bytes())?;
        // SAFETY: the parent remains pinned and name is NUL terminated.
        let rc = unsafe { libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), 0) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    pub fn remove(&self) -> Result<()> {
        match self.metadata()? {
            Some(meta) if meta.is_dir() => fs::remove_dir_all(self.path())?,
            Some(_) => fs::remove_file(self.path())?,
            None => {}
        }
        Ok(())
    }
}

pub(crate) fn missing(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
}

pub(crate) fn xattrs(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    unsafe {
        let size = libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0);
        if size < 0 {
            let err = std::io::Error::last_os_error();
            return if err.raw_os_error() == Some(libc::ENOTSUP) {
                Ok(Vec::new())
            } else {
                Err(err.into())
            };
        }
        let mut names = vec![0u8; size as usize];
        let size = libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len());
        if size < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut result = Vec::new();
        for name in names[..size as usize]
            .split(|b| *b == 0)
            .filter(|name| !name.is_empty())
        {
            let c_name = CString::new(name)?;
            let size = libc::lgetxattr(path.as_ptr(), c_name.as_ptr(), std::ptr::null_mut(), 0);
            if size < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut value = vec![0u8; size as usize];
            let actual = libc::lgetxattr(
                path.as_ptr(),
                c_name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            );
            if actual < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            value.truncate(actual as usize);
            result.push((name.to_vec(), value));
        }
        result.sort();
        Ok(result)
    }
}

pub(crate) fn set_xattrs(path: &Path, wanted: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    let existing = xattrs(path)?;
    let path = CString::new(path.as_os_str().as_bytes())?;
    for (name, _) in existing
        .iter()
        .filter(|(name, _)| !wanted.iter().any(|(want, _)| want == name))
    {
        let name = CString::new(name.as_slice())?;
        if unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    for (name, value) in wanted.iter().filter(|(name, value)| {
        !existing
            .iter()
            .any(|(old, bytes)| old == name && bytes == value)
    }) {
        let name = CString::new(name.as_slice())?;
        if unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// Isolate staging mounts from the caller and disable propagation before mounting.
pub(crate) fn private_mounts() -> Result<()> {
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } < 0 {
        return Err(std::io::Error::last_os_error())
            .context("creating a private staging mount namespace");
    }
    let slash = c"/";
    if unsafe {
        libc::mount(
            std::ptr::null(),
            slash.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error()).context("disabling staging mount propagation");
    }
    Ok(())
}

/// Also catch bind mounts of the same device: st_dev alone cannot identify them.
pub(crate) fn check_system_mounts(path: &Path) -> Result<()> {
    let root = fs::canonicalize(path)?;
    for mount in target_root::mount_table_at(Path::new("/proc/self/mountinfo"))? {
        if let Ok(relative) = mount.mount_point.strip_prefix(&root) {
            if relative.as_os_str().is_empty() {
                continue;
            }
            let first = relative.components().next().unwrap().as_os_str();
            if !crate::generation::EXCLUDED.contains(&first.as_bytes()) {
                bail!("unexpected mount inside system at '{}'", relative.display());
            }
        }
    }
    Ok(())
}

pub(crate) fn flush(root: &Root) -> Result<()> {
    if unsafe { libc::syncfs(root.0.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("flushing synchronized system");
    }
    Ok(())
}
