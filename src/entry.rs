//! What recab writes on the ESP, as text: the names of entries and kernel
//! directories, one boot entry (`loader/entries/*.conf`) and `loader.conf`.
//! One grammar for each, used by every reader and writer: a line's key is its
//! first word; blank lines and `#` comments are kept and never read.

use crate::state::Slot;

/// systemd-boot's loader configuration and entries, relative to the ESP.
pub const LOADER_CONF: &str = "loader/loader.conf";
pub const ENTRIES_DIR: &str = "loader/entries";
/// The kernel and initramfs file names in an entry's directory on the ESP.
pub const KERNEL_FILE: &str = "vmlinuz";
pub const INITRD_FILE: &str = "initramfs.img";

/// Root starts read-only. Mutable installs remount it writable through fstab;
/// A/B entries require the initramfs to protect it and mount writable state.
const ROOT_OPTIONS: &[&str] = &["ro", "rootwait"];
/// Seconds after a kernel panic before the kernel reboots, on every entry
/// recab writes: a trial that panics reboots into the default slot.
pub(crate) const PANIC_KEY: &str = "panic=";
pub(crate) const PANIC_OPTION: &str = "panic=10";

/// The keys whose value is a file the entry loads from the ESP.
const BOOT_FILE_KEYS: &[&str] = &["linux", "efi", "initrd", "devicetree"];

/// An entry's (and its kernel directory's) name: `<prefix>-a` for an A/B
/// slot, `<prefix>` for the one root of a mutable install.
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

/// A slot's entry file name in [`ENTRIES_DIR`] (`<prefix>-a.conf`).
pub fn entry_name(prefix: &str, slot: Slot) -> String {
    entry_file(prefix, Some(slot))
}

/// The slot an entry id names, as systemd-boot matches ids: with or without
/// `.conf`, in any case.
pub(crate) fn slot_of_entry(prefix: &str, id: &str) -> Option<Slot> {
    let id = id.trim().to_ascii_lowercase();
    let stem = id.strip_suffix(".conf").unwrap_or(&id);
    Slot::ALL
        .into_iter()
        .find(|slot| stem == layout_name(prefix, Some(*slot)))
}

/// Two entry ids name the same entry (systemd-boot's matching).
pub(crate) fn same_entry(a: &str, b: &str) -> bool {
    let stem = |id: &str| {
        let id = id.trim().to_ascii_lowercase();
        id.strip_suffix(".conf").map(str::to_string).unwrap_or(id)
    };
    stem(a) == stem(b)
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

/// A partition's PARTUUID as a `root=` names it, lower-cased: recab names
/// roots by `root=PARTUUID=` (or the equivalent `/dev/disk/by-partuuid/`).
fn root_partuuid(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(['"', '\'']);
    ["PARTUUID=", "/dev/disk/by-partuuid/"]
        .iter()
        .find_map(|prefix| {
            (value.len() > prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix))
                .then(|| value[prefix.len()..].to_ascii_lowercase())
        })
}

/// The kernel command line's root partition, when it names one by
/// PARTUUID (the kernel uses the last `root=`).
pub(crate) fn cmdline_root(cmdline: &str) -> Option<String> {
    cmdline
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("root="))
        .next_back()
        .and_then(root_partuuid)
}

