# recab

`recab` manages an A/B system on systemd-boot: two root slots, one running.
It writes a new system into the other slot, boots it once as a trial, keeps it
when the trial is healthy, returns to the previous slot when it is not, and
rolls back on request.

It is the slot state machine for LevitateOS's A/B layout, called by people,
installers and scenario harnesses. Requirements: [REQUIREMENTS.md](REQUIREMENTS.md).

## Commands

- `recab status [--json]`
- `recab stage --rootfs <image.erofs|partition> --kernel <vmlinuz> [--initrd <img>]`
- `recab set-next <A|B>`
- `recab commit`
- `recab rollback`
- `recab trial [--timeout <secs>]` (run by `recab-trial.service`)
- `recab install-unit [--root <dir>]`

An update:

```sh
recab stage --rootfs /mnt/live/filesystem.erofs --kernel /mnt/boot/vmlinuz --initrd /mnt/boot/initramfs.img
recab set-next B      # one trial boot of the staged slot
reboot                # into B: committed if it comes up healthy, else back to A
recab rollback        # later, if wanted: back to A, while A still holds its system
```

## Behavior

- `status` reports the active and inactive slot, the default, the known-good slot,
  whether the running slot is committed, the rollback target, whether a trial is
  pending, and the next boot. It only reads, so it also works off UEFI.
  The default is systemd-boot's `LoaderEntryDefault` EFI variable when set, else
  the last `default` line of `loader.conf`. A trial is pending while
  `LoaderEntryOneShot` names a slot entry; the firmware removes it when it boots it.
- `stage` writes a prebuilt system into the inactive slot. The slot's partition is
  the one its entry's `root=` names (both entries are written at install time). It
  removes the slot's kernel directory (`/boot/<prefix>-<a|b>/`), formats the
  partition (keeping its label), extracts the image with `recstrap`, carries the
  running system's identity, accounts, locale and time, enabled units, network
  connections and SSH host keys over, points the new `fstab` root at the slot,
  then installs the kernel (and initrd) and rewrites the entry's `linux`/`initrd`
  lines, adding `panic=10` to its options if they have no `panic=`. It refuses while
  the running slot is uncommitted (the other slot is then the only fallback) and
  while a trial of the slot is pending.
- `set-next` sets a one-shot boot of the inactive slot (`bootctl set-oneshot`). It
  refuses the running slot, a slot whose entry names files missing from the ESP,
  and a default that is not the known-good slot (a failed trial would not fall
  back to it; `recab commit` repairs that). Repeating it while the trial is
  pending changes nothing.
- `commit` makes the running slot known-good and the default; the slot that was
  known-good before becomes the rollback target. Committing a committed slot that
  is already the default, with no trial pending, changes nothing.
- `rollback` from an uncommitted slot, or with a trial pending, selects the
  known-good slot. From a committed slot it selects the slot that was good before
  it, as long as that slot has not been staged over since.
- `commit` and `rollback` clear a pending one-shot, so the next boot is the default.
- `trial` waits for startup to finish (`systemctl is-system-running --wait`, at
  most `--timeout`, default 900 s). On a committed slot it does nothing. On a
  trial boot it commits when startup ended `running`; when it ended any other way
  (`degraded`, `maintenance`) or timed out, it does not commit, makes the
  known-good slot the next boot, reboots, and exits 23.
- `install-unit` writes `recab-trial.service` to `<root>/etc/systemd/system` and
  enables it (`sysinit.target`). The installer runs it for A/B installs; `stage`
  refreshes the unit in the new slot when the running system has it.

### How a failed trial gets back

The one-shot is used up when the trial boots, so any reboot returns to the
known-good default. Something has to cause that reboot:

| Failure | What reboots |
|---|---|
| Units fail, or the system lands in emergency mode | `recab-trial.service` (it starts early and survives isolation) |
| Startup never finishes | `recab trial`'s timeout |
| Kernel panic | `panic=10` on the slot's entry |
| Kernel or PID 1 hangs | nothing yet: needs a hardware watchdog (`RuntimeWatchdogSec=`) |

A healthy trial is committed automatically. To keep committing by hand, disable
`recab-trial.service`; an uncommitted slot is then left on the next reboot.

## Persistence

The A/B layout mounts its state partition at `/var`, so both slots share recab's state.

- State: `/var/lib/recab/state.json`
- Audit log (JSON lines): `/var/lib/recab/audit.log`

State, `loader.conf`, the slot's entry and its kernel are replaced durably: a
temp file is written and flushed, renamed over the target, and the directory is
flushed. (On the FAT ESP the rename itself is not guaranteed atomic.) If an
operation fails before `bootctl` changed a firmware variable, the state file and
`loader.conf` are restored; a failure after that exits with the partial-update
code. A failure to append the audit log is reported as a warning.

