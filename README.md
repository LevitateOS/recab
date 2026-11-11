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

The CLI, TUI and existing desktop interfaces implement the lifecycle above,
including symmetric preparation, Recipe changes, release merging and trial
recovery. Focused native filesystem checks establish the behavior described
below; full installed boots and production deployment remain unverified:

- `init` now populates B from the personalized A, verifies normalized equivalence,
  seals both roots and publishes both generation-bound entries. A starts
  committed/default; B is ready without a fabricated successful trial.
- `stage` now imports a verified release and uses the same preserving merge as
  `update apply`. It reuses the inactive filesystem and preparation, then
  finalizes only a conflict-free candidate. Raw inputs require their manifest.
- `prepare` now synchronizes the inactive ext4 filesystem in place in either
  direction, with whole-system inventories, range writes, durable preparation
  identity/revisions and recovery. Reopening preserves pending changes.
- Synchronization finishes in `prepared`, while the slot remains incomplete for
  boot. `finalize` now validates and seals it, installs reusable boot artifacts,
  then publishes its entry. Recipe now applies software/configuration changes
  to that preparation. Release merging now preserves X or reports conflicts.
- Signed update discovery/download now run through Recipe, with pinned releases,
  bounded networking, verified caching and interruption recovery. Offline apply
  merges the pinned release against its retained baseline and prepared changes.
  Conflicts, incomplete reproductions and partial merges block finalization.
- Root's Recipe install/update/remove commands on immutable A/B installs delegate
  to recab. Package selection and `/usr/lib/recipe/profile` belong to each slot;
  independent per-user profiles and output-only builders retain their own routes.
- Installed immutable boot protects the active root device and keeps ordinary
  `/etc` on that read-only system. Nonempty legacy writable configuration overlays are refused; this release
  provides no adoption command. The package/finalizer now
  installs explicit runtime policy for mount tables, DNS, systemd first-boot
  writers and OpenRC clock updates. Actual installed boots on both distributions
  remain unverified with these changes.

Remaining work is full lifecycle boot validation on the supported architectures
and distribution stacks, plus configuring and publishing a production update
origin. These limits do not change recab's responsibility for system updates.

## Current commands

- `recab init --root <dir> [--mode ab] --slot-a <dev> --slot-b <dev> --kernel <vmlinuz> --initrd <img> [--options <kernel options>]`
- `recab init --root <dir> --mode mutable --root-device <dev> --kernel <vmlinuz> [--initrd <img>] [--options <kernel options>]`
- `recab status [--json] [--root <dir>]`
- `recab prepare [--preparation <id> --revision <n>] [--json]`
- `recab finalize [--preparation <id> --revision <n>] [--kernel <vmlinuz> --initrd <img>] [--change-flavour] [--json]`
- `recab discard --preparation <id> --revision <n>`
- `recab inventory --root <dir> [--json]`
- `recab compare [--json] [--machine-events | --roots <running> <other>]`
- `recab history [--json]`
- `recab update status|check`
- `recab update download [<manifest-sha256>]`
- `recab update apply <manifest-sha256> [--preparation <id> --revision <n>]`
- `recab update apply --manifest <release.json> [--trusted-local] [--preparation <id> --revision <n>]`
- `recab update cancel --preparation <id> --revision <n>`
- `recab release import <release.json> [--trusted-local] [--root <dir>]`
- `recab stage --manifest <release.json> [--trusted-local]`
- `recab stage --from-media <device|dir> [--trusted-local]`, or `recab stage --rootfs <image.erofs> --kernel <vmlinuz> --initrd <img> [--change-flavour]`
- `recab set-next <A|B>`
- `recab commit`
- `recab rollback`
- `recab trial [--timeout <secs>]` (run by the trial service on every boot)
- `recab health [--json] [--timeout <secs>]` (how startup ended: the verdict a trial judges by)
- `recab baseline --root <dir>`
- `recab install-unit [--root <dir>] --init <systemd|openrc> [--bin <path>]`

A verified media update, starting in A (use A as the trial target when running
B). Existing prepared changes participate in the merge; conflicts must be resolved
before finalization and trial selection:

```sh
recab stage --from-media /dev/sr0   # verifies its release.json and complete bound set
recab set-next B      # one trial boot of the staged slot
reboot                # into B: committed if it comes up healthy, else back to A
recab rollback        # later, if wanted: back to A, while A still holds its system
```

## Current command behavior

