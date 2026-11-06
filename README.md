# recab

`recab` manages an A/B system on systemd-boot: two root slots, one running.
It sets up the boot layout at install time, writes a new system into the
other slot, boots it once as a trial, keeps it when the trial is healthy,
returns to the previous slot when it is not, and rolls back on request.

It is the slot state machine for LevitateOS's A/B layout, called by people,
installers and scenario harnesses. Requirements: [REQUIREMENTS.md](REQUIREMENTS.md).

## Commands

- `recab init --root <dir> --slot-a <dev> --slot-b <dev> --kernel <vmlinuz> [--initrd <img>] [--options <kernel options>]`
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

- `init` sets up a freshly installed system (run from the installer, after
  `bootctl install`): an entry per slot naming its partition by
  `root=PARTUUID=` with `panic=10` and any `--options`, slot A's kernel in
  `/boot/<id>-a/`, `loader.conf` defaulting to slot A, recab's state with slot A
  known-good, and `recab-trial.service` enabled. It also points systemd-boot's
  firmware variables at slot A, so a default left by an earlier install cannot
  win. The prefix and titles come from the target's os-release (`ID`, `NAME`).
- `status` reports the active and inactive slot, the default, the known-good
  slot, whether the running slot is committed, the rollback target, whether a
  trial is pending, the next boot, and how the last trial ended. It only reads,
  so it also works off UEFI. The default is the `LoaderEntryDefault` EFI
  variable, else `loader.conf`'s `default`, skipping an entry whose files are
  missing from the ESP (systemd-boot skips it too). A trial is pending while
  `LoaderEntryOneShot` names a bootable slot entry.
- `stage` writes a prebuilt system into the inactive slot. It first checks,
  without changing anything, that the slot's entry names its partition by
  `root=PARTUUID=`, that the partition has a PARTUUID and is not mounted, and
  that the inputs exist. It records the slot as no longer a rollback target,
  then removes the slot's kernel directory, formats the partition (keeping its
  label), extracts the image with `recstrap`, and carries the running system's
  configuration over:
  - hostname, machine-id, locale, time, subuid/subgid and SSH host keys are copied;
  - `passwd`, `group`, `shadow` and `gshadow` are merged by name: the image's
    entries (new system users) plus the running system's, whose lines win;
  - `/etc/systemd/system` and NetworkManager connections are copied over the
    image's, adding what the machine enabled without removing what the image enables;
  - when `recab-trial.service` is enabled, the new image's own recab installs its unit;
  - the new `fstab` names the slot's root by PARTUUID.
  Last it installs the kernel (and initrd) and rewrites the entry's
  `linux`/`initrd` lines, with `panic=10` and the trial options (below). It
  refuses while the running slot is uncommitted (the other slot is then the
  only fallback) and while the slot is the next boot.
- `set-next` sets a one-shot boot of the inactive slot (`bootctl set-oneshot`)
  and adds the trial options to its entry. It refuses the running slot, a slot
  whose entry names files missing from the ESP, and a default that is not the
  known-good slot (`recab commit` repairs that). Repeating it while the trial
  is pending changes nothing.
- `commit` makes the running slot known-good and the default; the slot that was
  known-good before becomes the rollback target. Committing a committed slot
  that is already the default, with no trial pending, changes nothing.
- `rollback` from an uncommitted slot, or with a trial pending, selects the
  known-good slot. From a committed slot it selects the slot that was good
  before it, as long as that slot has not been staged over since.
- A new default (`commit`, `rollback`) first clears a pending one-shot, then
  loses the trial options, then is written to `loader.conf` and the firmware
  variable. recab never selects a slot whose entry cannot boot.
- `trial` waits for startup to finish (`systemctl is-system-running --wait`, at
  most `--timeout`, default 900 s), then judges only the boot `set-next`
  scheduled: a committed slot, a slot picked from the boot menu, or a trial an
  operator already rolled back is left alone. A trial whose startup ended
  `running` is committed. One that ended `degraded` or `maintenance`, or did
  not end in time, is recorded as failed, not committed, and rebooted into the
  known-good slot (exit 23); recab will not reboot toward a known-good slot that
  cannot boot. A startup that ended any other way (a shutdown, an unreachable
  systemd) gets no verdict. When the ESP or `/var` cannot be read, the firmware
  still tells: a failed boot whose `LoaderEntrySelected` is not the default is a
  one-shot trial, and is rebooted.
- `install-unit` writes `recab-trial.service` to `<root>/etc/systemd/system` and
  enables it (`sysinit.target`). `init` runs it.

### Trial options

While a slot is a trial (scheduled with `set-next`, or staged and not yet
committed), its entry carries `rd.emergency=reboot systemd.crash_action=reboot`:
the initramfs reboots instead of opening an emergency shell (recinit's systemd
initramfs honours it), and a crashing PID 1 reboots instead of freezing. An
entry loses them when it becomes the default, so a committed slot that fails
gets a shell to repair it rather than a reboot loop.

### How a failed trial gets back

The one-shot is used up when the trial boots, so any reboot returns to the
known-good default. Something has to cause that reboot:

