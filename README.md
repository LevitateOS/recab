# recab

`recab` is a small tool/library for managing A/B (slot-based) immutable systems:

- Identify active/inactive slot (`A` vs `B`)
- Stage a trial boot of the inactive slot (try/commit semantics)
- Commit the new slot on successful boot, otherwise allow rollback

This is intended to be the "slot state machine" glue for the repo's homegrown
A/B model, and to be called by higher-level tooling (e.g. installers, `recipe`,
and checkpoint harnesses).

Status: scaffold only (implementation TBD).

## Non-Goals

- Not a full updater (it does not build filesystem trees or download packages).
- Not a package manager (that stays in `tools/recipe`).

## Likely Integrations (TBD)

- `systemd-boot` boot counting / try-boot / commit semantics
- A persistent state file under `/var` (for "slot marked good" + audit metadata)

