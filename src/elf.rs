//! Read executable dependencies without executing code from the candidate.
use crate::error::refuse;
use crate::filesystem::Root;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(crate) struct Elf {
    pub machine: u16,
    pub interpreter: Option<String>,
    needed: Vec<String>,
    search: Vec<String>,
}
fn u16_at(b: &[u8], n: usize) -> u16 {
    u16::from_le_bytes(b[n..n + 2].try_into().unwrap())
}
fn u64_at(b: &[u8], n: usize) -> u64 {
    u64::from_le_bytes(b[n..n + 8].try_into().unwrap())
}
fn read(file: &mut File, offset: u64, size: usize) -> Result<Vec<u8>> {
    if size > 16 * 1024 * 1024
        || offset
            .checked_add(size as u64)
            .is_none_or(|end| end > file.metadata().map(|m| m.len()).unwrap_or(0))
    {
        return Err(refuse("invalid ELF bounds"));
    }
    let mut bytes = vec![0; size];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
impl Elf {
    pub fn read(file: &mut File) -> Result<Self> {
        let header = read(file, 0, 64)?;
        if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
            return Err(refuse(
                "candidate needs a 64-bit little-endian ELF executable",
            ));
        }
        let machine = u16_at(&header, 18);
        if !matches!(machine, 62 | 183) {
            return Err(refuse("unsupported executable architecture"));
        }
        let count = u16_at(&header, 56) as usize;
        let width = u16_at(&header, 54) as usize;
        if count == 0 || count > 4096 || width != 56 {
            return Err(refuse("invalid ELF program headers"));
        }
        let program = read(file, u64_at(&header, 32), count * width)?;
        let mut loads = Vec::new();
        let mut dynamic = None;
        let mut interpreter = None;
        for ph in program.chunks_exact(width) {
            let kind = u32::from_le_bytes(ph[..4].try_into().unwrap());
            let offset = u64_at(ph, 8);
            let length = u64_at(ph, 32);
            match kind {
                1 => loads.push((u64_at(ph, 16), length, offset)),
                2 => dynamic = Some((offset, length)),
                3 => {
                    if length > 4096 {
                        return Err(refuse("invalid ELF interpreter"));
                    }
                    let bytes = read(file, offset, length as usize)?;
                    interpreter = Some(
                        std::str::from_utf8(&bytes)?
                            .trim_end_matches('\0')
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        if loads.is_empty() {
            return Err(refuse("ELF has no loadable segments"));
        }
        let mut needed = Vec::new();
        let mut search = Vec::new();
        if let Some((offset, length)) = dynamic {
            let bytes = read(file, offset, length.try_into()?)?;
            let mut entries = Vec::new();
            let mut table = None;
            let mut table_size = None;
            for d in bytes.chunks_exact(16) {
                let key = u64_at(d, 0);
                let value = u64_at(d, 8);
                if key == 0 {
                    break;
                }
                if key == 5 {
                    table = Some(value)
                }
                if key == 10 {
                    table_size = Some(value)
                }
                if matches!(key, 1 | 15 | 29) {
                    entries.push((key, value));
                }
            }
            if !entries.is_empty() {
                let address = table.context("ELF string table missing")?;
                let offset = loads
                    .iter()
                    .find_map(|(base, len, offset)| {
                        (address >= *base && address - *base < *len)
                            .then(|| offset + address - base)
                    })
                    .context("ELF string table is not file-backed")?;
                let strings = read(
                    file,
                    offset,
                    table_size
                        .context("ELF string table size missing")?
                        .try_into()?,
                )?;
                for (key, index) in entries {
                    let value = strings
                        .get(index as usize..)
                        .context("invalid ELF string offset")?;
                    let end = value
                        .iter()
                        .position(|b| *b == 0)
                        .context("unterminated ELF string")?;
                    let text = std::str::from_utf8(&value[..end])?.to_string();
                    if key == 1 {
                        needed.push(text)
                    } else {
                        search.extend(text.split(':').map(str::to_string));
                    }
                }
            }
        }
        Ok(Self {
            machine,
            interpreter,
            needed,
            search,
        })
    }
}
pub(crate) fn executable(root: &Root, relative: &Path, machine: u16, libc: &str) -> Result<()> {
    linked(root,relative,machine,libc,true)
}
pub(crate) fn linked(root:&Root,relative:&Path,machine:u16,libc:&str,execute:bool)->Result<()> {
    let file = root.open_resolved(relative)?.ok_or_else(|| {
        refuse(format!(
            "required executable '{}' is missing",
            relative.display()
        ))
    })?;
    if execute && file.metadata()?.mode() & 0o111 == 0 {
        return Err(refuse("required executable has no execute permission"));
    }
    let mut queue = vec![(root.resolved_path(&file)?, file)];
    let mut seen = BTreeSet::new();
    while let Some((location, mut file)) = queue.pop() {
        if !seen.insert(location.clone()) {
            continue;
        }
        if seen.len() > 512 {
            return Err(refuse("too many executable dependencies"));
        }
        let elf = Elf::read(&mut file)?;
        if elf.machine != machine {
            return Err(refuse(
                "executable/dependency architecture does not match kernel",
            ));
        }
        if let Some(interpreter) = elf.interpreter {
            if !interpreter.starts_with('/') || (libc == "musl") != interpreter.contains("ld-musl-")
            {
                return Err(refuse(
                    "executable interpreter does not match declared libc",
                ));
            }
            let file = root
                .open_resolved(Path::new(interpreter.trim_start_matches('/')))?
                .context("ELF interpreter is missing")?;
            queue.push((root.resolved_path(&file)?, file));
        }
        let origin = location.parent().unwrap_or(Path::new(""));
        let mut dirs = elf
            .search
            .iter()
            .map(|p| {
                if let Some(rest) = p
                    .strip_prefix("$ORIGIN")
                    .or_else(|| p.strip_prefix("${ORIGIN}"))
                {
                    origin.join(rest.trim_start_matches('/'))
                } else {
                    PathBuf::from(p.trim_start_matches('/'))
                }
            })
            .collect::<Vec<_>>();
        dirs.extend(
            [
                "usr/lib64",
                "usr/lib",
                "lib64",
                "lib",
                "usr/lib/systemd",
                "usr/lib64/systemd",
            ]
            .map(PathBuf::from),
        );
        for library in elf.needed {
            if library.contains('/') || library.is_empty() {
                return Err(refuse("invalid executable library name"));
            }
            let mut found = None;
            for directory in &dirs {
                let mut normalized = PathBuf::new();
                for component in directory.components() {
                    match component {
                        std::path::Component::Normal(part) => normalized.push(part),
                        std::path::Component::ParentDir => {
                            if !normalized.pop() {
                                return Err(refuse("ELF search path escapes root"));
                            }
                        }
                        std::path::Component::CurDir => {}
                        _ => return Err(refuse("invalid ELF search path")),
                    }
                }
                if let Some(file) = root.open_resolved(&normalized.join(&library))? {
                    found = Some(file);
                    break;
                }
            }
            let file = found.ok_or_else(|| {
                refuse(format!(
                    "required executable library '{library}' is missing"
                ))
            })?;
            queue.push((root.resolved_path(&file)?, file));
        }
    }
    Ok(())
}
