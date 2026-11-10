//! Account bodies stay in memory or protected candidate files. Baselines contain
//! field fingerprints and membership fingerprints, never password hashes.
use crate::filesystem::Root;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
pub(crate) const FILES: [&str; 4] = ["etc/passwd", "etc/shadow", "etc/group", "etc/gshadow"];
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Row {
    fields: Vec<String>,
    members: BTreeMap<usize, BTreeSet<String>>,
}
pub(crate) type Base = BTreeMap<String, BTreeMap<String, Row>>;
pub(crate) type Tables = BTreeMap<String, BTreeMap<String, Vec<String>>>;
fn membership(file: &str, column: usize) -> bool {
    (file == "etc/group" && column == 3) || (file == "etc/gshadow" && column >= 2)
}
fn fingerprint(file: &str, fields: &[String]) -> Row {
    Row {
        fields: fields
            .iter()
            .map(|f| crate::generation::digest(f.as_bytes()))
            .collect(),
        members: fields
            .iter()
            .enumerate()
            .filter(|(i, _)| membership(file, *i))
            .map(|(i, f)| {
                (
                    i,
                    f.split(',')
                        .filter(|s| !s.is_empty())
                        .map(|s| crate::generation::digest(s.as_bytes()))
                        .collect(),
                )
            })
            .collect(),
    }
}
pub(crate) fn read(root: &Path) -> Result<Tables> {
    let root = Root::open(root)?;
    let mut result = BTreeMap::new();
    for file in FILES {
        let Some(text) = root.optional_text(Path::new(file))? else {
            continue;
        };
        ensure!(text.len() <= 1024 * 1024, "account file exceeds 1 MiB");
        let count = if file.ends_with("passwd") {
            7
        } else if file.ends_with("shadow") && !file.ends_with("gshadow") {
            9
        } else {
            4
        };
        let mut rows = BTreeMap::new();
        for line in text.lines() {
            let fields = line.split(':').map(str::to_owned).collect::<Vec<_>>();
            ensure!(
                fields.len() == count
                    && !fields[0].is_empty()
                    && !fields[0].chars().any(char::is_whitespace),
                "invalid account record"
            );
            let name = fields[0].clone();
            ensure!(
                rows.insert(name, fields).is_none(),
                "duplicate account name"
            );
        }
        result.insert(file.into(), rows);
    }
    Ok(result)
}
pub(crate) fn baseline(tables: &Tables) -> Base {
    tables
        .iter()
        .map(|(file, rows)| {
            (
                file.clone(),
                rows.iter()
                    .map(|(name, fields)| (name.clone(), fingerprint(file, fields)))
                    .collect(),
            )
        })
        .collect()
}
pub(crate) fn merge(
    old: &Base,
    current: &Tables,
    new: &Tables,
    choices: &BTreeMap<String, String>,
) -> Result<(BTreeMap<String, String>, Vec<String>)> {
    let mut result = BTreeMap::new();
    let mut conflicts = Vec::new();
    for file in FILES {
        let empty = BTreeMap::new();
        let o = old.get(file).unwrap_or(&empty);
        let empty_rows = BTreeMap::new();
        let c = current.get(file).unwrap_or(&empty_rows);
        let n = new.get(file).unwrap_or(&empty_rows);
        let names = o
            .keys()
            .chain(c.keys())
            .chain(n.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut output = BTreeMap::new();
        let mut conflict = false;
        let release = choices.get(file).is_some_and(|s| s == "release");
        for name in names {
            let original = o.get(&name);
            let local = c.get(&name);
            let incoming = n.get(&name);
            let cf = local.map(|v| fingerprint(file, v));
            let nf = incoming.map(|v| fingerprint(file, v));
            let row = if cf.as_ref() == original {
                incoming.cloned()
            } else if nf.as_ref() == original || cf == nf {
                local.cloned()
            } else if let (Some(o), Some(c), Some(n)) = (original, local, incoming) {
                ensure!(
                    o.fields.len() == c.len() && c.len() == n.len(),
                    "account field count changed"
                );
                let mut fields = Vec::new();
                for i in 0..c.len() {
                    if membership(file, i) {
                        let old = o.members.get(&i).context("membership baseline missing")?;
                        let cm = c[i]
                            .split(',')
                            .filter(|s| !s.is_empty())
                            .collect::<BTreeSet<_>>();
                        let nm = n[i]
                            .split(',')
                            .filter(|s| !s.is_empty())
                            .collect::<BTreeSet<_>>();
                        fields.push(
                            cm.union(&nm)
                                .copied()
                                .filter(|member| {
                                    let a =
                                        old.contains(&crate::generation::digest(member.as_bytes()));
                                    let b = cm.contains(member);
                                    let d = nm.contains(member);
                                    if b == a { d } else { b }
                                })
                                .collect::<Vec<_>>()
                                .join(","),
                        );
                    } else {
                        let a = &o.fields[i];
                        let b = crate::generation::digest(c[i].as_bytes());
                        let d = crate::generation::digest(n[i].as_bytes());
                        fields.push(if &b == a {
                            n[i].clone()
                        } else if &d == a || b == d {
                            c[i].clone()
                        } else {
                            conflict = true;
                            if release { n[i].clone() } else { c[i].clone() }
                        });
                    }
                }
                Some(fields)
            } else {
                conflict = true;
                if release {
                    incoming.cloned()
                } else {
                    local.cloned()
                }
            };
            if let Some(row) = row {
                output.insert(name, row);
            }
        }
        if conflict && !choices.contains_key(file) {
            conflicts.push(file.into());
        }
        if !output.is_empty() || current.contains_key(file) || new.contains_key(file) {
            result.insert(
                file.into(),
                output
                    .values()
                    .map(|f| f.join(":") + "\n")
                    .collect::<String>(),
            );
        }
    }
    Ok((result, conflicts))
}
