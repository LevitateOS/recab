//! What recab writes on the ESP, as text: the names of entries and kernel
//! directories, one boot entry (`loader/entries/*.conf`) and `loader.conf`.
//! One grammar for each, used by every reader and writer: a line's key is its
//! first word; blank lines and `#` comments are kept and never read.

use crate::Slot;

/// systemd-boot's loader configuration and entries, relative to the ESP.
pub const LOADER_CONF: &str = "loader/loader.conf";
pub const ENTRIES_DIR: &str = "loader/entries";
/// The kernel and initramfs file names in an entry's directory on the ESP.
pub const KERNEL_FILE: &str = "vmlinuz";
pub const INITRD_FILE: &str = "initramfs.img";

/// Every root starts read-only. A mutable install remounts it writable
/// through fstab; an A/B root stays read-only.
const ROOT_OPTIONS: &[&str] = &["ro", "rootwait", "panic=10"];

/// The keys whose value is a file the entry loads from the ESP.
const BOOT_FILE_KEYS: &[&str] = &["linux", "efi", "initrd", "devicetree"];

/// Kernel options recab sets itself; caller-supplied extra options never
/// carry them.
const OWN_OPTIONS: &[&str] = &[
    "root=",
    "rootflags=",
    "rootfstype=",
    "init=",
    "rdinit=",
    "recab.",
];

/// An entry's (and its kernel directory's) name: `<prefix>-a` for an A/B
/// root, `<prefix>` for the one root of a mutable install.
pub fn layout_name(prefix: &str, slot: Option<Slot>) -> String {
    match slot {
        Some(slot) => format!("{prefix}-{}", slot.to_string().to_ascii_lowercase()),
        None => prefix.to_string(),
    }
}

/// The entry file of `layout_name`, in [`ENTRIES_DIR`].
pub(crate) fn entry_file(prefix: &str, slot: Option<Slot>) -> String {
    format!("{}.conf", layout_name(prefix, slot))
}

/// A root's entry file name in [`ENTRIES_DIR`] (`<prefix>-a.conf`).
pub fn entry_name(prefix: &str, slot: Slot) -> String {
    entry_file(prefix, Some(slot))
}

/// The root an entry id names, as systemd-boot matches ids: with or without
/// `.conf`, in any case.
pub(crate) fn slot_of_entry(prefix: &str, id: &str) -> Option<Slot> {
    let id = id.trim().to_ascii_lowercase();
    let stem = id.strip_suffix(".conf").unwrap_or(&id);
    Slot::ALL
        .into_iter()
        .find(|slot| stem == layout_name(prefix, Some(*slot)))
}

/// A line's key and value, or `None` for a blank line or a comment.
fn key_value(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    Some(
        line.split_once(char::is_whitespace)
            .map_or((line, ""), |(key, value)| (key, value.trim())),
    )
}

/// A partition's PARTUUID as a `root=` names it, lower-cased.
fn root_partuuid(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(['"', '\'']);
    ["PARTUUID=", "/dev/disk/by-partuuid/"]
        .iter()
        .find_map(|prefix| {
            (value.len() > prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix))
                .then(|| value[prefix.len()..].to_ascii_lowercase())
        })
}

/// The raw `root=` of a command line (the kernel uses the last one).
pub(crate) fn cmdline_root_text(cmdline: &str) -> Option<&str> {
    cmdline
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("root="))
        .next_back()
}

/// The kernel command line's root partition, when it names one by PARTUUID.
pub(crate) fn cmdline_root(cmdline: &str) -> Option<String> {
    cmdline_root_text(cmdline).and_then(root_partuuid)
}

/// Caller-supplied kernel options without the ones recab sets itself.
pub(crate) fn extra_options(options: &str) -> String {
    options
        .split_whitespace()
        .filter(|o| {
            !matches!(*o, "ro" | "rw" | "rootwait")
                && !o.starts_with("panic=")
                && !OWN_OPTIONS.iter().any(|prefix| o.starts_with(prefix))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One boot entry, as lines, so what recab does not edit stays as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    lines: Vec<String>,
}

impl Entry {
    pub fn parse(text: &str) -> Self {
        Self {
            lines: text.lines().map(str::to_string).collect(),
        }
    }

    /// A new entry: `title`, the kernel (and initramfs) in `dir` on the ESP,
    /// the root by `partuuid`, the root options and `extra`. An A/B root's
    /// entry also asks the initramfs for the locked boot.
    pub fn new(
        title: &str,
        dir: &str,
        initrd: bool,
        partuuid: &str,
        ab: bool,
        extra: &str,
    ) -> Self {
        let mut lines = vec![
            format!("title {title}"),
            format!("linux /{dir}/{KERNEL_FILE}"),
        ];
        if initrd {
            lines.push(format!("initrd /{dir}/{INITRD_FILE}"));
        }
        let mut options = vec![format!("root=PARTUUID={partuuid}")];
        options.extend(ROOT_OPTIONS.iter().map(|o| o.to_string()));
        if ab {
            options.push(recinit::installed::IMMUTABLE_OPTION.to_string());
        }
        options.extend(extra_options(extra).split_whitespace().map(str::to_string));
        lines.push(format!("options {}", options.join(" ")));
        Self { lines }
    }

    pub fn render(&self) -> String {
        self.lines.join("\n") + "\n"
    }

    fn values<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.lines
            .iter()
            .filter_map(move |line| key_value(line).filter(|(k, _)| *k == key).map(|(_, v)| v))
    }

    /// The kernel options, all `options` lines joined (as systemd-boot does).
    pub fn options(&self) -> String {
        self.values("options").collect::<Vec<_>>().join(" ")
    }

    /// The files the entry loads from the ESP, relative to it.
    pub fn boot_files(&self) -> Vec<String> {
        BOOT_FILE_KEYS
            .iter()
            .flat_map(|key| self.values(key))
            .filter_map(|v| v.split_whitespace().next())
            .map(|v| v.trim_start_matches('/').to_string())
            .collect()
    }

    /// The kernel and the initramfs the entry loads, relative to the ESP.
    pub fn kernel_and_initrd(&self) -> (Option<String>, Option<String>) {
        let first = |key| {
            self.values(key)
                .next()
                .and_then(|v| v.split_whitespace().next())
                .map(|v| v.trim_start_matches('/').to_string())
        };
        (first("linux"), first("initrd"))
    }

    /// The PARTUUID of the root the entry boots, if it names one so.
    pub fn root_partuuid(&self) -> Option<String> {
        cmdline_root(&self.options())
    }
}

/// loader.conf's default entry: systemd-boot uses the last `default` line.
pub(crate) fn loader_default(text: &str) -> Option<String> {
    text.lines()
        .filter_map(key_value)
        .filter(|(key, value)| *key == "default" && !value.is_empty())
        .map(|(_, value)| value.split_whitespace().next().unwrap_or(value).to_string())
        .next_back()
}

/// loader.conf with `entry` as its only `default`. A file recab creates
/// (`text` is `None`) also gets a menu timeout and the editor disabled; an
/// existing one keeps the operator's other settings.
pub(crate) fn loader_with_default(text: Option<&str>, entry: &str) -> String {
    let mut lines = vec![format!("default {entry}")];
    match text {
        Some(text) => lines.extend(
            text.lines()
                .filter(|line| !key_value(line).is_some_and(|(key, _)| key == "default"))
                .map(str::to_string),
        ),
        None => lines.extend(["timeout 5".to_string(), "editor 0".to_string()]),
    }
    lines.join("\n") + "\n"
}
