# recab

`recab` owns system updates and A/B continuation for LevitateOS and AcornOS.
It prepares the next system in the inactive slot, coordinates system changes
and downloaded updates through Recipe, trial-boots the result, commits a healthy
system and returns to the previous system when the trial fails. It uses
systemd-boot with systemd (LevitateOS) or OpenRC (AcornOS).

The [implementation plan](IMPLEMENTATION.md) defines the storage, preparation,
Recipe integration, update merge, installation and recovery work needed to
deliver this lifecycle.

## Required update lifecycle

1. Installation populates A and B with the same system and machine configuration,
   including both slots' kernels and initramfs images. Partition references,
   boot paths and slot identity differ; system contents are equivalent. Both
   slots must be bootable at first use, after personalization is complete.
2. The user boots committed A. Its system root is immutable. To make system
   change X, recab prepares writable B as a continuation of A.
3. Recipe applies X to B. If a system update becomes available before reboot,
   recab coordinates its discovery, download and application to that same B.
   The update preserves X or reports a conflict; it does not erase pending work.
4. Recab finalizes B and its matching boot artifacts before scheduling a trial.
   A remains the protected fallback. A healthy boot commits B; a failed trial
   returns to A once the failure causes a reboot.
5. To make change Y while running committed B, recab synchronizes inactive A
   to B's current system using only the differences, then applies Y to A.
   X and the update are therefore retained. The same lifecycle repeats A→B→A.

A and B have the same capabilities. Active, inactive, prepared and known-good
are roles, not permanent properties of a slot letter. Preparation reuses the
inactive filesystem and avoids rewriting unchanged system data. Synchronization
must include deletions, metadata and links, exclude shared data and runtime
mounts, and preserve the destination's slot identity. A fresh install's matching
slots avoid an unnecessary full-system rewrite on the first continuation.

User data and caches may be shared on `/var`; system package selection and
configuration must belong to the system being prepared. Once preparation starts
overwriting the previous slot, that older rollback is gone: the committed running
slot is the fallback. Never overwrite that fallback while running an uncommitted
trial. Requirements: [REQUIREMENTS.md](REQUIREMENTS.md).

## Implementation status

The current implementation has release-image staging, trial boot, commit and
rollback. It does **not yet implement the full lifecycle above**:

- `init` prepares A and writes both boot entries, but leaves B unpopulated and
  without its own kernel. Identical initial slots are still required.
- `stage` reformats the inactive slot and carries selected configuration from
  the running slot. It cannot continue a system incrementally or preserve X
  already prepared in the inactive slot when applying an update.
- Generation inventories, durable preparation identity/revisions and readiness
  checks now exist. `prepare` records intent and invalidates the candidate entry;
  incremental synchronization (P03) and writable continuation editing are still
  missing. It does not yet clone the active system.
- Update discovery/download, conflict handling and Recipe integration with an
  inactive system remain missing. `stage` refuses an existing preparation so it
  cannot silently discard X.
- Root's current Recipe profile is shared on `/var`; this is not a substitute
  for package selection tied to a prepared system.
- Installed immutable boot protects the active root device and keeps ordinary
  `/etc` on that read-only system. Legacy writable configuration overlays require
  explicit adoption; they are never silently hidden. Runtime configuration writes
  still need the P04 migration and boot validation on both distributions.

These are implementation gaps in recab's update responsibility, not exclusions
from its purpose. The commands below describe the existing implementation.

## Current commands

- `recab init --root <dir> [--mode ab] --slot-a <dev> --slot-b <dev> --kernel <vmlinuz> --initrd <img> [--options <kernel options>]`
- `recab init --root <dir> --mode mutable --root-device <dev> --kernel <vmlinuz> [--initrd <img>] [--options <kernel options>]`
- `recab status [--json] [--root <dir>]`
- `recab prepare [--preparation <id> --revision <n>] [--json]`
- `recab discard --preparation <id> --revision <n>`
- `recab inventory --root <dir> [--json]`
- `recab stage --from-media <device|dir>`, or `recab stage --rootfs <image.erofs|partition> --kernel <vmlinuz> --initrd <img> [--change-flavour]`
- `recab set-next <A|B>`
- `recab commit`
- `recab rollback`
- `recab trial [--timeout <secs>]` (run by the trial service on every boot)
- `recab health [--json] [--timeout <secs>]` (how startup ended: the verdict a trial judges by)
- `recab baseline --root <dir>`
- `recab install-unit [--root <dir>] --init <systemd|openrc> [--bin <path>]`