- `prepare` requires the committed active generation to be read-only, match its
  inventory and boot artifacts, and be the default with no pending trial. It
  checks the inactive partition's identity, mount status, available bytes and
  inodes read-only before saving intent and removing its boot entry. It then
  mounts only that inactive ext4 partition writable in a private namespace,
  synchronizes it without formatting, verifies the resulting inventory, flushes
  and unmounts it, and saves phase `prepared`. The old rollback ends when intent
  is saved; the committed active system remains protected.

  File writes compare fixed aligned 1 MiB ranges and write only differing data
  ranges, including a partial final range. New files stream in; sparse holes,
  deletions, hardlink groups (including symlinks), ownership, modes, ACLs,
  capabilities and xattr removals are preserved. Directory metadata is restored
  after child operations. A mutation detaches an inode when aliases outside the
  intended group could otherwise modify excluded data or cached publications.
  Unexpected mounts (including same-device binds), devices, FIFOs and sockets
  in system content are refused. Shared/runtime and ESP contents are excluded.

  Source root references and immutable configuration are materialized with the
  destination's slot/PARTUUID. The journal records only changed operations and
  affected parents, not an expanded third root. After interruption, retry
  re-inventories actual contents and re-compares ranges; completed-step counters
  do not authorize skipping data checks. Supply both preparation ID and revision
  to reject stale requests. Once synchronized, reopening preserves its ID and
  pending changes and does not reseed it. No writable mount is left open between
  commands. `prepared` is not boot-ready: run `recab finalize` before selection.

  `synchronization` in the preparation reports the last completed attempt's
  logical payload bytes/range writes, comparisons, hole punching, creations,
  removals, metadata operations, marker bytes and serialized plan bytes.
  Filesystem metadata/journal bytes and other recab state bytes are `null`
  (unmeasured), not zero. These counters are not SSD SMART totals. After a killed
  attempt, `accounting_complete=false`: earlier payload writes are unknown.
- `finalize` works identically for B from A and A from B. It requires a
  synchronized continuation and checks identity/profile, executable init and its
  libraries, account consistency, fstab, runtime compatibility, optional
  generation-owned Recipe receipts, and the complete installed initramfs plus
  kernel/module binding. It does not execute services from the inactive root.
  Machine identity and enabled SSH keys must already be seeded by installation.

  Omit the boot-input options to reuse the active boot set. Supply both for an
  updated kernel/initramfs; a changed kernel/module set requires the release
  producer's `usr/lib/recab/kernel.json` binding. The candidate must include a
  recab binary matching its producer's `usr/lib/recab/runtime.json`, supporting
  state 2.5 and immutable boot protocol v3. The retained fallback must have a
  compatible reader and installed boot protocol too. This release targets fresh
  paired installations; legacy adoption and a two-cycle runtime bootstrap are
  not implemented. Use compatible recovery media or a fresh installation.

  Durable intent and absent entry precede writable access. Runtime policy,
  inventory and root records are flushed; boot objects and manifest are made
  durable; the root is unmounted and its device protected read-only; then the
  slot entry is published last. Hash-named kernel/initramfs objects on the ESP
  are reused in either direction. Collection keeps both slots and foreign entry
  references. Retry reconciles a frozen finalization; use `prepare` to reopen it
  before making further changes. Failed finalization stays incomplete.

  JSON reports boot payload and manifest/entry bytes separately, reuse count and
  whether prior interrupted writes are unknown. These exclude filesystem and
  controller writes. `set-next`, commit and rollback recheck the root generation,
  boot record, manifest and artifacts before changing selection. The initramfs
  checks `recab.slot`, root PARTUUID and `recab.generation` against the sealed
  root before starting init, and protects both root devices.
- `status --json` (schema 10) also says where the two systems stand in one
  word, so front ends need not work it out from slots and phases:
  `lifecycle.state` is `stable`, `open`, `working`, `interrupted`, `ready`,
  `restart_set`, `on_trial`, `trial_failed` or `unusable`; `lifecycle.other`
  says what the other system is (`copy`, `previous`, `next`, `unknown`);
  `lifecycle.working` is whether a recab command holds the run lock right now
  (null when the reader may not open the lock file), which tells work in
  progress from an interruption. `trial_failed` comes from the last judged
  trial, which names a slot and not a generation.
- `preparation.changes` lists every change finished in the preparation, oldest
  first: its action, package or path, time, and `actor`, the label the caller
  gave with the global `--actor LABEL` (for example `person` or `agent:codex`).
  The label is for the person reading; it identifies nobody and grants nothing.
- `discard` explicitly abandons the matching ID/revision and removes its entry.
  It does not restore an older rollback or claim the remaining payload is ready.
- `compare` reads the running system and the other slot and sets them side by
  side: files and bytes on each side, what is the same, every folder down to
  three names below `/` with all it holds, and each difference as
  `only-running`, `only-other`, `content` or `metadata` (a one-sided folder
  stands for all it holds; at most 5000 are listed, the rest counted in
  `more`). `etc/fstab` and `usr/lib/recab/immutable` name their own slot and
  are reported under `own`, not as differences. It changes neither system: it
  takes the run lock, mounts the other slot read-only without replaying its
  journal, and unmounts it. `--machine-events` reports `plan`, `step` and
  `done` as JSON lines on stderr; there is no byte progress yet. It uses the
  same inventory as `prepare`, so it refuses what that refuses (an unexpected
  mount inside a system, a special file). `--roots` compares two trees that
  are mounted already and takes no lock; this is the only form that has been
  run. The two-slot form has never run on an A/B system.
- `inventory` reads the complete system tree using path bytes, content digests,
  ownership/mode, xattrs, link relationships, modification times and sparse
  extents. It excludes shared/runtime mount contents and its own generation
  marker; it never includes regular file bodies. Only recab-owned slot fields
  and their publication timestamps are normalized in generation identity.

