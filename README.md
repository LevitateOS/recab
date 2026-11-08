# recab

`recab` manages an A/B system on systemd-boot, with systemd (LevitateOS) or
OpenRC (AcornOS) as init: two root slots, one running.
It sets up the boot layout at install time, writes a new system into the
other slot, boots it once as a trial, keeps it when the trial is healthy,
returns to the previous slot when it is not, and rolls back on request.

It is the slot state machine for LevitateOS's A/B layout, called by people,
installers and scenario harnesses. Requirements: [REQUIREMENTS.md](REQUIREMENTS.md).

## Commands

- `recab init --root <dir> [--mode ab] --slot-a <dev> --slot-b <dev> --kernel <vmlinuz> [--initrd <img>] [--options <kernel options>]`
- `recab init --root <dir> --mode mutable --root-device <dev> --kernel <vmlinuz> [--initrd <img>] [--options <kernel options>]`
- `recab status [--json] [--root <dir>]`
- `recab stage --from-media <device|dir>`, or `recab stage --rootfs <image.erofs|partition> --kernel <vmlinuz> [--initrd <img>]`
- `recab set-next <A|B>`
- `recab commit`
- `recab rollback`
- `recab trial [--timeout <secs>]` (run by the trial service on every boot)
- `recab health [--json] [--timeout <secs>]` (how startup ended: the verdict a trial judges by)
- `recab install-unit [--root <dir>] [--package <systemd|openrc>]`

An update:

```sh
recab stage --from-media /dev/sr0   # the release medium: its rootfs, kernel and installed initramfs
recab set-next B      # one trial boot of the staged slot
reboot                # into B: committed if it comes up healthy, else back to A
recab rollback        # later, if wanted: back to A, while A still holds its system
```

## Behavior

- `init` sets up a freshly installed system's ESP (recab is the only writer of
  it, for both layouts): it copies the target's own systemd-boot
  (`usr/lib/systemd/boot/efi/systemd-boot<arch>.efi`) to `EFI/systemd/` and
  `EFI/BOOT/`, writes an entry per root naming its partition by
  `root=PARTUUID=` with `ro rootwait panic=10` and any `--options`, the kernel
  in `/boot/<id>-a/` (A/B) or `/boot/<id>/` (mutable, entry `<id>.conf`), and
  `loader.conf` defaulting to it (`timeout 5`, `editor 0` when recab creates
  it). For A/B it also writes recab's state with slot A known-good. It points
  systemd-boot's firmware variables at the installed entry, so a default left
  by an earlier install cannot win; where efivars are not available it warns
  on stderr that they could not be reset (`stage` resets them before it writes
  slot B). The prefix and titles come from the target's os-release (`ID`,
  `NAME`); the reader finds the entries by the same rule.
- `status` reports the active and inactive slot, the default, the known-good
  slot, whether the running slot is committed, the rollback target, whether a
  trial is pending, the next boot, and how the last trial ended. It only reads,
  so it also works off UEFI. `slots` lists each slot's entry, kernel
  directory, `root=`, device, whether it can boot and whether it carries the
  trial options. With `--root <dir>` it reads the system installed there (its
  root is the partition mounted at `<dir>`). The default is the `LoaderEntryDefault` EFI
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
  target, removes the slot's kernel directory, formats the partition with the
  filesystem and label recpart's layout gives the slot, extracts the image with
  `recstrap`, and carries over the machine state the new image lists in
  `/usr/lib/recab/carried` (packaged with recab; the list states its format:
  files and directories copied over the image's, account files merged by name).
  The new `fstab` names the slot's root by PARTUUID. `--from-media` mounts a
  release medium read-only and stages its rootfs, kernel and, when it has one,
  installed initramfs.
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
- `health` waits for startup like `trial` and prints the verdict (`--json`:
  `init`, `finished`, `state`, `failed`); it changes nothing and needs no A/B
  layout. It exits non-zero only when no verdict can be formed.
- `install-unit --package <init>` writes recab's package contents into a tree:
  the trial service, enabled (`recab-trial.service` in `sysinit.target`, or
  `/etc/init.d/recab-trial` in the boot runlevel, which runs `recab trial` in
  the background, logging to the kernel log, and stops it at shutdown), and the
  carried-state list. The install-tools recipe packages it so, so every image
  runs the trial service; with no recab state and no one-shot boot it exits at
  once.

### Trial options

While a slot is a trial (scheduled with `set-next`, or staged and not yet
committed), its entry carries the trial options: recinit's fail-reboot word
(`recinit::live::FAIL_REBOOT_OPTION`) when the entry boots through an
initramfs, so the initramfs reboots instead of opening an emergency shell, and
the init's crash option (systemd's `systemd.crash_action=reboot`; OpenRC has
none). Without an initramfs a root that cannot mount and a dying init both
panic, and `panic=10` already reboots. An entry loses them when it becomes
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
the trial service (`systemctl disable recab-trial.service`, or `rc-update del
recab-trial boot`); an uncommitted slot is then left on the next reboot.

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
| `src/init_system.rs` | The init adapter: startup and failures (`health`), reboot, the crash option |
| `src/package.rs`, `src/services/` | The trial service files and the carried-state list recab's package carries |
| `src/carried.rs` | Reading an image's carried-state list |
| `src/error.rs` | Error classes and exit codes |

