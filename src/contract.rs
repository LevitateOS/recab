//! recab's contract for `cargo xtask gen`: the JSON `status`, `health` and a
//! failed `--json` command print, and the facts front ends need (error
//! classes and exit codes, the state file, the slots).

use anyhow::Result;
use std::collections::BTreeMap;
use tool_contract::Contract;

use crate::{ErrorClass, ErrorPayload, Health, Slot, Status};

pub fn export() -> Result<Contract> {
    let docs = concat!(env!("CARGO_MANIFEST_DIR"), "/docs");
    let mut c = Contract::new("recab", &["recab"]);
    c.schema::<Status>(docs, "status")?
        .schema::<Health>(docs, "health")?
        .schema::<ErrorPayload>(docs, "error")?;
    c.types_from::<Status>().types_from::<Health>().types_from::<ErrorPayload>();
    c.value_enum::<Slot>("SLOTS", "SLOT_HELP")?;
    c.constant("ERROR_SCHEMA_VERSION", "", &crate::ERROR_SCHEMA_VERSION)?;
    let classes: BTreeMap<String, serde_json::Value> = ErrorClass::ALL
        .iter()
        .map(|class| {
            let name = serde_json::to_value(class).unwrap().as_str().unwrap().to_string();
            (name, serde_json::json!({ "code": class.code(), "tag": class.tag() }))
        })
        .collect();
    c.constant("ERROR_CLASSES", "Each error class's exit status and the tag its text message starts with.", &classes)?;
    c.constant("STATE_FILE", "Where recab keeps its state on an installed system.", &crate::state_file())?;
    c.constant("TRIAL_TIMEOUT_SECS", "How long a trial waits for startup to finish.", &crate::TRIAL_TIMEOUT_SECS)?;
    Ok(c)
}
