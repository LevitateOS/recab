//! One command's view of the system it changes or reads, and the one way a
//! transition is applied: the boot selection first, recab's state second
//! (after a crash between the two the machine boots what was asked and the
//! state is what is stale), then the audit record.

use anyhow::Result;
use std::path::Path;

use crate::error::{classified, or_class, ErrorClass};
use crate::state::{plan, PersistentState, Request, Transition};
use crate::store::{now, Lock, Store};
use crate::systemd_boot::{Boot, SelectError, SystemdBoot};
use crate::target::Target;

/// The boot selection and recab's state, read once (under the run lock
/// when the command may change anything).
pub(crate) struct System {
    pub target: Target,
    pub backend: SystemdBoot,
    pub store: Store,
    pub boot: Boot,
    pub state: PersistentState,
    /// recab had written state (else `state` is the default).
    pub state_found: bool,
    _lock: Option<Lock>,
}

impl System {
    /// Read-only view (`status`) of this system, or of the system installed
    /// under `root`; works without root.
    pub fn read(root: Option<&Path>) -> Result<Self> {
        let target = match root {
            Some(root) => Target::installed(root)?,
            None => Target::running()?,
        };
        Self::read_with(target, None)
    }

    /// View for `command`, which changes the running system: refused unless
    /// root, and holding the run lock until dropped.
    pub fn locked(command: &str) -> Result<Self> {
        let target = Target::running()?;
        let lock = lock(&target, command)?;
        Self::read_with(target, Some(lock))
    }

    /// The view under a lock already held.
    pub fn read_locked(target: Target, lock: Lock) -> Result<Self> {
        Self::read_with(target, Some(lock))
    }

    fn read_with(target: Target, lock: Option<Lock>) -> Result<Self> {
        let backend = SystemdBoot::new(&target);
        let store = Store::new(&target);
        // An ESP or state the run cannot read is "the slots or state could
        // not be read" (exit 21), whatever the I/O error.
        let boot = backend
            .read()
            .map_err(|err| or_class(err.context("reading the boot selection"), ErrorClass::StateOrDetection))?;
        let loaded = store.load()?;
        Ok(Self {
            state_found: loaded.is_some(),
            state: loaded.unwrap_or_default(),
            target,
            backend,
            store,
            boot,
            _lock: lock,
        })
    }

    /// Plan `request` and apply it; nothing to do when the system already is
    /// where it leads.
    pub fn run(&self, request: Request) -> Result<()> {
        match plan(request, &self.boot.selection, &self.state)? {
            Some(transition) => self.apply(&transition),
            None => Ok(()),
        }
    }

    /// Apply `transition`: its boot selection, then recab's state, stamped
    /// with the operation and time, then the audit record.
    pub fn apply(&self, transition: &Transition) -> Result<()> {
        let name = transition.operation.name();
        if let Some(change) = transition.boot {
            self.backend.select(&self.boot, change, || self.target.init()).map_err(|err| select_failed(name, err))?;
        }
        let mut next = transition.next.clone();
        next.last_operation = Some(transition.operation.label());
        next.last_transition_unix = Some(now()?);
        self.store.save(&next).map_err(|err| {
            if transition.boot.is_some() {
                classified(
                    ErrorClass::PartialUpdate,
                    format!("{name} changed the boot selection, but recab's state was not saved: {err:#}"),
                )
            } else {
                err.context(format!("{name} aborted"))
            }
        })?;
        self.store.audit(transition.operation, self.boot.selection.active, &self.state, &next);
        Ok(())
    }
}

/// Root and the run lock, for `command`.
pub(crate) fn lock(target: &Target, command: &str) -> Result<Lock> {
    require_root(command)?;
    Store::new(target).lock()
}

pub(crate) fn require_root(command: &str) -> Result<()> {
    if !target_root::is_root() {
        return Err(classified(ErrorClass::Permission, format!("{command} must run as root")));
    }
    Ok(())
}

/// The error of a failed boot-selection change, saying what it changed.
fn select_failed(name: &str, err: SelectError) -> anyhow::Error {
    let SelectError { cause, firmware, entry, restore } = err;
    if let Some(restore) = restore {
        return classified(
            ErrorClass::PartialUpdate,
            format!(
                "{name} failed and loader.conf could not be put back; recab's state was not updated.\n\
primary error: {cause:#}\nrestore error: {restore:#}"
            ),
        );
    }
    if !firmware.is_empty() {
        return classified(
            ErrorClass::PartialUpdate,
            format!(
                "{name} changed {} but not the rest of the boot selection, and recab's state was not updated: {cause:#}",
                firmware.join(" and ")
            ),
        );
    }
    let entry = if entry { " (the slot's entry options were rewritten)" } else { "" };
    cause.context(format!("{name} aborted; the boot selection is unchanged{entry}"))
}