- A/B `init` requires the personalized A mounted at `--root`, separate ext4
  state at `var/`, a vfat ESP at `boot/`, and an unmounted ext4 B. It never
  formats B. It preserves the original image baseline, requires seeded machine
  identity and enabled SSH keys, and copies the completed system with the same
  inventory/range synchronizer used by `prepare`. Shared homes/state and nested
  runtime/ESP contents are excluded. Root's initial home is published into shared
  state before either system can boot.

  A and B receive their own fstab/slot/root markers and the same normalized
  generation. Matching kernel/initramfs objects are stored once on the ESP.
  Both roots are flushed and protected read-only before entries are published.
  A is committed/default; B is ready and trial-capable at first use. `init`
  reports success only after both entries, the ledger and completion record are
  durable. Mutable initialization retains its single-root layout.

  `/var/lib/recab/installation.json` pins partition identities, boot inputs,
  options and the frozen personalized generation. Retry the same initialization
  after interruption; it checks actual data and reuses completed ranges and
  boot objects. A changed source or live generation ledger is refused, never
  reseeded. Incomplete installation blocks ordinary mutating recab commands.
  A completed retry revalidates both roots and entries. Installing from an
  existing A/B host preserves that host's firmware selection; unavailable EFI
  variables produce an explicit warning. Entry names come from os-release ID.
- `status` reports the active and inactive slot, the default, the known-good
  slot, whether the running slot is committed, the rollback target, whether a
  trial is pending, the next boot, and how the last trial ended. It only reads,
  so it also works off UEFI. `slots` lists each slot's entry, kernel
  directory, `root=`, device, whether it can boot and whether it carries the
  trial options. Status schema 6 also reports generation identities, readiness,
  preparation progress and a recovery reason. `bootable` checks ESP-file presence;
  `generation_ready` additionally requires a complete recorded generation and
  matching boot-artifact digests. Boot scheduling requires that stronger check. With `--root <dir>` it reads the system installed there (its
  root is the partition mounted at `<dir>`). The default is the `LoaderEntryDefault` EFI
  variable, else `loader.conf`'s `default`, skipping an entry whose files are
  missing from the ESP (systemd-boot skips it too). A trial is pending while
  `LoaderEntryOneShot` names a bootable slot entry.
- `stage` verifies its complete release set, creates/reopens the inactive
  continuation and runs the same O/C/N merge as `update apply`. It does not format
  or erase a preparation. Conflicts remain inspectable through `update conflicts`;
  resolving them applies the captured plan, after which `finalize` publishes it.
  A conflict-free stage finalizes automatically. Media stays read-only. Raw
  rootfs/kernel/initramfs inputs must match the corresponding signed manifest;
  use `--manifest --trusted-local` explicitly for unsigned development media.
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
    daemon (`rc-status --crashed`) or the single-user runlevel is a failure.
    Inactive one-shot services count as successful.
  - The deadline covers the complete assessment, including failed-unit and
    crashed-daemon helpers and descendants holding output pipes open. Helpers
    are terminated and reaped on timeout, cancellation or collection error.
    An explicit shutdown gets no verdict; an unreachable init or failed probe
    fails a confirmed trial.
  - Before selecting the one-shot, recab saves a generation-bound recovery ticket
    in the `RecabTrial` EFI variable (vendor GUID
    `a69e7a1c-44ab-42f5-93ed-836a4c103bad`). It binds the candidate and fallback
    partitions, generations and boot seals, independently of shared `/var`.
    Both generation identities and the unique trial attempt also appear in new
    `last_trial` records. A failed assessment is checkpointed on `/var` even
    when EFI writes fail; a later explicit retry gets its own attempt identity.
    An already verified firmware fallback needs no EFI rewrite, so read-only
    firmware does not block that safe reboot.
  - A missing or unreadable ledger cannot authorize a commit. Recovery uses the
    ticket, checked running command line and firmware selection, and verifies
    the fallback root and available ESP payloads. With the ESP unmounted, only
    the ticket's unchanged firmware default and a consumed one-shot authorize
    recovery. No state is created through an unmounted `/var` directory.
  - If a required EFI selection fails or the fallback is unavailable/corrupt,
    recab reports a recovery error and retains the ticket without claiming a reboot. A failure
    stays failed until the fallback runs. That boot reconciles its durable
    verdict; commit retries after partial writes remain bound to the same
    generation. Preparation is blocked until an outstanding ticket is resolved.
- `health` waits for startup like `trial` and prints the verdict (`--json`:
  `schema_version`, `init`, `startup`, `finished`, `state`, `failed`); it changes nothing and needs no A/B
  layout. It exits non-zero only when no verdict can be formed.
- `history` lists what happened to this computer's systems, newest first, and
  changes nothing. It reads only the audit log and the state file, and needs
  root only if those are unreadable to the caller (`audit.log` is written with
  the default mode, so a person normally reads it). Each entry has a `time`, a
  `kind` (the audit log's operation names, plus `change` for a finished change
  and `trial-kept` for a commit that ended a healthy trial), the `slot` and
  `preparation` it concerns where the record shows them, and for a change its
  `action`, `package` and `actor` (`--actor` is kept only with a change, so no
  other entry has one). `outcome` is `started`, `done` or `interrupted` for
  prepare, update, finalize and discard, and `detail` is recab's own text where
  the record holds it (how startup ended, why an operation stopped). The end of
  `prepare` is not audited, so its entry says only that it began or stopped.
  `records`, `unreadable`, `truncated` and `log_found` say how complete the
  reading was. Point it at another state folder with `RECAB_STATE_DIR`.
