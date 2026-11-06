# recab

`recab` is a small tool/library for managing A/B (slot-based) immutable systems:

- Identify active/inactive slot (`A` vs `B`)
- Stage a trial boot of the inactive slot (try/commit semantics)
- Commit the new slot on successful boot, otherwise allow rollback

This is intended to be the "slot state machine" glue for the repo's homegrown
A/B model, and to be called by higher-level tooling (e.g. installers, `recipe`,
and scenario harnesses).

Status: implemented with a systemd-boot backend and persistent local state.

## Commands

- `recab status`
- `recab status --json`
- `recab set-next <A|B>`
- `recab commit`
- `recab rollback`

## Behavior

- `status` reports active/inactive slot, default slot, known-good slot, trial-pending state, and next boot target.
  It only reads, so it also works off UEFI. The default slot is systemd-boot's `LoaderEntryDefault` EFI variable when set, else the last `default` line of `loader.conf`.
- `set-next` uses `bootctl set-oneshot` to stage a one-shot trial boot of the inactive slot (the running slot is refused).
  One-shot trial plus an already-stable default entry provides bounded-attempt fallback.
  Repeating it for an already staged slot only sets the one-shot entry again.
- `commit` sets the currently active slot as known-good and default (`bootctl set-default` + `loader.conf` update).
- `rollback` sets previously known-good slot as default.
- `commit` and `rollback` clear a pending one-shot entry, so the next boot is the new default.
- `set-next` refuses to run if the default is not aligned to the known-good fallback.
  Run `recab commit` first to repair fallback invariants.

## Persistence

- State: `/var/lib/recab/state.json`
- Audit log (JSON lines): `/var/lib/recab/audit.log`

State and `loader.conf` writes are atomic (`temp file + rename`). If an
operation fails before `bootctl` changed a firmware variable, both files are
restored; a failure after that exits with the partial-update code. A failure to
append the audit log is reported as a warning.

## Backend Preconditions

`recab` targets a `systemd-boot` backend. Mutating operations fail fast unless
host UEFI runtime, efivars and `bootctl` are available:

- `/sys/firmware/efi`
- `/sys/firmware/efi/efivars`
- `bootctl` (or `RECAB_BOOTCTL_BIN`)

## Security

- Mutating operations (`set-next`, `commit`, `rollback`) require root.

## E2E on `/dev/sdc` (Destructive)

`recab` includes ignored integration tests that exercise a full A/B lifecycle
using real `/dev/sdc` partitions and real PARTUUIDs.

- File: `tools/recab/tests/e2e_sdc.rs`
- Scope:
  - repartition `/dev/sdc` as GPT with 2 ext4 partitions (`system-a`, `system-b`)
  - verify `status -> set-next -> commit -> rollback`
  - validate audit/state updates and bootctl command sequencing
- Safety gates:
  - must run as root
  - must set `RECAB_E2E_SDC_ACK=ERASE_SDC`
  - target device must be `/dev/sdc`
  - refuses to run if `/dev/sdc` has mounted filesystems
- Trace output:
  - writes a step-by-step markdown trace (commands, outputs, file snapshots)
  - default path: `/tmp/recab-<test>-<pid>.trace.md`
  - override path with `RECAB_E2E_TRACE_OUT=<path>`

Run:

```bash
cd tools/recab
sudo RECAB_E2E_SDC_ACK=ERASE_SDC cargo test --test e2e_sdc -- --ignored --nocapture
```

## Exit Codes

`0` on success, `1` for an unclassified failure; every classified failure prints a `[RECAB_ERR_*]` tag and exits with its class's code. The classes and codes are defined once, in `ErrorClass` and the `EXIT_CODE_*` constants in [`src/lib.rs`](src/lib.rs).

## Environment Overrides

- `RECAB_BOOT_ROOT` (default `/boot`)
- `RECAB_STATE_PATH` (default `/var/lib/recab/state.json`)
- `RECAB_AUDIT_PATH` (default `/var/lib/recab/audit.log`)
- `RECAB_OS_RELEASE_PATH` (default `/etc/os-release`)
- `RECAB_PROC_CMDLINE_PATH` (default `/proc/cmdline`)
- `RECAB_SYS_FIRMWARE_EFI_PATH` (default `/sys/firmware/efi`)
- `RECAB_EFIVARS_PATH` (default `/sys/firmware/efi/efivars`)
- `RECAB_BOOTCTL_BIN` (default `bootctl`)
- `RECAB_ENTRY_PREFIX` (override inferred `<prefix>-a.conf` / `<prefix>-b.conf`)

## Non-Goals

- Not a full updater (it does not build filesystem trees or download packages).
- Not a package manager (that stays in `tools/recipe`).

## Likely Integrations (TBD)

- `systemd-boot` boot counting / try-boot / commit semantics
- A persistent state file under `/var` (for "slot marked good" + audit metadata)
