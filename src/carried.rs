//! The machine state an A/B update keeps: a list the image declares (its
//! `usr/lib/recab/carried`, packaged with recab), read from the image `stage`
//! writes and carried exactly. The format is stated at the top of the list.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// The list, relative to an image's root.
pub const CARRIED_LIST: &str = "usr/lib/recab/carried";

/// One entry of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carry {
    /// Copied over the image's, when the machine has it.
    File(String),
    /// Its entries copied over the image's directory, the image's others kept.
    Dir(String),
    /// Every entry of a directory whose name starts with the last component.
    Prefix(String),
    /// Merged by account name with the image's; the machine's entries win.
    Accounts(String),
}

impl Carry {
    pub fn path(&self) -> &str {
        match self {
            Carry::File(p) | Carry::Dir(p) | Carry::Prefix(p) | Carry::Accounts(p) => p,
        }
    }
}

/// The list the image under `root` declares; an image without one, or with a
/// line that is not an entry, is an error.
pub fn read(root: &Path) -> Result<Vec<Carry>> {
    let path = root.join(CARRIED_LIST);
    let text = target_root::read_optional(&path)?
        .with_context(|| format!("the image declares no carried machine state ('{}' is missing)", path.display()))?;
    parse(&text).with_context(|| format!("reading '{}'", path.display()))
}

fn parse(text: &str) -> Result<Vec<Carry>> {
    let mut entries = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kind, path) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let path = path.trim().to_string();
        if path.is_empty() || path.starts_with('/') || path.split('/').any(|c| c == "..") {
            bail!("line {}: '{line}' needs a path relative to / without '..'", number + 1);
        }
        entries.push(match kind {
            "file" => Carry::File(path),
            "dir" => Carry::Dir(path),
            "prefix" => Carry::Prefix(path),
            "accounts" => Carry::Accounts(path),
            other => bail!("line {}: '{other}' is not file, dir, prefix or accounts", number + 1),
        });
    }
    Ok(entries)
}