- `baseline --root <dir>` records the complete original release inventory and
  account fields before personalization, in `usr/lib/recab/release-baseline.json`.
  Setup retries retain it; generation-marked systems cannot invent an original
  baseline. Missing release policy leaves no baseline; a later release merge
  reports that constraint and requires the verified original release.
- `install-unit --init <init>` writes recab's package contents into a tree:
  the trial service, enabled (`recab-trial.service` in `sysinit.target`, or
  `/etc/init.d/recab-trial` in the boot runlevel, which runs `recab trial` in
  the background, logging to the kernel log, and stops it at shutdown), immutable
  runtime policy and an executable-bound compatibility record. No carried-path
  list or legacy configuration baseline is produced. Every image with recab
  runs the trial service. The service waits for startup health before deciding
  whether this boot is a trial.

### Trial options

While a slot is scheduled/running as a trial through `set-next`, its entry carries the trial options: recinit's fail-reboot word
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
| Startup or an assessment helper never finishes | `recab trial`'s complete-assessment timeout |
| Health collection fails in a confirmed trial | the trial service, after verifying its generation-bound fallback |
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

Fresh installs write state format `{"major":2,"minor":5}` and immutable boot
protocol 3 in both personalized systems. Mutations require a completed paired
installation record and matching boot seals for every ready/committed generation.
Numeric schema 1 and structured 2.0–2.4 remain inspectable but read-only; they
are never promoted by a command. Unsupported versions and inconsistent records
refuse mutation with a recovery reason. There is no legacy adoption/bootstrap
command: use compatible recovery media or a fresh installation. Do not edit the
state version, patch the running immutable root, or replace the only known-good
fallback to force an upgrade. Older structured writers refuse minor 5. The
structured major cannot be parsed by the old numeric reader. Both bootable runtimes must enforce the state and boot protocol.
Status stays bounded and carries IDs and summaries, not system file bodies.

Partition mappings survive boot-entry removal. Each complete slot records its
generation and kernel/initramfs digests; the boot command line identifies the
booted generation, and pending trials bind to that generation rather than only
its reusable slot letter. Inventories live under `/var/lib/recab/inventories/`;
the system's slot/generation marker is `/usr/lib/recab/generation.json`.
An unfinished synchronization has a protected `synchronize-<preparation>.json`
operation plan in `/var/lib/recab/`. It is removed after successful preparation or
discard; a remaining plan triggers conservative interrupted-write accounting.

A boot-selection change writes the boot selection first and recab's state
second: after a crash between the two, the machine boots what was asked and
the state is what is stale. `stage` records its slot as no longer a rollback
target before touching it. State, `loader.conf`, the slot entries and kernels
are replaced durably: a temp file is written and flushed, renamed over the
target, and the directory is flushed. (On the FAT ESP the rename itself is not
guaranteed atomic.) A failed audit-log append is reported as a warning.

## Backend Preconditions

Changing the boot selection, `prepare`, and `stage` need UEFI with efivarfs, and fail
fast without:

- `/sys/firmware/efi`
- `/sys/firmware/efi/efivars`

recab writes systemd-boot's variables there itself (clearing the immutable flag
efivarfs sets, one write per variable), as `bootctl` would, so it needs no
`bootctl`: AcornOS installs only systemd-boot's EFI binary.

`stage` uses the same `blkid`/`mount`/`umount` and confined range writer as
preparation; it does not run mkfs, chroot or recstrap. `init` uses `blkid`. `prepare` uses `blkid`, `mount` and `umount`, requires mount
namespace/device access, and refuses an already mounted inactive partition.

## Security

Every operation that writes (`init`, `stage`, `set-next`, `commit`, `rollback`,
`prepare`, `finalize`, `discard`, and `trial` when it commits or reboots) requires root.

## Code

| File | Owns |
|---|---|
| `src/state.rs` | The state machine without I/O: what each request changes or why it is refused, and how a trial is judged |
| `src/systemd_boot.rs` | The boot selection: entries, `loader.conf`, EFI variables, slot kernels, trial options on entries |
| `src/store.rs` | The state file, audit log, lock and durable file replacement |
| `src/history.rs` | `history`: the audit log and state file read as a list of what happened |
| `src/init.rs`, `src/installation.rs` | Single-root boot setup and recoverable equivalent A/B installation |
| `src/stage.rs` | Manifest/media adapter to the preserving release merge |
| `src/merge.rs`, `src/merge_accounts.rs` | Frozen O/C/N plans, structured accounts, conflicts, resolution and recovery |
| `src/immutable.rs` | Read-only system configuration and immutable-initramfs compatibility |
| `src/filesystem.rs` | Directory-FD-confined filesystem access and complete xattrs |
| `src/generation.rs` | Deterministic system inventories, generation identity and slot markers |
| `src/finalize.rs` | Validation/sealing transaction, frozen recovery, root/entry verification |
| `src/candidate.rs`, `src/elf.rs` | Profile, accounts, fstab, runtime and executable/kernel/module checks |
| `src/runtime.rs` | Explicit immutable runtime policy and binary compatibility binding |
| `src/prepare.rs` | Preparation identity, synchronization, interruption and explicit discard |
| `src/sync.rs` | Delta operation planning, capacity, range writes, sparse files, links and metadata |
| `src/slot_mount.rs` | Checked inactive ext4 mounts and device protection restoration |
| `src/init_system.rs` | The init adapter: startup and failures (`health`), reboot, the crash option |
| `src/package.rs`, `src/services/` | Enabled trial service, immutable runtime policy and reader compatibility |
| `src/fstab.rs` | Slot-specific root-reference materialization |
| `src/error.rs` | Error classes and exit codes |

