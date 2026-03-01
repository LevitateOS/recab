# recab

`recab` is a small tool/library for managing A/B (slot-based) immutable systems:

- Identify active/inactive slot (`A` vs `B`)
- Stage a trial boot of the inactive slot (try/commit semantics)
- Commit the new slot on successful boot, otherwise allow rollback

This is intended to be the "slot state machine" glue for the repo's homegrown
A/B model, and to be called by higher-level tooling (e.g. installers, `recipe`,
and stage harnesses).

Status: implemented with a systemd-boot backend and persistent local state.

## Commands

- `recab status`
- `recab status --json`
- `recab set-next <A|B>`
- `recab commit`
- `recab rollback`

## Behavior

- `status` reports active/inactive slot, default slot, known-good slot, trial-pending state, and next boot target.
- `set-next` uses `bootctl set-oneshot` to stage a one-shot trial boot.
- `commit` sets the currently active slot as known-good and default (`bootctl set-default` + `loader.conf` update).
- `rollback` sets previously known-good slot as default.

## Persistence

- State: `/var/lib/recab/state.json`
- Audit log (JSON lines): `/var/lib/recab/audit.log`

State writes are atomic (`temp file + rename`).

## Security

- Mutating operations (`set-next`, `commit`, `rollback`) require root.

## Environment Overrides

- `RECAB_BOOT_ROOT` (default `/boot`)
- `RECAB_STATE_PATH` (default `/var/lib/recab/state.json`)
- `RECAB_AUDIT_PATH` (default `/var/lib/recab/audit.log`)
- `RECAB_OS_RELEASE_PATH` (default `/etc/os-release`)
- `RECAB_PROC_CMDLINE_PATH` (default `/proc/cmdline`)
- `RECAB_BOOTCTL_BIN` (default `bootctl`)
- `RECAB_ENTRY_PREFIX` (override inferred `<prefix>-a.conf` / `<prefix>-b.conf`)

## Non-Goals

- Not a full updater (it does not build filesystem trees or download packages).
- Not a package manager (that stays in `tools/recipe`).

## Likely Integrations (TBD)

- `systemd-boot` boot counting / try-boot / commit semantics
- A persistent state file under `/var` (for "slot marked good" + audit metadata)
