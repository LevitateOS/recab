//! Frontend operations: concrete commands pinned to the status being viewed.
use crate::{
    System,
    status::{Action, Verdict},
};
use serde::{Deserialize, Serialize};
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Input {
    pub name: String,
    pub label: String,
    pub flag: Option<String>,
    pub required: bool,
    pub choices: Vec<String>,
}
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Operation {
    pub id: String,
    pub label: String,
    /// Arguments to recab, excluding its executable. Never a shell command.
    pub argv: Vec<String>,
    pub inputs: Vec<Input>,
    /// Ready permits requesting the operation; execution revalidates disk facts.
    pub verdict: Verdict,
    pub reason: Option<String>,
    pub destructive: bool,
}
fn input(name: &str, label: &str, flag: Option<&str>, required: bool, choices: &[&str]) -> Input {
    Input {
        name: name.into(),
        label: label.into(),
        flag: flag.map(Into::into),
        required,
        choices: choices.iter().map(|s| s.to_string()).collect(),
    }
}
pub(crate) fn views(
    sys: &System,
    actions: &[crate::ActionView],
    updates: Option<&crate::update::UpdateStatus>,
    recovery: Option<&str>,
    ticket: bool,
) -> Vec<Operation> {
    let p = sys.state.preparation.as_ref();
    let cas = p
        .map(|p| {
            vec![
                "--preparation".into(),
                p.id.clone(),
                "--revision".into(),
                p.revision.to_string(),
            ]
        })
        .unwrap_or_else(|| {
            vec![
                "--preparation".into(),
                "none".into(),
                "--revision".into(),
                "0".into(),
            ]
        });
    let mut out = vec![];
    for (id, label, action, command) in [
        (
            "prepare",
            "Prepare or continue the other system",
            Action::Prepare,
            "prepare",
        ),
        (
            "finalize",
            "Finalize the prepared system",
            Action::Finalize,
            "finalize",
        ),
        (
            "discard",
            "Discard all prepared changes",
            Action::Discard,
            "discard",
        ),
        (
            "set_next",
            "Try the prepared system on next reboot",
            Action::SetNext,
            "set-next",
        ),
        (
            "commit",
            "Keep the running system",
            Action::Commit,
            "commit",
        ),
        (
            "rollback",
            "Return to the protected system",
            Action::Rollback,
            "rollback",
        ),
    ] {
        let view = actions.iter().find(|v| v.action == action);
        let mut argv = vec![command.to_string()];
        if action == Action::SetNext {
            argv.push(sys.boot.selection.active.other().to_string());
        }
        if matches!(
            action,
            Action::Prepare
                | Action::Finalize
                | Action::Discard
                | Action::SetNext
                | Action::Commit
                | Action::Rollback
        ) {
            argv.extend(cas.clone());
        }
        if matches!(action, Action::SetNext | Action::Commit | Action::Rollback) {
            if let Some(g) = view
                .and_then(|v| sys.state.slot(v.slot))
                .and_then(|s| s.generation.as_ref())
            {
                argv.extend(["--generation".into(), g.id.clone()]);
            }
        }
        out.push(Operation {
            id: id.into(),
            label: label.into(),
            argv,
            inputs: vec![],
            verdict: view.map(|v| v.verdict).unwrap_or(Verdict::Refused),
            reason: view
                .and_then(|v| v.reason.clone())
                .or_else(|| view.is_none().then(|| "No preparation is open".into())),
            destructive: action == Action::Discard,
        });
    }
    let merge = updates
        .and_then(|u| u.merge.as_ref())
        .filter(|m| m.phase != "complete");
    if merge.is_some() {
        for op in &mut out {
            if matches!(op.id.as_str(), "prepare" | "finalize") {
                op.verdict = Verdict::Refused;
                op.reason = Some(
                    "Resolve or recover the selected release before preparing another change"
                        .into(),
                );
            }
        }
    }
    if p.is_none() && crate::status::failed_candidate(&sys.boot.selection, &sys.state) {
        let index = out.iter().position(|o| o.id == "prepare").unwrap();
        let mut replace = out[index].clone();
        replace.id = "replace_failed".into();
        replace.label = format!(
            "Replace failed slot {} with the running system",
            sys.boot.selection.active.other()
        );
        replace.destructive = true;
        out[index].verdict = Verdict::Refused;
        out[index].reason=Some("The failed generation has no preparation record. Retry its boot, or explicitly replace it; its changes are kept until then".into());
        out.push(replace);
    }
    let prepare = out.iter().find(|v| v.id == "prepare").unwrap();
    let writable = prepare.verdict != Verdict::Refused && p.is_none_or(|p| p.change.is_none());
    let write_reason = prepare
        .reason
        .clone()
        .unwrap_or_else(|| "Recover the interrupted operation before adding changes".into());
    let mut add = |id: &str,
                   label: &str,
                   args: Vec<String>,
                   inputs: Vec<Input>,
                   enabled: bool,
                   reason: Option<String>| {
        out.push(Operation {
            id: id.into(),
            label: label.into(),
            argv: args,
            inputs,
            verdict: if enabled {
                Verdict::Ready
            } else {
                Verdict::Refused
            },
            reason: if enabled {
                None
            } else {
                Some(reason.unwrap_or_else(|| "Not available in the current state".into()))
            },
            destructive: false,
        });
    };
    for (id, label, verb) in [
        (
            "install",
            "Install software in the prepared system",
            "install",
        ),
        ("change_update", "Update prepared software", "update"),
        ("remove", "Remove prepared software", "remove"),
    ] {
        let mut args = vec!["change".into(), verb.into()];
        args.extend(cas.clone());
        add(
            id,
            label,
            args,
            vec![input(
                "package",
                "Recipe or selected package",
                None,
                true,
                &[],
            )],
            writable,
            Some(write_reason.clone()),
        );
    }
    let mut edit = vec!["edit".into()];
    edit.extend(cas.clone());
    add(
        "edit",
        "Apply a configuration file to the prepared system",
        edit,
        vec![
            input("path", "System-relative path", None, true, &[]),
            input(
                "file",
                "Local file containing the new contents",
                Some("--file"),
                true,
                &[],
            ),
        ],
        writable,
        Some(write_reason),
    );
    let configured = crate::update::configuration(sys).is_ok();
    add(
        "update_check",
        "Check for system updates",
        vec!["update".into(), "check".into()],
        vec![],
        configured,
        Some("No usable update source is configured".into()),
    );
    let available = updates.and_then(|u| u.available.as_ref());
    let mut download = vec!["update".into(), "download".into()];
    if let Some(r) = available {
        download.push(r.id.clone());
    }
    add(
        "update_download",
        "Download the available release through Recipe",
        download,
        vec![],
        available.is_some(),
        Some("Check for an available release first".into()),
    );
    let downloaded = updates
        .and_then(|u| u.download.as_ref())
        .filter(|d| d.downloaded);
    let selected = updates
        .and_then(|u| u.pending_release.as_ref())
        .or_else(|| downloaded.map(|d| &d.release));
    let mut apply = vec!["update".into(), "apply".into()];
    if let Some(id) = selected {
        apply.push(id.clone());
    }
    apply.extend(cas.clone());
    add(
        "update_apply",
        "Merge the selected release, preserving prepared changes",
        apply,
        vec![],
        selected.is_some() && writable,
        Some("A verified release and a writable preparation are required".into()),
    );
    let mut media = vec!["update".into(), "apply".into()];
    media.extend(cas.clone());
    add(
        "update_media",
        "Merge a release manifest from disk",
        media,
        vec![input(
            "manifest",
            "Verified release.json path",
            Some("--manifest"),
            true,
            &[],
        )],
        writable,
        None,
    );
    let mut recover = vec!["update".into(), "recover".into()];
    recover.extend(cas.clone());
    add(
        "update_recover",
        "Continue the captured release merge",
        recover,
        vec![],
        merge.is_some()
            && merge
                .is_none_or(|m| m.unresolved == 0 || p.is_some_and(|p| p.revision != m.revision)),
        Some("Resolve outstanding conflicts first".into()),
    );
    if let Some(m) = merge {
        let merge_cas = vec![
            "--preparation".into(),
            m.preparation.clone(),
            "--revision".into(),
            m.revision.to_string(),
        ];
        for c in m.conflicts.iter().filter(|c| {
            c.resolution.is_none()
                && c.kind != "package_rebuild"
                && c.kind != "missing_or_unsafe_baseline"
        }) {
            let mut args = vec!["update".into(), "resolve".into(), c.id.clone()];
            args.extend(merge_cas.clone());
            add(
                &format!("resolve:{}", c.id),
                &format!("Resolve {} ({})", c.path, c.kind),
                args,
                vec![
                    input(
                        "choice",
                        "Resolution",
                        Some("--choice"),
                        true,
                        if c.kind == "package_ownership" {
                            &["local"]
                        } else if c.kind == "directory_tree" {
                            &["local", "release"]
                        } else {
                            &["local", "release", "edited"]
                        },
                    ),
                    input(
                        "file",
                        "File for an edited resolution (otherwise leave empty)",
                        Some("--file"),
                        false,
                        &[],
                    ),
                ],
                p.is_some_and(|p| p.revision == m.revision),
                Some("Recover merge publication before resolving conflicts".into()),
            );
        }
        let mut args = vec!["update".into(), "reproduce".into()];
        args.extend(merge_cas);
        add(
            "update_reproduce",
            "Reproduce selected software for this release through Recipe",
            args,
            vec![
                input("package", "Selected package", None, true, &[]),
                input(
                    "recipe",
                    "Recipe path (optional)",
                    Some("--recipe"),
                    false,
                    &[],
                ),
            ],
            p.is_some_and(|p| p.revision == m.revision)
                && m.conflicts
                    .iter()
                    .any(|c| c.kind == "package_rebuild" && c.resolution.is_none()),
            Some("The merge has no blocked package reproduction".into()),
        );
    }
    if p.is_some_and(|p| p.operation == "finalize" && p.phase != crate::PreparationPhase::Ready) {
        let mut args = vec!["finalize".into()];
        args.extend(cas.clone());
        let allowed = actions
            .iter()
            .find(|v| v.action == Action::Finalize)
            .is_some_and(|v| v.verdict != Verdict::Refused);
        add(
            "finalize_recover",
            "Continue interrupted finalization",
            args,
            vec![],
            allowed,
            None,
        );
    }
    if merge.is_some_and(|m| {
        m.conflicts
            .iter()
            .any(|c| c.kind == "missing_or_unsafe_baseline")
    }) {
        let mut args = vec!["update".into(), "adopt-baseline".into()];
        args.extend(cas.clone());
        add(
            "adopt_baseline",
            "Adopt the verified original release baseline",
            args,
            vec![input(
                "manifest",
                "Original release.json path",
                Some("--manifest"),
                true,
                &[],
            )],
            true,
            None,
        );
    }
    let mut cancel = vec!["update".into(), "cancel".into()];
    cancel.extend(cas.clone());
    let cancelable = p.is_some_and(|p| p.phase != crate::PreparationPhase::Ready)
        && updates.is_some_and(|u| u.pending_release.is_some())
        && crate::merge::can_cancel(sys).unwrap_or(false);
    add("update_cancel","Detach the selected release and retain prepared changes",cancel,vec![],cancelable,Some("An applied merge must be recovered; cancellation cannot reconstruct its captured changes".into()));
    let mut recover = vec!["change".into(), "recover".into()];
    recover.extend(cas.clone());
    add(
        "change_recover",
        "Recover the interrupted software/configuration change",
        recover,
        vec![],
        p.is_some_and(|p| p.change.is_some()),
        None,
    );
    if p.is_some_and(|p| p.operation == "prepare" && !p.synchronized) {
        let mut args = vec!["prepare".into()];
        args.extend(cas);
        add(
            "sync_recover",
            "Continue incremental synchronization",
            args,
            vec![],
            true,
            None,
        );
    }
    add(
        "trial_recover",
        "Assess or reconcile trial recovery",
        vec!["trial".into()],
        vec![],
        ticket,
        None,
    );
    let working = sys.store.busy() == Some(true);
    for op in &mut out {
        let reason = if working {
            Some("Another recab command is running")
        } else if ticket && !matches!(op.id.as_str(), "trial_recover" | "commit" | "rollback") {
            Some("Reconcile the pending trial before changing either system")
        } else if op.id != "trial_recover" {
            recovery
        } else {
            None
        };
        if let Some(reason) = reason {
            op.verdict = Verdict::Refused;
            op.reason = Some(reason.into());
        }
    }
    out
}

/// Operation boundaries on stderr; these never invent byte or percentage progress.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub event: String,
    pub operation: String,
    pub message: String,
    pub code: Option<u8>,
}