## Tests

The native CLI checks use disposable GPT/ext4/vfat disks and existing kernel
artifacts. `tests/finalization.py` covers sealing/publication and both directions;
`--lifecycle`, `--trials`, `--installation` and `--deployment` select the relevant
focused integration. The old mock-only lifecycle suite and pre-protocol P03
fixture were retired; they cannot represent a current paired installation.

For the deployment refusals, from the superproject root:

```sh
sudo python3 tools/recab/tests/finalization.py --binary target/debug/recab \
  --kernel <existing-UEFI-kernel> --module <its-module.ko> --deployment
```

This checks old state formats, missing installation records/seals, old protocols,
legacy entries and corrupted committed boot objects. Refusals leave the ledger,
ESP, firmware selection and running root unchanged in both directions. Current
sealed pairs still schedule and cancel trials. The fixture supplies observed
boot records; only actual boot scenarios establish PID 1 and service behavior.

## Release manifest and verified inputs

The builder and recab now share the typed `distro-contract::release` format.
Media and online bundles use one `release.json`, binding distro, architecture,
init/libc/flavour, channel/sequence/version, source provenance, intended system
inventory, kernel/module release, exact artifact hashes/sizes and minimum
recab/state/boot compatibility. See the
[shared format and trust rules](../../distro/contract/docs/releases.md).

```sh
recab release import /mnt/release/release.json --trusted-local
# Import verifies/caches inputs while an existing preparation still holds X.
# It does not apply the release, reset the preparation or change boot selection.
```

`--trusted-local` explicitly permits unsigned local input only when no
installed distribution key is configured. With a key, a detached Ed25519
signature of the exact manifest bytes is mandatory. Trust comes from
`/usr/lib/recab/update-source.json`, never the incoming release. The current
variant defaults have no production endpoint/key. Online discovery/acquisition is
implemented, including baseline-aware merging of the prepared continuation.

Import verifies identity/compatibility, copies and hashes the exact inputs into
a private read-only set, mounts its actual EROFS image and checks the complete
intended inventory, runtime, ELF architecture/libc, kernel/module binding and
full installed initramfs. The verified set remains frozen through use, so
replacing a mutable source pathname cannot change the consumed bytes. Import
moves it into a content-addressed cache without a second full-payload copy;
existing cache bytes are rechecked. It does not mutate a candidate or ledger.

`stage --manifest PATH` and `stage --from-media MEDIA` use the same verifier
before invalidating the candidate. Media must carry the release manifest;
matching filenames alone no longer establish a release set. For unsigned
local/development media add `--trusted-local` explicitly. These staging commands
use the preserving merge and accept an existing preparation. Raw
`--rootfs/--kernel/--initrd` inputs must match their corresponding manifest;
unbound raw images are refused.

The focused fixture extends `tests/finalization.py` with
`--release-builder /absolute/path/to/distro-builder`. Build only those CLI/library
targets first and set `RECAB_TEST_RUSTC` to the absolute compiler binary (from
`rustup which rustc`) when running under sudo. It produces tiny native LZ4 EROFS
images using the real image writer and existing kernel/module artifacts; tests
real signatures, byte binding and refusal without changing A, B or pending X.
It is not a full OS build or a kernel/VM boot.

## Prepared system changes

On an immutable A/B installation, root's `recipe install`, `update`, and
`remove` route through recab. `--system` selects this route explicitly.
Recipe builds isolated payloads inside Ginux; it never receives a writable slot,
the running system as a whole input, the ESP or shared `/var`.

```sh
recipe --system install service
recipe --system installed                 # active and prepared receipts
recipe --system isinstalled service --prepared
recab edit /etc/example.conf               # editor works on a private bounded copy
recab edit /etc/example.conf --file ./new.conf
recipe --system update service
recipe --system remove service
recab finalize
recab set-next B                           # use A when running B
```

`recab change install|update|remove RECIPE` exposes the same application owner.
Producer options follow `--`; `--producer-recipe PATH` selects the current source
for an update. Recipe's frontend resolves the current catalog first and falls
back to the selected source when no catalog entry exists. Optional
`--preparation ID --revision N` guards stale requests. Changes are pending reboot;
they do not activate a shared root profile. `--package-root DIR` selects an
independent profile; `--no-activate` retains output-only production.

Explicit `system` directory outputs require `target_finalize` metadata and use
root-relative paths. Ordinary `package` outputs install in the candidate's
`/usr/lib/recipe/profile`. The slot-owned `/usr/lib/recipe/system.json` records
producer digest, architecture, scoped target, runtime dependencies and owned
metadata/content. Unowned base/admin file replacement, incompatible coownership,
and overwrite/removal of administrator edits refuse before publication.
Directories already present retain their metadata; removal retains directories
and files owned by remaining selections. Shared/runtime and recab identity paths
are excluded. An edit preserves original ownership, permissions and xattrs and does not silently
change its package fingerprint, so later overwrite/removal detects that edit.
Edits are limited to existing regular files of at most 1 MiB.