A release-image update with today's destructive staging path, starting in A
(use A as the trial target instead when running B). This replaces the inactive
system. It refuses to run while a preparation exists:

```sh
recab stage --from-media /dev/sr0   # the release medium: its rootfs, kernel and installed initramfs
recab set-next B      # one trial boot of the staged slot
reboot                # into B: committed if it comes up healthy, else back to A
recab rollback        # later, if wanted: back to A, while A still holds its system
```

## Current command behavior

- `prepare` requires the committed active generation to be read-only, match its
  inventory and boot artifacts, and be the default with no pending trial. It
  records a random preparation ID, source generation, revision and durable intent
  before removing the inactive boot entry. The inactive slot becomes incomplete
  and stops being a rollback target. At this implementation step it reports
  `awaiting_synchronization`; **it has not copied a continuation**. P03 will add
  synchronization. Repeating the command preserves its ID and payloads and can
  finish interrupted invalidation. Supply both ID and revision to reject stale
  requests. Reopening a completed candidate preserves its contents while making
  it incomplete again.
- `discard` explicitly abandons the matching ID/revision and removes its entry.
  It does not restore an older rollback or claim the remaining payload is ready.
- `inventory` reads the complete system tree using path bytes, content digests,
  ownership/mode, xattrs, link relationships, modification times and sparse
  extents. It excludes shared/runtime mount contents and its own generation
  marker; it never includes regular file bodies. Only recab-owned slot fields
  and their publication timestamps are normalized in generation identity.

- `init` sets up a freshly installed system's ESP (recab is the only writer of
  it, for both layouts): it copies the target's own systemd-boot
  (`usr/lib/systemd/boot/efi/systemd-boot<arch>.efi`) to `EFI/systemd/` and
  `EFI/BOOT/`, writes an entry per root naming its partition by
  `root=PARTUUID=` with `ro rootwait panic=10` and any `--options`, the kernel
  in `/boot/<id>-a/` (A/B) or `/boot/<id>/` (mutable, entry `<id>.conf`), and
  `loader.conf` defaulting to it (`timeout 5`, `editor 0` when recab creates
  it). For A/B it also prepares A's immutable boot configuration and writes
  recab's state with slot A known-good; it does not yet populate B. It points
  systemd-boot's firmware variables at the installed entry, so a default left
  by an earlier install cannot win; where efivars are not available it warns
  on stderr that they could not be reset (`stage` resets them before writing
  the inactive slot). When installing from a running recab A/B system, its
  host firmware variables are left alone. The prefix and titles come from the target's os-release (`ID`,
  `NAME`); the reader finds the entries by the same rule.
- `status` reports the active and inactive slot, the default, the known-good
  slot, whether the running slot is committed, the rollback target, whether a
  trial is pending, the next boot, and how the last trial ended. It only reads,
  so it also works off UEFI. `slots` lists each slot's entry, kernel
  directory, `root=`, device, whether it can boot and whether it carries the
  trial options. Status schema 3 also reports generation identities, readiness,
  preparation progress and a recovery reason. `bootable` checks ESP-file presence;
  `generation_ready` additionally requires a complete recorded generation and
  matching boot-artifact digests. Boot scheduling requires that stronger check. With `--root <dir>` it reads the system installed there (its
  root is the partition mounted at `<dir>`). The default is the `LoaderEntryDefault` EFI
  variable, else `loader.conf`'s `default`, skipping an entry whose files are
  missing from the ESP (systemd-boot skips it too). A trial is pending while
  `LoaderEntryOneShot` names a bootable slot entry.
