//! recab's contract for `cargo xtask gen`: the JSON Schemas of what its commands
//! print with `--json`. No front end reads a TypeScript module of it, so none is
//! generated.

use anyhow::Result;
use tool_contract::Contract;

use crate::{
    Comparison, Copied, ErrorPayload, Event, Health, Started, Status, VerifiedInstallation,
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
    Ok(c)
}