A durable publication intent invalidates the candidate and advances its revision.
After interruption, `recab change recover` reconciles the same frozen selected
payload and receipt; other edits/finalization refuse until recovery or explicit
discard. Failed producers publish no candidate changes. The prepared system is
unmounted between commands; finalization validates and seals it for trial boot.

`last_change` and a pending `change` first appeared in status schema 6 / state 2.3.
Reports separate logical candidate payload writes from package projection bytes
and `/proc/self/io` deltas for Recipe and the recab invocation (including reaped children and streams). Returned invocation counters cover cleanup; the persisted record predates that observation and leaves it unmeasured.
The latter includes runtime input staging, checkpoints, exports and diagnostics;
it is not physical SSD traffic or a SMART count. Interrupted attempts explicitly
report incomplete accounting. Compiler runtime extraction is still included in
producer I/O; no claim of zero total invocation writes is made.

For focused real Recipe/Ginux checks, pass `--changes /absolute/path/to/recipe`
to `tests/finalization.py` with a compatible `GINUX_BIN`, plus its existing `--binary`, `--kernel`
and `--module` arguments. It uses disposable ext4/vfat loop partitions and actual
Ginux producers using a tiny native worker/loader runtime; boot/firmware observations remain structural fixtures.

## End-to-End Validation

The previously recorded release-image `ab-update` run on 2026-11-08 passed across seven boots on
both LevitateOS (systemd) and AcornOS (OpenRC), using fresh A/B installations
and live-tools images under the libvirt user session with OVMF. All eight
scenarios in `test all` passed on each distro. This is historical evidence for
those artifacts. It does not validate the current continuation, synchronization,
matching initial slots, release merge or generation-bound trial recovery.

```sh
cargo xtask test all levitate
cargo xtask test all acorn
cargo xtask test ab-update levitate
cargo xtask test ab-update acorn
```

These commands consume existing release images. A passing result for the same
inputs is reused unless `--force` is supplied. Run records and their input
references are in `.artifacts/out/<distro>/scenarios/<scenario>/<run>/run-manifest.json`.

The existing `ab-update` scenario still uses the legacy staging flow and needs
updating before it can validate the current lifecycle. Required boot evidence
includes actual slot roots, OVMF one-shot consumption, both init systems' trial
services, the installed initramfs and retained identity across reboot. Native
filesystem checks cannot establish those behaviors, physical firmware support
or recovery from hardware hangs.

Pass `--trials` to `tests/finalization.py` with its `--binary`, `--kernel` and
`--module` arguments, and `--init systemd` or `--init openrc`, for focused trial
checks on disposable GPT/ext4/vfat partitions. It covers both directions,
health helpers and cancellation, missing state/ESP, failed EFI writes,
partial-commit reconciliation, invalid fallbacks and generation reuse refusal.
Combining it with `--updates` and `--release-builder` exercises the merged
release candidate. These checks use observed boot/init records; they do not
boot a VM or write physical firmware variables.

## Lifecycle interfaces

`recab status --json` schema 10 supplies `operations`: labels, verdicts, exact
argument vectors, input fields and destructive-action markers. The CLI, TUI and
System desktop use the same candidate/fallback decisions in either direction.
`ready` permits requesting an operation; execution rechecks filesystem and
firmware facts. No frontend infers readiness from the presence of kernel files.

Commands bind to the preparation ID/revision and selected generation shown in
status. `--preparation none --revision 0` explicitly requires no open session;
it refuses if another caller has prepared work since the screen was read.
Finalized candidates can be reopened for more changes without reseeding X.
A failed generation without its preparation record offers an explicit destructive
replacement, instead of silently copying over its remaining changes.

`lifecycle` reports conflicts, interruption, working state, current-generation
trial results and whether the selected next boot changes the running system.
When the candidate is running on trial, the other system is the protected previous
one. Status includes candidate/running release names, completed and interrupted
changes, download counters and redacted merge conflicts. Unreadable update
metadata is reported, never silently presented as an empty update list.

`--machine-events` emits operation `started`, `finished` or `failed` JSON lines
on stderr. Stdout remains the command's result; existing Recipe/compare event
streams retain their own schemas. Boundary events do not invent percentages.
The generated event and status schemas/types are the interface contract.

The TUI lists all offered operations, gathers their declared inputs and refreshes
after execution. System's existing pages retain their workflow and add an
expandable software/update/recovery section. Conflict resolutions are submitted
one at a time against the current revision. Neither interface discards a
preparation to apply another change or release. A distribution update source
must still be configured before online checking is available.

Before A/B installer completion, `recab verify-installation --root TARGET`
checks the durable initial publication, both identical personalized root
inventories and generation records, reader compatibility and both sealed boot
entries/payloads. The install hub records that proof and verifies again before
unmounting; a legacy “boot done” flag alone cannot complete an A/B installation.

## Exit Codes

`0` on success, `1` for an unclassified failure. Every classified failure prints
a `[RECAB_ERR_*]` tag and exits with its class's code:

