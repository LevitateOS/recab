# recab

`recab` manages an A/B system on systemd-boot, with systemd (LevitateOS) or
OpenRC (AcornOS) as init: two root slots, one running.
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
- `recab trial [--timeout <secs>]` (run by the trial service on every boot)
- `recab install-unit [--root <dir>]`

An update:

```sh
recab stage --rootfs /mnt/live/filesystem.erofs --kernel /mnt/boot/vmlinuz --initrd /mnt/boot/initramfs-installed.img
recab set-next B      # one trial boot of the staged slot
reboot                # into B: committed if it comes up healthy, else back to A
recab rollback        # later, if wanted: back to A, while A still holds its system
```

## Behavior

- `init` sets up a freshly installed system (run from the installer, after
  installing systemd-boot): an entry per slot naming its partition by
  `root=PARTUUID=` with `panic=10` and any `--options`, slot A's kernel in
  `/boot/<id>-a/`, `loader.conf` defaulting to slot A, recab's state with slot A
  known-good, and the trial service enabled. It also points systemd-boot's
  firmware variables at slot A, so a default left by an earlier install cannot
  win; where efivars are not available it warns on stderr that they could not
  be reset (`stage` resets them before it writes slot B). The prefix and titles come from the target's os-release (`ID`, `NAME`);
  the init system from the target's distro profile (os-release `REC_INIT`). It creates `loader/` when
  the installer only copied systemd-boot's EFI binary (AcornOS has no `bootctl`).
- `status` reports the active and inactive slot, the default, the known-good
  slot, whether the running slot is committed, the rollback target, whether a
  trial is pending, the next boot, and how the last trial ended. It only reads,
  so it also works off UEFI. The default is the `LoaderEntryDefault` EFI
  variable, else `loader.conf`'s `default`, skipping an entry whose files are
  missing from the ESP (systemd-boot skips it too). A trial is pending while
  `LoaderEntryOneShot` names a bootable slot entry.
- `stage` writes a prebuilt system into the inactive slot. Before it changes
  anything on the slot it checks (refusing with exit 21):
  - the rootfs is a file or block device starting with an EROFS superblock
    (magic `0xE0F5E1E2` at byte 1024), and the kernel and initrd are files;
  - none of the rootfs, kernel and initrd is inside the slot's kernel directory
    (`/boot/<id>-a|b/`), which stage removes first: copy them elsewhere;
  - the slot's entry names its partition by `root=PARTUUID=`, the partition
    has a PARTUUID, is not the running root, and is not mounted;
  - the rootfs is not the slot's partition, a rootfs partition is not mounted,
    and the image (a file's length, or the size a partition's EROFS superblock
    declares) is no larger than the slot's partition. The extracted system is
    larger than the image, so this only catches gross mistakes.

  It needs efivarfs (exit 20 without): a `LoaderEntryDefault` naming another
  entry than the running slot's, or a `LoaderEntryOneShot` naming the slot
  while no trial is pending, is skipped by systemd-boot while the slot has no
  kernel, but would boot the freshly written slot (as the default, untried)
  once it has one. Stage points the default at the running slot's entry and
  removes such a one-shot, which changes nothing systemd-boot boots now, and
  refuses if it cannot. It then records the slot as no longer a rollback
  target, removes the slot's kernel directory, formats the partition (keeping
  its label), extracts the image with `recstrap`, and carries the running system's
  configuration over:
  - hostname, machine-id, locale, time, subuid/subgid and SSH host keys are copied;
  - `passwd`, `group`, `shadow` and `gshadow` are merged by name: the image's
    entries (new system users) plus the running system's, whose lines win;
  - hostname, `/etc/hosts`, and what the init system and privilege tool keep
    (os-release `REC_INIT`, `REC_PRIVILEGE`): on systemd `/etc/systemd/system`
    and NetworkManager connections; on OpenRC `/etc/runlevels`, `/etc/conf.d`,
    `/etc/network/interfaces` and `/etc/inittab`; with doas its configuration. Directories are copied over the image's, adding
    what the machine enabled without removing what the image enables;
  - when the trial service is enabled, the new image's own recab installs it;
  - the new `fstab` names the slot's root by PARTUUID.
  Last it installs the kernel (and initrd) and rewrites the entry's
  `linux`/`initrd` lines, with `panic=10` and the trial options (below). It
  refuses while the running slot is uncommitted (the other slot is then the
  only fallback) and while the slot is the next boot or the default.