| Failure | What reboots |
|---|---|
| Units fail, or the system lands in emergency mode | `recab-trial.service` (it starts early and survives isolation) |
| Startup never finishes | `recab trial`'s timeout |
| The initramfs cannot mount the root | `rd.emergency=reboot` (recinit's systemd initramfs) |
| PID 1 crashes | `systemd.crash_action=reboot` |
| Kernel panic | `panic=10` |
| Kernel or PID 1 hangs without crashing | nothing yet: needs a hardware watchdog (`RuntimeWatchdogSec=`) |

A healthy trial is committed automatically. To keep committing by hand, disable
`recab-trial.service`; an uncommitted slot is then left on the next reboot.

## Persistence

The A/B layout mounts its state partition at `/var`, so both slots share recab's state.

- State: `/var/lib/recab/state.json`
- Audit log (JSON lines): `/var/lib/recab/audit.log`
- Lock: `/run/lock/recab.lock`, held by every run that changes anything

Both slots' recab read the same state file, so a newer schema only adds
fields: an older recab keeps the fields it does not know and never lowers the
schema number.

A boot-selection change writes the boot selection first and recab's state
second: after a crash between the two, the machine boots what was asked and
the state is what is stale. `stage` records its slot as no longer a rollback
target before touching it. State, `loader.conf`, the slot entries and kernels
are replaced durably: a temp file is written and flushed, renamed over the
target, and the directory is flushed. (On the FAT ESP the rename itself is not
guaranteed atomic.) A failed audit-log append is reported as a warning.

## Backend Preconditions

Changing the boot selection needs UEFI, efivars and `bootctl`, and fails fast
without them:

- `/sys/firmware/efi`
- `/sys/firmware/efi/efivars`
- `bootctl` (or `RECAB_BOOTCTL_BIN`)

`stage` also uses `blkid`, `mkfs.ext4`, `mount`, `cp`, `chroot` and `recstrap`;
`init` uses `blkid`.

## Security

Every operation that writes (`init`, `stage`, `set-next`, `commit`, `rollback`,
and `trial` when it commits or reboots) requires root.

## Code

| File | Owns |
|---|---|
| `src/state.rs` | The state machine without I/O: what each request changes or why it is refused, and how a trial is judged |
| `src/systemd_boot.rs` | The boot selection: entries, `loader.conf`, EFI variables, `bootctl`, slot kernels, trial options |
| `src/store.rs` | The state file, audit log, lock and durable file replacement |
| `src/init.rs` | The install-time boot layout |
| `src/stage.rs` | Formatting and filling a slot, and carrying configuration |
| `src/trial.rs` | What `trial` asks systemd |
| `src/unit.rs` | `recab-trial.service` |
| `src/error.rs` | Error classes and exit codes |

## Tests

`tests/lifecycle.rs` runs recab against a scratch boot tree: slot entries naming
fake devices, a stand-in `/proc/cmdline`, a fake `bootctl` that writes
systemd-boot's EFI variables, and a fake `systemctl` whose startup state the
test sets. No disk is touched. Runs that need root get it in a user namespace
(`unshare --user --map-root-user`), so `cargo test -p recab` runs everything
unprivileged.

## End-to-End Testing Still Needed

The A/B flow has not run on a booted LevitateOS yet. What is verified:
`tests/lifecycle.rs`, and a manual `recab stage` onto loop-device slots on a
Linux host (including the account and unit merge).

What still needs an e2e run, once Recipe can build the levitate live-tools ISO
(on 2026-11-06 it fails extracting the Fedora RPMs inside Ginux):

```sh
cargo xtask build live-tools levitate
cargo xtask test install levitate     # recab init: entries, slot A kernel, state, recab-trial enabled
cargo xtask test runtime levitate     # recab and recstrap present on the installed system
cargo xtask test ab-update levitate   # stage B; degraded trial reboots back; healthy trial commits itself;
                                      # second-generation stage of A from B; rollback
```

`ab-update` checks what the host checks cannot: that the installed system boots
from a slot `recab stage` wrote, that OVMF keeps and consumes the one-shot
variable, that `recab-trial.service` reboots out of a degraded trial and commits
a healthy one, that the machine keeps its identity, and that commit and
rollback survive reboots.

Also open:
- The installer copies the live ISO's initramfs (`boot/initramfs.img`, busybox,
  searches for the ISO) to the installed system, not `initramfs-installed.img`
  (systemd, mounts `root=`). Installed boots need the latter, and only it
  honours `rd.emergency=reboot`.
- No e2e covers an initramfs or PID 1 failure in a trial.

## Exit Codes

`0` on success, `1` for an unclassified failure. Every classified failure prints
a `[RECAB_ERR_*]` tag and exits with its class's code:

| Code | Tag | Meaning |
|---|---|---|
| 13 | `RECAB_ERR_PERMISSION` | needs root |
| 20 | `RECAB_ERR_BACKEND` | no UEFI, efivars or `bootctl` |
| 21 | `RECAB_ERR_STATE` | refused, or the slots or state could not be read |
| 22 | `RECAB_ERR_PARTIAL_UPDATE` | the boot selection changed only in part, or recab's state was not saved after it changed |
| 23 | `RECAB_ERR_TRIAL_FAILED` | a trial boot was not healthy and was not committed |

They are defined in [`src/error.rs`](src/error.rs).

## Environment Overrides

- `RECAB_BOOT_ROOT` (default `/boot`)
- `RECAB_STATE_PATH` (default `/var/lib/recab/state.json`)
- `RECAB_AUDIT_PATH` (default `/var/lib/recab/audit.log`)
- `RECAB_LOCK_PATH` (default `/run/lock/recab.lock`)
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