/// The raw `root=` of a command line, for messages.
pub(crate) fn cmdline_root_text(cmdline: &str) -> Option<&str> {
    cmdline
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("root="))
        .next_back()
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
    /// the root by `partuuid`, the root options, `panic=` and `extra`.
    pub fn new(title: &str, dir: &str, initrd: bool, partuuid: &str, extra: &str) -> Self {
        let mut entry = Self {
            lines: vec![format!("title {title}")],
        };
        entry.set_boot_files(dir, initrd);
        let mut options = vec![format!("root=PARTUUID={partuuid}")];
        options.extend(ROOT_OPTIONS.iter().map(|o| o.to_string()));
        options.push(PANIC_OPTION.to_string());
        options.extend(extra.split_whitespace().map(str::to_string));
        entry.set_options(options);
        entry
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
    pub fn options(&self) -> Vec<String> {
        self.values("options")
            .flat_map(|v| v.split_whitespace().map(str::to_string))
            .collect()
    }

    /// Replace the options with one `options` line where the first was, or
    /// at the end.
    pub fn set_options(&mut self, options: Vec<String>) {
        self.replace_keys(&["options"], vec![format!("options {}", options.join(" "))]);
    }

    pub fn make_immutable(&mut self) {
        let mut options = self.options();
        if !options
            .iter()
            .any(|o| o == recinit::installed::IMMUTABLE_OPTION)
        {
            options.push(recinit::installed::IMMUTABLE_OPTION.to_string());
        }
        self.set_options(options);
    }

    pub fn set_generation(&mut self, id: &str) {
        let mut options = self
            .options()
            .into_iter()
            .filter(|option| !option.starts_with("recab.generation="))
            .collect::<Vec<_>>();
        options.push(format!("recab.generation={id}"));
        self.set_options(options);
    }

    pub fn set_slot(&mut self, slot: crate::Slot) {
        let mut options = self
            .options()
            .into_iter()
            .filter(|o| !o.starts_with("recab.slot="))
            .collect::<Vec<_>>();
        options.push(format!("recab.slot={slot}"));
        self.set_options(options);
    }
    pub fn set_artifacts(&mut self, kernel: &str, initrd: &str) {
        self.replace_keys(
            &["linux", "efi", "initrd"],
            vec![format!("linux /{kernel}"), format!("initrd /{initrd}")],
        );
    }
    pub fn artifacts(&self) -> anyhow::Result<(String, Option<String>)> {
        let kernels = self.values("linux").collect::<Vec<_>>();
        let initrds = self.values("initrd").collect::<Vec<_>>();
        if kernels.len() != 1 || initrds.len() > 1 || self.values("efi").next().is_some() {
            return Err(crate::error::refuse(
                "generation entry needs one kernel and at most one initramfs",
            ));
        }
        Ok((
            kernels[0].trim_start_matches('/').to_string(),
            initrds
                .first()
                .map(|p| p.trim_start_matches('/').to_string()),
        ))
    }
    /// The files the entry loads from the ESP.
    pub fn boot_files(&self) -> Vec<String> {
        BOOT_FILE_KEYS
            .iter()
            .flat_map(|key| self.values(key))
            .filter_map(|v| v.split_whitespace().next())
            .map(str::to_string)
            .collect()
    }

    /// Point the entry at the kernel (and initramfs) in `dir` on the ESP.
    pub fn set_boot_files(&mut self, dir: &str, initrd: bool) {
        let mut lines = vec![format!("linux /{dir}/{KERNEL_FILE}")];
        if initrd {
            lines.push(format!("initrd /{dir}/{INITRD_FILE}"));
        }
        self.replace_keys(&["linux", "efi", "initrd"], lines);
    }

    /// The entry boots through an initramfs.
    pub fn has_initrd(&self) -> bool {
        self.values("initrd").next().is_some()
    }

    /// The PARTUUID of the root the entry boots, if it names one so.
    pub fn root_partuuid(&self) -> Option<String> {
        let options = self.options();
        options
            .iter()
            .filter_map(|o| o.strip_prefix("root="))
            .next_back()
            .and_then(root_partuuid)
    }

    /// Replace every line with one of `keys` by `new`, placed where the
    /// first of them was (or at the end).
    fn replace_keys(&mut self, keys: &[&str], new: Vec<String>) {
        let at = self
            .lines
            .iter()
            .position(|line| key_value(line).is_some_and(|(k, _)| keys.contains(&k)));
        self.lines
            .retain(|line| !key_value(line).is_some_and(|(k, _)| keys.contains(&k)));
        let at = at.unwrap_or(self.lines.len()).min(self.lines.len());
        self.lines.splice(at..at, new);
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