- `set-next` sets a one-shot boot of the inactive slot (`LoaderEntryOneShot`)
  and adds the trial options to its entry. A `LoaderEntryDefault` naming
  another entry than the known-good default (one systemd-boot skips) is first
  pointed at it, so a failed trial falls back there. It refuses the running
  slot, a slot whose entry names files missing from the ESP, and a default
  that is not the known-good slot (`recab commit` repairs that). Repeating it while the trial
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
- `trial` waits for startup to finish (at most `--timeout`, default 900 s),
  then judges only the boot `set-next`
  scheduled: a committed slot, a slot picked from the boot menu, or a trial an
  operator already rolled back is left alone. A healthy trial is committed;
  one that failed, or did not finish starting in time, is recorded as failed,
  not committed, and rebooted into the known-good slot (exit 23); recab will
  not reboot toward a known-good slot that cannot boot. A startup cut short by
  a shutdown gets no verdict.
  - systemd: `systemctl is-system-running --wait`; `running` is healthy,
    `degraded` or `maintenance` failed; failed units from `systemctl`.
  - OpenRC: startup has finished when the default runlevel is entered, no
    service is starting and the `openrc` process has exited (OpenRC's state
    directory, `/run/openrc`); a service OpenRC marked failed, a crashed
    daemon (`rc-status --crashed`) or the single-user runlevel is a failure. When the ESP or `/var` cannot be read, the firmware
  still tells: a failed boot whose `LoaderEntrySelected` is not the default is a
  one-shot trial, and is rebooted.
- `install-unit` writes the trial service for the init system under `<root>`
  and enables it: `recab-trial.service` in `sysinit.target`, or
  `/etc/init.d/recab-trial` in the boot runlevel (it runs `recab trial` in the
  background, logging to the kernel log). Recipe packages this service in the system image; `init` and `stage` only enable it. `install-unit` remains an explicit repair command.

### Trial options

