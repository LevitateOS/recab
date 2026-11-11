//! Materialize the root mount reference for a reusable system slot.
use crate::error::refuse;
use anyhow::Result;

pub(crate) fn for_slot(machine: &str, partuuid: &str, fs: &str) -> Result<String> {
    let mut found_root = false;
    let lines: Vec<String> = machine
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            if line.trim_start().starts_with('#') || fields.get(1) != Some(&"/") {
                return line.to_string();
            }
            found_root = true;
            let spec = format!("PARTUUID={partuuid}");
            fields[0] = &spec;
            match fields.get_mut(2) {
                Some(fstype) => *fstype = fs,
                None => fields.push(fs),
            }
            fields.join("\t")
        })
        .collect();
    if !found_root {
        return Err(refuse("the running system's fstab has no entry for /"));
    }
    Ok(lines.join("\n") + "\n")
}