- `stage` writes a prebuilt system into the inactive slot. Before it changes
  anything on the slot it checks (refusing with exit 21):
  - the rootfs is an `.erofs` file or block device with an EROFS superblock
    (magic `0xE0F5E1E2` at byte 1024), and the kernel and initrd are files;
  - the initramfs supports immutable boot, `/var` is a separate ext4 state
    partition, and the image matches the running distro and init system
    (and flavour unless `--change-flavour` is given);
  - none of the rootfs, kernel and initrd is inside the slot's kernel directory
    (`/boot/<id>-a|b/`), which stage removes first: copy them elsewhere;
  - the independent slot mapping names its partition by PARTUUID, the partition
    has a PARTUUID, is not the running root, and is not mounted;
  - the rootfs is not the slot's partition, a rootfs partition is not mounted,
    and the compressed image fits; expanded files, carried configuration,
    filesystem overhead margin and inode demand must also fit;
  - carried-path parents/types, complete metadata baseline, account collisions,
    fstab and immutable layout are valid. Reads and writes stay within opened
    directory descriptors; absolute image links cannot resolve against the host.

  It needs efivarfs (exit 20 without): a `LoaderEntryDefault` naming another
  entry than the running slot's, or a `LoaderEntryOneShot` naming the slot
  while no trial is pending, is skipped by systemd-boot while the slot has no
  kernel, but would boot the freshly written slot (as the default, untried)
  once it has one. After input checks, stage records the slot as no longer a
  rollback target, points a stale default at the running slot and removes such
  a one-shot before removing the slot's kernel directory. It aborts if those
  firmware changes fail. It then formats the partition with the
  filesystem and label recpart's layout gives the slot, extracts the image with
  `recstrap`, and carries over the machine state the new image lists in
  `/usr/lib/recab/carried` (packaged with recab). A baseline records the original
  image's selected paths; changed files win over the release's whole file, with
  no three-way conflict report. The transitional baseline detects content, mode,
  owner, link and xattr changes/removals. Account carry checks numeric identities
  across the complete set and refuses ambiguous additions/deletions, identity
  migration and differing membership until structured merging arrives in P09.
  Rewritten account files retain the image's ownership and xattrs. This remains
  selected configuration carry, not preservation of all system changes.
  The new `fstab` names the slot's root by PARTUUID. `--from-media` mounts a
  release medium read-only and requires its rootfs, kernel and installed
  initramfs. A/B staging requires an initramfs supporting immutable boot.
  Last it installs the kernel (and initrd) and rewrites the entry's
  `linux`/`initrd` lines, with `panic=10` and the trial options (below). It
  refuses while the running slot is uncommitted (the other slot is then the
  only fallback) and while the slot is the next boot or the default. Some
  predictable checks run before invalidation. Unexpected extraction, I/O or
  publication failures can still leave the inactive slot incomplete and unavailable.
- `set-next` sets a one-shot boot of the inactive slot (`LoaderEntryOneShot`)
  and adds the trial options to its entry. A `LoaderEntryDefault` naming
  another entry than the known-good default (one systemd-boot skips) is first
  pointed at it, so a failed trial falls back there. It refuses the running
  slot, an incomplete generation, stale or mismatched boot artifacts, and a default
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
  variable. recab refuses entries whose referenced ESP files are missing.
- `trial` waits for startup to finish (`--timeout`, default 900 s),
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
  one-shot trial, and is rebooted. Health-collection errors can currently return
  before recovery, and helper subprocesses are not all bounded by the timeout.
- `health` waits for startup like `trial` and prints the verdict (`--json`:
  `schema_version`, `init`, `startup`, `finished`, `state`, `failed`); it changes nothing and needs no A/B
  layout. It exits non-zero only when no verdict can be formed.
- `baseline --root <dir>` records the extracted image's carried paths before
  personalization. It is a configuration baseline, not a whole-system delta.
- `install-unit --init <init>` writes recab's package contents into a tree:
  the trial service, enabled (`recab-trial.service` in `sysinit.target`, or
  `/etc/init.d/recab-trial` in the boot runlevel, which runs `recab trial` in
  the background, logging to the kernel log, and stops it at shutdown), and the
  carried-state list. The install-tools recipe packages it so, so every image
  runs the trial service. The service waits for startup health before deciding
  whether this boot is a trial.

### Trial options

