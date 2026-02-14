//! recab - A/B slot management primitives.
//!
//! This crate is intentionally small and is expected to evolve alongside the
//! repo's A/B implementation.

use anyhow::Result;

/// System slot identifier (A/B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
}

/// A/B state query.
///
/// Note: v0 is a placeholder; real implementation will likely query:
/// - bootloader metadata (systemd-boot try/commit)
/// - persistent state under /var (audit + "marked good")
pub fn status() -> Result<()> {
    Ok(())
}