## Backend Preconditions

Changing the boot selection needs UEFI, efivars and `bootctl`, and fails fast
without them:

- `/sys/firmware/efi`
- `/sys/firmware/efi/efivars`
- `bootctl` (or `RECAB_BOOTCTL_BIN`)

`stage` also uses `blkid`, `mkfs.ext4`, `mount`, `cp` and `recstrap`.

## Security

Every operation that writes (`stage`, `set-next`, `commit`, `rollback`, and
`trial` when it commits or reboots) requires root.

## Code

| File | Owns |
|---|---|
| `src/state.rs` | The state machine without I/O: what each request changes, or why it is refused |
| `src/systemd_boot.rs` | The boot selection: entries, `loader.conf`, EFI variables, `bootctl`, slot kernels |
| `src/store.rs` | The state file, audit log and durable file replacement |
| `src/stage.rs` | Formatting and filling a slot |
| `src/trial.rs` | What `trial` asks systemd |
| `src/unit.rs` | `recab-trial.service` |
| `src/error.rs` | Error classes and exit codes |

## Tests

`tests/lifecycle.rs` runs recab against a scratch boot tree: slot entries naming
fake devices, a stand-in `/proc/cmdline`, a fake `bootctl` that writes
systemd-boot's EFI variables, and a fake `systemctl` whose startup state the
test sets. No disk is touched. `status`, `trial` on a committed slot,
`install-unit` and the root check run unprivileged; the rest need root
(`--include-ignored`).

## End-to-End Testing Still Needed

The A/B flow has not run on a booted LevitateOS yet. What is verified:
`tests/lifecycle.rs` and a manual `recab stage` onto loop-device slots on a
Linux host.

What still needs an e2e run, once Recipe can build the levitate live-tools ISO
(on 2026-11-06 it fails extracting the Fedora RPMs inside Ginux):

```sh
cargo xtask build live-tools levitate
cargo xtask test install levitate     # A/B install: per-slot kernels, both entries, STATE at /var, recab-trial enabled
cargo xtask test runtime levitate     # recab and recstrap present on the installed system
cargo xtask test ab-update levitate   # stage B; degraded trial reboots back; healthy trial commits itself; rollback
```

`ab-update` checks what the host checks cannot: that the installed system boots
from a slot `recab stage` wrote, that OVMF keeps and consumes the one-shot
variable, that `recab-trial.service` reboots out of a degraded trial and commits a
healthy one, and that commit and rollback survive reboots.

## Exit Codes

`0` on success, `1` for an unclassified failure. Every classified failure prints
a `[RECAB_ERR_*]` tag and exits with its class's code:

| Code | Tag | Meaning |
|---|---|---|
| 13 | `RECAB_ERR_PERMISSION` | needs root |
| 20 | `RECAB_ERR_BACKEND` | no UEFI, efivars or `bootctl` |
| 21 | `RECAB_ERR_STATE` | refused, or the slots or state could not be read |
| 22 | `RECAB_ERR_PARTIAL_UPDATE` | a firmware variable changed, then the operation failed |
| 23 | `RECAB_ERR_TRIAL_FAILED` | a trial boot was not healthy and was not committed |

They are defined in [`src/error.rs`](src/error.rs).

## Environment Overrides

- `RECAB_BOOT_ROOT` (default `/boot`)
- `RECAB_STATE_PATH` (default `/var/lib/recab/state.json`)
- `RECAB_AUDIT_PATH` (default `/var/lib/recab/audit.log`)
- `RECAB_OS_RELEASE_PATH` (default `/etc/os-release`)
- `RECAB_PROC_CMDLINE_PATH` (default `/proc/cmdline`)
- `RECAB_SYS_FIRMWARE_EFI_PATH` (default `/sys/firmware/efi`)
- `RECAB_EFIVARS_PATH` (default `/sys/firmware/efi/efivars`)
- `RECAB_BOOTCTL_BIN` (default `bootctl`)
- `RECAB_SYSTEMCTL_BIN` (default `systemctl`, used by `trial`)
- `RECAB_RECSTRAP_BIN` (default `recstrap`, used by `stage`)
- `RECAB_ENTRY_PREFIX` (override inferred `<prefix>-a.conf` / `<prefix>-b.conf`)

## Non-Goals

- Not a full updater: it writes a prebuilt image into a slot, but does not build filesystem trees or download anything.
- Not a package manager (that stays in `tools/recipe`).