While a slot is a trial (scheduled with `set-next`, or staged and not yet
committed), its entry carries the trial options: recinit's fail-reboot word
(`recinit::live::FAIL_REBOOT_OPTION`) when the entry boots through an
initramfs, so the initramfs reboots instead of opening an emergency shell, and
the init's crash option (systemd's `systemd.crash_action=reboot`; OpenRC has
none). A/B boot requires the installed-system initramfs on both distros.
An entry loses the trial-specific options when it becomes
the default, so a committed slot that fails gets a shell to repair it rather
than a reboot loop.

### How a failed trial gets back

The one-shot is used up when the trial boots, so any reboot returns to the
known-good default. Something has to cause that reboot:

| Failure | What reboots |
|---|---|
| Units or services fail, emergency mode, single-user runlevel | the trial service (on systemd it starts early and survives isolation) |
| Startup never finishes | `recab trial`'s timeout |
| The root cannot be mounted | `rd.emergency=reboot` in recinit's installed initramfs, on both distros |
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

Both slots share state format `{"major":2,"minor":0}`. The structured major
version intentionally cannot be read by the old numeric-schema writer, which
would otherwise ignore new safety restrictions. Numeric schema 1 is inspectable
but read-only; adoption with compatible runtimes is still required. Unsupported
versions and inconsistent generation records refuse mutation with a recovery
reason. Status stays bounded and carries IDs and summaries, not system file bodies.

Partition mappings survive boot-entry removal. Each complete slot records its
generation and kernel/initramfs digests; the boot command line identifies the
booted generation, and pending trials bind to that generation rather than only
its reusable slot letter. Inventories live under `/var/lib/recab/inventories/`;
the system's slot/generation marker is `/usr/lib/recab/generation.json`.

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
`prepare`, `discard`, and `trial` when it commits or reboots) requires root.

## Code

| File | Owns |
|---|---|
| `src/state.rs` | The state machine without I/O: what each request changes or why it is refused, and how a trial is judged |
| `src/systemd_boot.rs` | The boot selection: entries, `loader.conf`, EFI variables, slot kernels, trial options on entries |
| `src/store.rs` | The state file, audit log, lock and durable file replacement |
| `src/init.rs` | The current install-time boot layout; matching initial slot population is missing |
| `src/stage.rs` | The current destructive release-image update path |
| `src/immutable.rs` | Read-only system configuration and immutable-initramfs compatibility |
| `src/filesystem.rs` | Directory-FD-confined filesystem access and complete xattrs |
| `src/generation.rs` | Deterministic system inventories, generation identity and slot markers |
| `src/prepare.rs` | Preparation identity, entry invalidation, interruption and explicit discard |
| `src/init_system.rs` | The init adapter: startup and failures (`health`), reboot, the crash option |
| `src/package.rs`, `src/services/` | The trial service files and the carried-state list recab's package carries |
| `src/carry.rs` | The carried-state list, configuration baseline, copying and account merge |
| `src/error.rs` | Error classes and exit codes |

## Tests

`tests/lifecycle.rs` runs recab against a scratch boot tree: slot entries naming
fake devices, a stand-in `/proc/cmdline`, an efivars directory recab writes
systemd-boot's variables into, a fake `systemctl` whose startup state the test
sets, and a fake OpenRC state directory. No disk is touched. Runs that need root get it in a user namespace
(`unshare --user --map-root-user`). Some fixtures still encode the old empty-B
installation, use legacy state and lack the initramfs now required by this checkout. These checks
do not establish the required continuation lifecycle.

## End-to-End Validation

The previously recorded release-image `ab-update` run on 2026-11-08 passed across seven boots on
both LevitateOS (systemd) and AcornOS (OpenRC), using fresh A/B installations
and live-tools images under the libvirt user session with OVMF. All eight
scenarios in `test all` passed on each distro. This is historical evidence for
those artifacts, not validation of the current working tree or the missing
continuation, incremental synchronization and matching initial slots.

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

## Ownership

Recab owns the system update lifecycle: obtaining updates, preparing and
continuing a system, applying updates without losing pending changes, trial
boot, commit and rollback. Recipe owns software acquisition, builds, installation,
update and removal inside Ginux; recab coordinates those operations for its
prepared system. Installers call recab to establish the initial A/B systems.
Frontends expose recab's state and operations instead of implementing another
updater. Missing implementation belongs in these existing owners.