While a slot is a trial (scheduled with `set-next`, or staged and not yet
committed), its entry carries the init system's trial options. On systemd they
are `rd.emergency=reboot systemd.crash_action=reboot`: the initramfs reboots
instead of opening an emergency shell (recinit's systemd initramfs honours it),
and a crashing PID 1 reboots instead of freezing. OpenRC systems boot without an
initramfs, so a root that cannot mount and a dying init both panic, and
`panic=10` already reboots; they need none. An entry loses them when it becomes
the default, so a committed slot that fails gets a shell to repair it rather
than a reboot loop.

### How a failed trial gets back

The one-shot is used up when the trial boots, so any reboot returns to the
known-good default. Something has to cause that reboot:

| Failure | What reboots |
|---|---|
| Units or services fail, emergency mode, single-user runlevel | the trial service (on systemd it starts early and survives isolation) |
| Startup never finishes | `recab trial`'s timeout |
| The root cannot be mounted | systemd: `rd.emergency=reboot` (recinit's systemd initramfs); OpenRC, no initramfs: the kernel panics, `panic=10` |
| PID 1 crashes | systemd: `systemd.crash_action=reboot`; OpenRC: the kernel panics, `panic=10` |
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

Changing the boot selection, and `stage`, need UEFI with efivarfs, and fail
fast without:

- `/sys/firmware/efi`
- `/sys/firmware/efi/efivars`

recab writes systemd-boot's variables there itself (clearing the immutable flag
efivarfs sets, one write per variable), as `bootctl` would, so it needs no
`bootctl`: AcornOS installs only systemd-boot's EFI binary.

`stage` also uses `blkid`, `mkfs.ext4` and `mount` (util-linux, e2fsprogs),
`chroot` and `recstrap`; it copies files itself, so busybox systems work. `init`
uses `blkid`.

## Security

Every operation that writes (`init`, `stage`, `set-next`, `commit`, `rollback`,
and `trial` when it commits or reboots) requires root.

## Code

| File | Owns |
|---|---|
| `src/state.rs` | The state machine without I/O: what each request changes or why it is refused, and how a trial is judged |
| `src/systemd_boot.rs` | The boot selection: entries, `loader.conf`, EFI variables, slot kernels, trial options on entries |
| `src/store.rs` | The state file, audit log, lock and durable file replacement |
| `src/init.rs` | The install-time boot layout |
| `src/stage.rs` | Formatting and filling a slot, and carrying configuration |
| `src/init_system.rs` | What differs between systemd and OpenRC: startup and failures, reboot, the trial service, trial options, carried configuration |
| `src/error.rs` | Error classes and exit codes |

## Tests

`tests/lifecycle.rs` runs recab against a scratch boot tree: slot entries naming
fake devices, a stand-in `/proc/cmdline`, an efivars directory recab writes
systemd-boot's variables into, a fake `systemctl` whose startup state the test
sets, and a fake OpenRC state directory. No disk is touched. Runs that need root get it in a user namespace
(`unshare --user --map-root-user`), so `cargo test -p recab` runs everything
unprivileged.

## End-to-End Testing Still Needed

The A/B flow has not run on a booted LevitateOS yet. What is verified:
`tests/lifecycle.rs`, and a manual `recab stage` onto loop-device slots on a
Linux host (including the account and unit merge).

What still needs an e2e run, for each distro, once Recipe can build its
live-tools ISO (on 2026-11-06 levitate's fails extracting the Fedora RPMs inside
Ginux; acorn's has not been tried):

```sh
cargo xtask build live-tools levitate   # or acorn
cargo xtask test install levitate       # recab init: entries, slot A kernel, state, trial service enabled
cargo xtask test runtime levitate       # recab and recstrap present on the installed system
cargo xtask test ab-update levitate     # stage B; degraded trial reboots back; healthy trial commits itself;
                                        # second-generation stage of A from B; rollback
```

Not yet seen on real hardware or OVMF: recab's own writes to efivarfs (the
lifecycle tests write a plain directory), and OpenRC's state directory and
`rc-status --crashed` output on a real AcornOS boot.

`ab-update` checks what the host checks cannot: that the installed system boots
from a slot `recab stage` wrote, that OVMF keeps and consumes the one-shot
variable, that `recab-trial.service` reboots out of a degraded trial and commits
a healthy one, that the machine keeps its identity, and that commit and
rollback survive reboots.

Also open:
- The release now puts the installed system's initramfs on the ISO
  (`boot/initramfs-installed.img`: systemd, mounts `root=`, honours
  `rd.emergency=reboot`) and recsetup installs it instead of the live one; no
  e2e has booted it yet.
- No e2e covers an initramfs or PID 1 failure in a trial.

## Exit Codes

`0` on success, `1` for an unclassified failure. Every classified failure prints
a `[RECAB_ERR_*]` tag and exits with its class's code:

| Code | Tag | Meaning |
|---|---|---|
| 13 | `RECAB_ERR_PERMISSION` | needs root |
| 20 | `RECAB_ERR_BACKEND` | no UEFI or efivars, or the running os-release has no distro profile (`REC_*`) |
| 21 | `RECAB_ERR_STATE` | refused, or the slots or state could not be read |
| 22 | `RECAB_ERR_PARTIAL_UPDATE` | the boot selection changed only in part, or recab's state was not saved after it changed |
| 23 | `RECAB_ERR_TRIAL_FAILED` | a trial boot was not healthy and was not committed |

They are defined in [`src/error.rs`](src/error.rs).

## Environment Overrides

- `RECAB_BOOT_ROOT` (default `/boot`)
- `RECAB_STATE_PATH` (default `/var/lib/recab/state.json`)
- `RECAB_AUDIT_PATH` (default `/var/lib/recab/audit.log`)
- `RECAB_LOCK_PATH` (default `/run/lock/recab.lock`)
- `RECAB_OS_RELEASE_PATH` (default `/etc/os-release`; its `ID` names the entries and its `REC_*` distro profile picks the init system and privilege tool)
- `RECAB_PROC_CMDLINE_PATH` (default `/proc/cmdline`)
- `RECAB_SYS_FIRMWARE_EFI_PATH` (default `/sys/firmware/efi`)
- `RECAB_EFIVARS_PATH` (default `/sys/firmware/efi/efivars`)
- `RECAB_SYSTEMCTL_BIN` (default `systemctl`, used by `trial` on systemd)
- `RECAB_OPENRC_DIR` (default `/run/openrc`, used by `trial` on OpenRC)
- `RECAB_RC_STATUS_BIN` (default `rc-status`, used by `trial` on OpenRC)
- `RECAB_RUNLEVELS_DIR` (default `/etc/runlevels`, used by `trial` on OpenRC: every service of the sysinit, boot and default runlevels must be started or inactive)
- `RECAB_REBOOT_BIN` (default: `systemctl reboot`, or OpenRC's `openrc-shutdown -r now` / `reboot`)
- `RECAB_RECSTRAP_BIN` (default `recstrap`, used by `stage`)
- `RECAB_ENTRY_PREFIX` (override inferred `<prefix>-a.conf` / `<prefix>-b.conf`)

## Non-Goals

- Not a full updater: it writes a prebuilt image into a slot, but does not build filesystem trees or download anything.
- Not a package manager (that stays in `tools/recipe`).