## Tests

`tests/lifecycle.rs` runs recab against a scratch boot tree: slot entries naming
fake devices, a stand-in `/proc/cmdline`, an efivars directory recab writes
systemd-boot's variables into, a fake `systemctl` whose startup state the test
sets, and a fake OpenRC state directory. No disk is touched. Runs that need root get it in a user namespace
(`unshare --user --map-root-user`), so `cargo test -p recab` runs everything
unprivileged.

## End-to-End Validation

On 2026-11-08, the existing `ab-update` scenario passed across seven boots on
both LevitateOS (systemd) and AcornOS (OpenRC), using fresh A/B installations
and live-tools images under the libvirt user session with OVMF. All eight
scenarios in `test all` passed on each distro.

```sh
cargo xtask test all levitate
cargo xtask test all acorn
cargo xtask test ab-update levitate
cargo xtask test ab-update acorn
```

These commands consume existing release images. A passing result for the same
inputs is reused unless `--force` is supplied. Run records and their input
references are in `.artifacts/out/<distro>/scenarios/<scenario>/<run>/run-manifest.json`.

`ab-update` checks what the host checks cannot: that the installed system boots
from a slot `recab stage` wrote, that OVMF keeps and consumes the one-shot
variable, that `recab-trial.service` reboots out of a degraded trial and commits
a healthy one, that the machine keeps its identity, and that commit and
rollback survive reboots.

The runs exercise the installed initramfs, real efivarfs writes through OVMF,
and each distro's actual service-health reporting. Physical hardware and
injected initramfs or PID 1 failures remain untested by this scenario.

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

Read once per command (`src/target.rs`), to point recab at a scratch tree.
Whether the variables directory is efivarfs is read from the filesystem: a
plain directory holds the variables as plain files.

- `RECAB_ROOT` (default `/`): the running system's root, which `stage` carries the machine's state from
- `RECAB_OS_RELEASE_PATH` (default: the root's `etc/os-release`, else `usr/lib/os-release`; its `ID` names the entries and its `REC_INIT` picks the init system)
- `RECAB_BOOT_ROOT` (default `/boot`)
- `RECAB_STATE_DIR` (default `/var/lib/recab`: `state.json` and `audit.log`)
- `RECAB_LOCK_PATH` (default `/run/lock/recab.lock`)
- `RECAB_PROC_PATH` (default `/proc`: the kernel command line, the mount table, OpenRC's processes)
- `RECAB_SYS_FIRMWARE_EFI_PATH` (default `/sys/firmware/efi`)
- `RECAB_EFIVARS_PATH` (default `/sys/firmware/efi/efivars`)
- `RECAB_STAGE_DIR` (default `/run/recab`, where `stage` mounts the medium, the image and the slot)
- `RECAB_OPENRC_DIR` (default `/run/openrc`) and `RECAB_RUNLEVELS_DIR` (default the root's `etc/runlevels`), read on OpenRC
- `RECAB_SYSTEMCTL_BIN` (default `systemctl`), `RECAB_RC_STATUS_BIN` (default `rc-status`), `RECAB_REBOOT_BIN` (default `reboot`, busybox init's, used on OpenRC), `RECAB_RECSTRAP_BIN` (default `recstrap`)

## Non-Goals

- Not a full updater: it writes a prebuilt image into a slot, but does not build filesystem trees or download anything.
- Not a package manager (that stays in `tools/recipe`).