| Code | Tag | Meaning |
|---|---|---|
| 13 | `RECAB_ERR_PERMISSION` | needs root |
| 20 | `RECAB_ERR_BACKEND` | no UEFI or efivars, or the running os-release has no distro profile (`REC_*`) |
| 21 | `RECAB_ERR_STATE` | refused, or the slots or state could not be read |
| 22 | `RECAB_ERR_PARTIAL_UPDATE` | the boot selection changed only in part, or recab's state was not saved after it changed |
| 23 | `RECAB_ERR_TRIAL_FAILED` | trial assessment/commit failed, or recovery needs attention |

They are defined in [`src/error.rs`](src/error.rs).

## Environment Overrides

Read once per command (`src/target.rs`), to point recab at a scratch tree.
Whether the variables directory is efivarfs is read from the filesystem: a
plain directory holds the variables as plain files.

- `RECAB_ROOT` (default `/`): the running system's protected source root
- `RECAB_OS_RELEASE_PATH` (default: the root's `etc/os-release`, else `usr/lib/os-release`; its `ID` names the entries and its `REC_INIT` picks the init system)
- `RECAB_BOOT_ROOT` (default `/boot`)
- `RECAB_STATE_DIR` (default `/var/lib/recab`: `state.json` and `audit.log`)
- `RECAB_LOCK_PATH` (default `/run/lock/recab.lock`)
- `RECAB_PROC_PATH` (default `/proc`: the kernel command line, the mount table, OpenRC's processes)
- `RECAB_SYS_FIRMWARE_EFI_PATH` (default `/sys/firmware/efi`)
- `RECAB_EFIVARS_PATH` (default `/sys/firmware/efi/efivars`)
- `RECAB_STAGE_DIR` (default `/run/recab`, where `stage` mounts the medium, the image and the slot)
- `RECAB_OPENRC_DIR` (default `/run/openrc`) and `RECAB_RUNLEVELS_DIR` (default the root's `etc/runlevels`), read on OpenRC
- `RECAB_SYSTEMCTL_BIN` (default `systemctl`), `RECAB_RC_STATUS_BIN` (default `rc-status`), `RECAB_REBOOT_BIN` (default `reboot`, busybox init's, used on OpenRC)

## Ownership

Recab owns the system update lifecycle: obtaining updates, preparing and
continuing a system, applying updates without losing pending changes, trial
boot, commit and rollback. Recipe owns software acquisition, builds, installation,
update and removal inside Ginux; recab coordinates those operations for its
prepared system. Installers call recab to establish the initial A/B systems.
Frontends expose recab's state and operations instead of implementing another
updater. Missing implementation belongs in these existing owners.

The focused P04 check is `tests/finalization.py --binary <recab> --kernel
<existing-UEFI-kernel> --module <matching-ko> --init <systemd|openrc>` (root/loop
access required). It uses real ext4 roots and a vfat ESP and exercises interrupted
intent, publication failure/retry, sealing, A→B→A selection/commit/rollback, boot
record/artifact tampering refusal, preservation of local X, safe collection with
foreign boot references, and zero repeated boot payload writes. Its native ELF
programs, initramfs and observed boot records are structural fixtures; this is
not evidence of a kernel/PID 1 boot.

P05 installation integration uses `tests/finalization.py --installation --recsetup
<recsetup> --busybox <native-static-busybox> --binary <recab>
--kernel <existing-kernel> --module <matching-ko> --init <systemd|openrc>`. It runs real chroot personalization and real ext4/FAT
copy/sealing/publication checks, including failed initialization, copy termination,
publication failure, retry without repeating passwords/settings, and zero payload
writes on the first continuation. Boot observations remain structural fixtures;
this does not establish a kernel/PID 1 or distro-service boot.

Release policy is a regular slot-owned root file. Recab refuses symlinked or
group/world-writable policy so shared mutable state cannot replace the
distribution trust authority.


## Signed update discovery and download (P08)

The producer must configure the installed source, acquisition recipe and public
release key. `check` gives a useful error when they are missing; it never infers
an endpoint from `HOME_URL` or a package-signing key. Every HTTP request is made
by Recipe inside Ginux, with only a small request input and configured endpoint
access. The worker never receives a running/prepared system tree.

```sh
recab update check                # verifies signed index and manifest
recab update download            # pins the checked release; copy its returned id
recab update download <id>       # retry that exact release, even after restart
recab update status
recab update apply <id>          # offline merge, retaining X or reporting conflicts
# Or select mounted media: recab update apply --manifest /mnt/release/release.json
recab update cancel --preparation <id> --revision <n>
```

Check/download hold recab's lock, leave both roots protected and do not change
preparation revisions, slot ledger or boot eligibility. They can run while the
inactive preparation contains X. Signed index/release high-water marks reject
rollback or same-sequence substitution. The exact signed manifest, URLs and
hashes are retained under the manifest's SHA256; retry does not switch to a new
release behind the caller's back.

Interrupted download retains completed artifacts. Retry rehashes them and
restarts only the incomplete artifact; byte-range resume within that artifact
is not implemented. A successful download verifies the complete actual
EROFS/kernel/initramfs/inventory set through the P07 importer. A cached release
can be reverified and selected with the network disconnected. These commands
work for either active slot.

Status schema 10 reports `updates` and its structured `merge`. Download progress reports verified files/bytes,
reused objects and Recipe process I/O for the current attempt. Failed producers
can have unreported writes, so `accounting_complete` is false after interruption.
Counts exclude other verification/cache copies, filesystem journals and device
writes; no SMART or zero-write claim is made. Failed Recipe diagnostics remain
in the private update work directory; successful invocation work is removed.

`update apply` now merges the exact verified release into the preparation. An
unresolved or partially applied merge blocks ordinary edits and finalization.
`cancel` requires the current preparation ID/revision and retains X before
release application starts. Once writes start, recover the captured plan;
cancellation cannot reconstruct discarded release-side choices. Explicit
`discard` abandons the preparation. Cached release inputs remain retained.

Focused integration uses `tests/finalization.py --release-builder <builder>
--updates <recipe>` with existing native kernel/module artifacts and a matching
Ginux launcher; `--updates-running B` exercises B to A. It runs a genuine signed local HTTP feed, disconnect/retry,
index rollback refusal, offline reuse/selection and cancellation while X is
prepared. It does not build a distribution or boot a VM.


## Release merging, conflicts and recovery (P09)

O is the original release baseline, C is the prepared system (including committed
local changes and X), and N is the verified incoming release. When C equals O,
take N, including deletions. When N equals O, keep C, including local deletions.
When C equals N, keep that result. Otherwise report a conflict. Content, ownership,
modes, xattrs, symlinks and hardlink groups participate; directory replacement
conflicts select complete subtrees. Release timestamps are image-writer policy,
not evidence of a separate content edit. Only recab-owned root references are materialized in fstab; independent release
changes and conflicts still participate in the merge. Kernel-assigned SELinux labels are
retained when incoming metadata declares none, as in Recipe publication.

The installer calls `baseline` before personalization. The protected slot file
`/usr/lib/recab/release-baseline.json` stores fingerprints and account-field/member
fingerprints, separate from personalized generation identity. It contains no
account bodies, password hashes or private keys. Verified adoption and subsequent
updates also retain the exact release ID/cache. Older installs without a known
baseline stop visibly; provide the signed original release with
`update adopt-baseline --manifest OLD/release.json`. Recab never guesses O from C.

```sh
recab update apply <release-id>
recab update conflicts
recab update resolve <conflict-id> --choice local --preparation ID --revision N
recab update resolve <conflict-id> --choice release --preparation ID --revision N
recab update resolve <conflict-id> --choice edited --file ./resolved.conf --preparation ID --revision N
recab update recover --preparation ID --revision N
recab finalize
```

Use the current preparation/revision from each result; stale requests fail.
Edited input is a regular file of at most 1 MiB; its contents and intended mode,
UID/GID and xattrs are imported explicitly. Directory and hardlink conflicts
choose a complete side. Account local/release choices select colliding fields,
preserving unrelated changes; edited account input explicitly replaces the file.
Membership is merged by presence, so removed users/groups/members do not return
through unconditional unions. Duplicate names/IDs, missing primary groups,
passwd/shadow/group/gshadow inconsistencies and invalid file-owner identities
stop before release writes. Recab does not remap shared user data.

The merge plan captures C once, pins N and persists choices before applying any
release files. After a kill, `recover` uses that plan and verifies its retained
inputs; it never recalculates X from a partly updated tree. Account/edited bodies
remain only in protected candidate work files, which are removed on completion;
shared state/status carries fingerprints and redacted errors. A failed validation
or partial merge leaves the baseline incomplete and candidate unavailable. The
baseline advances only after the complete resolved tree passes account, receipt,
runtime and matching kernel/module/initramfs validation. The finalizer reuses that
pinned boot set automatically, including after further local changes Y.

Local Recipe selection and receipts are retained. Changes to runtime libraries
conservatively require reproduction of selected native ELF packages through
Recipe. Module packages also require reproduction when the kernel binding
changes. `package_rebuild` cannot be bypassed with a local/release choice:

```sh
recab update reproduce PACKAGE --recipe ./compatible.rhai --preparation ID --revision N
```

The producer declares `NEEDS.inputs.release` as a file. Recab supplies the exact
verified manifest and `RECAB_RELEASE_BASE` define, forces a fresh isolated
production, and requires `ctx.recab_release` to acknowledge that ID. The recipe
must use compatible toolchain/dependencies for that target; this handshake is
provenance, not a general ABI proof. Reconciliation refuses administrator-owned
collisions and retains dependency selection; final validation checks native ELF
loaders/library closure. The worker receives no running/prepared tree. A package
claiming a changed release-owned path requires an explicit local choice, compatible
reproduction, or cancellation and removal through Recipe before retrying.

Focused native integration uses `--updates <recipe> --release-builder <builder>`
with `--updates-running B` for B to A. Optional `--adopt-baseline`,
`--merge-id-collision`, `--update-kernel <existing-kernel> --update-module <ko>`
exercise migration, ID collision refusal and an actual different cached kernel.
The fixture covers deletions, metadata/type conflicts, edited resolution, account
fields/member removal, Recipe reproduction, actual partial-write kill, frozen-input
tampering, recovery, later Y and bound finalization. No full distro/kernel/ISO
build or VM boot is implied.
