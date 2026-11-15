//! recab's contract for `cargo xtask gen`: the JSON its commands print with
//! `--json`, and the facts front ends need (error classes and exit codes,
//! the installation record, where the other root is mounted, the roots).

use anyhow::Result;
use std::collections::BTreeMap;
use tool_contract::Contract;

use crate::{
    Comparison, Copied, ErrorClass, ErrorPayload, Event, Health, Slot, Started, Status,
    VerifiedInstallation,
};

pub fn export() -> Result<Contract> {
    let docs = concat!(env!("CARGO_MANIFEST_DIR"), "/docs");
    let mut c = Contract::new("recab", &["recab"]);
    c.schema::<Status>(docs, "status")?
        .schema::<Copied>(docs, "copy")?
        .schema::<Started>(docs, "started")?
        .schema::<Comparison>(docs, "comparison")?
        .schema::<Health>(docs, "health")?
        .schema::<VerifiedInstallation>(docs, "installation")?
        .schema::<Event>(docs, "event")?
        .schema::<ErrorPayload>(docs, "error")?;
    c.types_from::<Status>()
        .types_from::<Copied>()
        .types_from::<Comparison>()
        .types_from::<Health>()
        .types_from::<Event>()
        .types_from::<ErrorPayload>();
    c.constant(
        "SLOTS",
        "The roots, as every recab command and document spells them.",
        &Slot::ALL,
    )?;
    c.constant(
        "ERROR_SCHEMA_VERSION",
        "The schema of the error a failed `--json` command prints.",
        &crate::error::ERROR_SCHEMA_VERSION,
    )?;
    let classes: BTreeMap<&str, serde_json::Value> = ErrorClass::ALL
        .iter()
        .map(|class| {
            (
                class.name(),
                serde_json::json!({ "code": class.code(), "tag": class.tag() }),
            )
        })
        .collect();
    c.constant(
        "ERROR_CLASSES",
        "Each error class's exit status and the tag its text message starts with.",
        &classes,
    )?;
    c.constant(
        "INSTALLATION_FILE",
        "Where recab records which partition is root A and which is root B.",
        &crate::installation_file(),
    )?;
    c.constant(
        "OTHER_MOUNT",
        "Where the other (unlocked) root is mounted read-write on a running A/B system.",
        &crate::OTHER_MOUNT,
    )?;
    c.constant(
        "STARTUP_TIMEOUT_SECS",
        "How long `recab started` and `recab health` wait for startup to finish.",
        &crate::STARTUP_TIMEOUT_SECS,
    )?;
    Ok(c)
}
