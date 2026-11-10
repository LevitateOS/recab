# Recab implementation plan

Recab must implement the complete system update and continuation lifecycle in
[REQUIREMENTS.md](REQUIREMENTS.md). Installation finishes with equivalent,
personalized A and B systems. A committed running system stays immutable. Its
continuation receives system changes and downloaded updates in the inactive
slot, then becomes eligible for trial boot. After commitment, the same process
works in the other direction using differences rather than another full copy.

This plan covers recab and the changes required in Recipe, installation,
recinit, release production, frontends and runtime validation. It is an
implementation plan, not a claim that the proposed operations already exist.
The acceptance case is the user's X plus update, then Y, across A→B→A.

## Implementation progress (2026-11-10)

P01 is implemented in recinit `903a504` and recab `aa8f2e7`, with account-label
and numeric-identity safeguards completed alongside P02. Existing root-protection
work was retained. General writable `/etc` overlays were removed; nonempty legacy
uppers refuse boot until explicit adoption. Staging preflights carried parents,
types, accounts, fstab, expanded capacity and inode demand before invalidation.
Filesystem reads/writes use opened roots and pinned directory descriptors.

P02 now implements complete system inventories, normalized generation/boot
identities, independent slot mappings, structured major state compatibility,
durable preparation ID/source/revision/progress, incomplete/readiness checks,
reopen and explicit discard. Preparing removes the inactive entry after saving
intent. At the P02 checkpoint it stopped at `awaiting_synchronization`.
Release staging refuses an existing preparation.

Focused validation: recab/recinit library and CLI compilation; actual CLI runs
against read-only bind-mounted scratch roots in a user/mount namespace; xattrs,
raw path bytes, hardlinks, sparse extents and shared tmpfs exclusion; generation
artifact mismatch and incomplete candidate refusals; stale ID/revision refusal;
process termination after durable intent followed by reopening that same ID;
discard; uncommitted-source, legacy and future-format refusal; the same
preparation contract in both slot directions; and refusal to commit altered system
content. P01 baseline
CLI checks also exercised xattr recording, symlink confinement and FIFO refusal.
Public schemas and the TUI contract are generated through recab's existing
contract exporter; the TUI and documentation TypeScript checks pass. No unit suites, full system builds or VM scenarios were run.

P03 now synchronizes the inactive ext4 partition in place in either direction.
It preflights identity, mount boundaries, bytes and inodes read-only before ending
old rollback. Durable intent and entry invalidation precede writable mounting;
the delta journal precedes file mutations. Range comparisons write only differing
1 MiB ranges and preserve sparse holes, deletions, types, raw paths, hardlinks
(including symlinks), UID/GID/modes, ACLs, capabilities and xattr removal. Changes
detach inodes whose other aliases could affect excluded data. Directory metadata
is restored last. Source references are materialized for the destination slot.

The operation journal contains actual changes and affected parents. Recovery
re-inventories and checks actual ranges, rather than trusting saved completion.
A verified, flushed and unmounted result becomes `prepared`, remains incomplete
for boot, and reopens without reseeding X. Plans are removed after successful
preparation/discard. Saved reports separate logical payload writes, metadata
operations, identity bytes and plan bytes. Filesystem metadata/journal and other
state byte totals remain unmeasured; interrupted prior writes are explicitly
unknown. These are not physical SSD/SMART write totals.

The reusable real CLI check is `tests/synchronization.py --binary <recab>`.
It passed with two temporary GPT/ext4 roots: A→B, kill after a payload write,
resume, preserve pending X, B→A, and another equal-root continuation. Recovery
skipped the already written 1 MiB range and wrote the remaining 15 MiB of the
interrupted file. Reverse synchronization wrote one 1 MiB range plus 108 bytes
of configuration/slot-reference changes. Equal roots wrote zero payload and zero
generation-marker bytes (616 serialized plan bytes and one directory metadata
update were recorded separately). ACL/capability/xattr/ownership, sparse files,
file/symlink hardlink groups, excluded aliases, raw paths, deletions/type changes,
unsupported special files, same-device bind mounts and space refusal were checked.
Insufficient space left the ledger and existing boot entry unchanged. The check
supplies scratch firmware/boot records; it does not boot a system.

At the P03 checkpoint, writes promoted state 2.0 to 2.1; P02's reader refuses
2.1, preventing it from
dropping synchronization progress and reseeding pending changes. P03's status schema 4
and generated schemas/TypeScript expose the saved synchronization report.
Focused Rust compilation and TUI/docs typechecks pass. No full builds, unit
suites or VM runs were performed. The later P05 checkpoint below adds initial
A/B equivalence; Recipe/update integration remains outstanding.

P04 now implements `finalize` in both directions. Read-only preflight validates
identity/profile, ELF init and libraries, accounts, fstab, runtime directories,
producer-bound recab compatibility in candidate and retained fallback, available
Recipe receipts, full v3 initramfs, kernel release/architecture and module provenance. Machine identity and enabled
SSH keys must be seeded before finalization. The destructive staging entry point
now uses the same finalizer and restores device protection on extraction failure. Its geometry preflight runs on an
anonymous sparse file because mke2fs opens even a dry-run target writable.
The package/finalizer installs explicit runtime policy for mtab, DNS, systemd first-boot writers and OpenRC clock updates.
The installer orders personalization before generation capture. Release production
binds the kernel hash to the installed module tree; old images with a changed
kernel cannot bypass this by merely matching a release string.

Finalization saves intent, keeps the entry absent, applies policy, freezes the
candidate inventory, writes root records and flushes the filesystem. Content-hash
boot objects and generation manifests become durable before the root is unmounted
and its block device protected read-only. Publication is last. Interrupted work
reconciles the same generation; reopening preparation explicitly permits editing.
Selection/commit/rollback recheck root/record/manifest/artifacts. The v3 initramfs
checks kernel release, unique partition identities and the slot/root/generation
record before installed init, and protects the peer root too. Boot collection
retains both recorded slots and foreign entry references. State 2.2 makes earlier
writers refuse mutation; status schema 5 and finalization schema expose seals,
artifact paths, progress and boot-write accounting. Unknown interrupted writes
remain explicit and application bytes are never described as SMART totals.

Focused validation uses `tests/finalization.py` against temporary GPT/ext4 roots
and a real vfat ESP, an existing UEFI kernel/module, native ELF fixtures and
structural firmware/initramfs records. It checks durable-intent termination,
publication failure/retry, absent entries, readonly devices, stale revision,
idempotence, boot-record tampering, A→B→A selection/commit/rollback, and artifact
reuse with zero reverse boot payload writes. Local X, corrupted boot artifacts,
and case-insensitive foreign references during collection are checked too.
Both systemd and OpenRC configurations passed these filesystem/CLI checks.
The OpenRC run also passed real EROFS extraction through recstrap, shared
finalization after staging, and protection restoration after failed extraction.
Scratch roots carry valid host SELinux labels; denied label removal remains an
explicit incomplete-preparation error with operation context.
The P03 real-filesystem synchronization regression check still passes. A focused
recinit producer run using cached Alpine tools, static busybox and the existing
7.3.3-levitate kernel tree produced a 1922 KiB v3 archive accepted by the new
whole-stream inspector. Rust compilation and documentation/TUI typechecks pass.
This does not establish a kernel, PID 1 or distro-service boot. Full installed boot/hardware and aarch64 validation
remain pending; no full system builds or VM runs were requested or performed.

P05 now initializes both personalized systems. Recsetup captures the image
baseline once and finishes requested accounts/passwords, service/network settings,
machine identity and enabled SSH host keys before recab initialization. Its
request/filesystem-bound apply receipt stores completed commands without passwords;
retries skip those commands, revalidate the boot step, and refuse changed unfinished
personalization. Media/device paths may change, with actual identity validation
remaining in recab. A fresh state filesystem retains ordinary /var/lib permissions.

Recab records installation intent and freezes the personalized generation under
/var/lib/recab/installation.json. It copies A into the existing ext4 B with the
P03 synchronizer, including full metadata and destination-specific root/slot
records, excluding shared/runtime/ESP contents. Initial root-home contents are
published durably into shared state for either first boot. It validates normalized
A/B equivalence and P04 boot/runtime bindings, flushes/seals both roots, installs
one reusable boot object set, then publishes both entries and the default. A is
committed/default; B is ready without an invented successful trial. Completion
requires both roots, entries, ledger and installation record to be durable.
Incomplete installation blocks ordinary mutations and automatic commitment.

Initialization retries reconcile the same frozen source, actual ranges, objects
and publication. A matching completed retry revalidates both systems; a changed
source, preparation or live ledger refuses reset. The install guide now places
service/network choices before sealing; the finish action remains dependent on
successful recsetup completion. The existing VM install evidence checks now read
both generation-bound entries instead of expecting A-only legacy kernel paths.

Focused validation uses the existing filesystem integration harness with
`--installation --recsetup <recsetup> --busybox <native-static-busybox>`. It performs actual chroot account/password
and hostname changes, seeds identity/SSH keys, then exercises invalid initramfs,
copy termination, publication failure and retries. It checks preserved settings
edited after the failed attempt, unchanged password hashes/keys/machine identity,
shared homes, equivalent A/B generations and destination identities, two bootable
entries, initial B readiness, live-ledger reset refusal and a first continuation
with zero system payload writes. Boot observations are structural fixtures;
actual kernel/PID 1/distro boots remain unverified. No full builds or VM scenarios
were run. Recipe system targeting and downloaded-update merging remain later work.

P06 now connects Recipe software changes to the inactive preparation. Root's
package commands on immutable A/B systems delegate to recab; `--system` selects
that route explicitly. Independent profiles and output-only builders keep their
existing routes. Package directories install into the slot-owned system profile;
explicit system outputs carry root-relative intended metadata. Recipe's existing
graph produces runtime dependencies and coordinator-owned producer provenance.
The versioned target manifest includes owned paths and explicit removals.

Recab checks observed fingerprints, dependency constraints and compatible
coownership before publication, refusing unowned base/admin file replacement and
overwrite/removal of administrator edits. It projects only selected package data,
then uses P03's range writer against those paths. No whole candidate is passed as
a Recipe input, checkpoint or export. Producer caches include preparation/source
and revision scope in addition to Recipe's existing source/dependency/architecture
identity. Normal package inspection leaves published caches unchanged.
Unspecified SELinux labels retain installed kernel policy; authored labels and
other xattrs remain exact constraints.

A durable selected-payload journal precedes writable access. Interrupted changes
block other edits/finalization and recover the same frozen bytes and receipt, or
can be explicitly discarded. `recab edit` uses a private existing regular-file
copy limited to 1 MiB, preserving ownership, permissions and xattrs, and retaining package fingerprints for
later edit detection. Active/prepared selection is explicit and activation stays
pending reboot. Login paths use the booted slot's system profile for all users,
with independent ordinary-user profiles. State 2.3 / status schema 6 expose
pending/completed changes and counters; older writers cannot drop them.

Focused compilation and the real Recipe/Ginux/ext4/vfat integration passed
service/library install, update and removal, dependency refusal, administrator
conflicts/edits, failed producer isolation, reopening X, both slot directions,
and finalization/boot-artifact reuse without changing the running root. The
extended native-runtime fixture passed with OpenRC too, including ordinary
package output, killed publication recovery and altered-payload refusal.
The existing P03 synchronization regression and docs/TUI typechecks passed. Process I/O counters include runtime staging/checkpoints/
exports/diagnostics; projection and candidate logical writes are separate. They
are not SSD or SMART totals. Compiler-runtime extraction remains producer I/O;
this does not claim zero total writes or optimal binary deltas. Actual distro
boots, full builds, and downloaded-release merging remain deferred.

P07 now defines one typed version 1 release manifest and system inventory in
`distro/contract::release`, shared by builder emission and recab import/staging.
It binds distro/profile, architecture, channel, publisher-controlled sequence,
version/source digest, exact payload paths/hashes/sizes, kernel/module release,
and minimum recab/state/boot compatibility. Optional source/trust fields are
validated in the variant contract and installed at
`usr/lib/recab/update-source.json`. The development variants declare sequence 1
without inventing a production endpoint or distribution key.

The image writer captures intended UID/GID/mode/xattrs/link/content metadata from
the exact tar stream passed to mkfs and binds it to the output EROFS digest in
local image-writer provenance. Release emission checks that proof, the prepared
tree and boot bindings, then publishes `release.json` last. The ISO carries the
same manifest/inventory/signature as the online payload layout. Producer checks
do not require the host kernel to support the target's EROFS compression.

Recab `release import` verifies installed identity/order/compatibility and
configured distribution Ed25519 trust, hashes/copies artifacts to a private
read-only set and checks the actual mounted EROFS inventory, ELF/runtime,
kernel/module tree and complete installed initramfs before any candidate change.
Source paths are never reopened for use after verification. Import moves those
same files into a content-addressed cache without another full payload copy;
cache reuse is rehashed. `stage --manifest` and `stage --from-media` use the same
verifier before invalidation, while retaining the frozen inputs through use.
Unsigned local use requires explicit `--trusted-local`; it cannot bypass a
configured key. Explicit legacy raw-file staging remains pending P12.

Focused CLI/library compilation and a real native EROFS builder/consumer
integration passed. The fixture uses existing kernel/module artifacts and actual
image packing, intended non-root ownership/mode/xattrs/hardlinks, wrong
identity/protocol/digest/set refusals, exact-byte Ed25519 signatures with a local
test key, cache alteration refusal, source replacement after verification and
read-only verified inputs. Running A, prepared B with pending X, state and boot
entries remain unchanged. The host lacks EROFS zstd, so mounted consumer checks
use a genuine small LZ4 fixture; producer provenance avoids that host limitation.
No full distro/ISO/kernel builds or VM boots were run. P08 acquisition/index
publishing is implemented below; P09 release merging remains outstanding.

The starting-point description below records the pre-implementation audit.

Part of P11 was brought forward on 2026-11-10 for the desktop front ends
(`gui/`): `status --json` schema 7 adds `lifecycle` (one state, what the other
system is, whether a command runs now), `preparation.changes` (every finished
change, with the caller's `--actor` label), and the text status prints both.
It is derived from existing state only and adds no operation. Compiled with
the library; not run on an A/B system. The generated schemas and the TUI's
TypeScript were not regenerated then, because P08's unfinished `updates` field
was open in the same files: run `cargo xtask gen` when that lands.

Also for the front ends, the same day: `recab compare` (`src/compare.rs`), a
read-only comparison of the two systems from the inventory `prepare` already
uses. Its types are not registered in `contract.rs` yet, for the same reason.

## Starting point

The documentation baseline is recab `d5da60b`, with superproject `cceed07` and
docs-content `76ffc63`. Recab already provides release-image staging,
systemd-boot selection, trial health, commit, rollback, locking and durable
state writes. Its existing state machine and boot backend remain useful.

The working tree also contains uncommitted immutability work in recab and
recinit. It protects the root device and introduces writable per-slot `/etc`
overlays. Review and reconcile those exact changes before implementing this
plan; do not overwrite or commit them incidentally with documentation. Their
current presence is not validation of an immutable installed system.

The main mismatches are empty initial B, destructive `stage`, a configuration
allowlist instead of a complete system delta, no preparation lifecycle, and
Recipe activation into a root profile shared by both slots. Existing scenario
results concern release-image staging and do not establish continuation.

## Implementation decisions

### Storage and write avoidance

Keep the current two ext4 root partitions, shared ext4 `/var`, and EFI system
partition. Recab synchronizes the inactive filesystem in place. It does not
format during ordinary continuation or update application. Recpart remains the
owner of partition creation and initial formatting.

Compare file contents and complete metadata before writing. For an existing
regular file whose contents changed, compare fixed 1 MiB ranges and write only
different ranges, including a partial final range and required truncation.
Stream new files. Preserve sparse extents and hardlink groups within the
destination. Metadata-only changes must not rewrite file contents. Fixed-range
comparison is deliberately bounded: inserting bytes near the start may change
many ranges; do not claim optimal binary deltas.

In-place range writes are allowed only to an inactive, unbootable preparation.
A crash may leave that preparation incomplete; recovery checks actual contents
against the persisted operation before continuing. Do not describe this as an
atomic file replacement. The committed running slot remains untouched.

The write budget includes input staging, Recipe checkpoints, exports, temporary
trees and ESP installation. A delta at the final destination is insufficient
if another step first writes a complete root tree to the SSD. Reuse verified
release payloads, select only needed Recipe inputs, and publish only changed
outputs for installed-system operations. Shared caches may store downloaded or
built content once. Routine continuation must not allocate a third full system
tree on disk.

### System configuration and runtime data

Treat persistent system configuration, including ordinary `/etc` content, as
part of the generation being prepared. The target policy is a read-only active
system tree, including that configuration. A general writable `/etc` overlay
would let system changes bypass continuation, so the uncommitted overlay design
must be revised rather than adopted as the final policy.

Runtime files belong on `/run` or `/var` through explicit paths or links.
Inventory actual systemd, OpenRC, networking, SSH, login and desktop writes and
move only runtime state there. Resolver output and lock files are runtime
state; enabled services, administrator configuration and saved system settings
are system configuration. Seed machine identity and SSH host keys once during
installation so both initial systems have the same identity. Do not start
services in the inactive system to generate them.

User homes, root's home, application data and build/download caches remain
shared. Copy neither their contents nor mounted views as part of a system
delta. System package selection and receipts live with their generation,
separate from shared payload/cache storage. Per-user Recipe profiles can remain
independent of the system lifecycle. Boot rollback does not reverse changes to
shared application data; incompatible data migrations must be reported before
activation and must not run as preparation-time host hooks.

### Software execution and publication

Recipe remains the software engine, running recipes inside Ginux. Recab owns
the destination, generation, change application and boot lifecycle. Recab
invokes Recipe's producer interface with activation disabled and consumes its
validated, versioned result. There is no dependency cycle in which Recipe's
system-facing command calls recab and recab calls that same command again.

Extend the existing Recipe target metadata and publication path for system
outputs: full paths, logical owners and modes, xattrs, links, content digests,
removed paths and package ownership/receipt information. Current publication
permissions are host staging permissions, not intended installed permissions.
The existing `target.rs` manifest is the starting point; do not derive root
ownership from the UID that ran a build or bypass the manifest to copy files.

Do not weaken the existing independent user-package surface to install arbitrary
system files into a live shared profile. Add a system consumer of the same
recipe format and typed outputs. It can install libraries, services and boot
artifacts, beyond the current independent profile's `bin` and `share` surface.

For a system package, pass the inventory and explicit target files needed by
that recipe, reuse immutable dependency outputs, and publish the package's
changed paths. For configuration editing, use a temporary copy of the selected
file and submit its content and metadata as a checked change. Do not import
and export an entire installed system for every small edit. Extend Recipe's
existing input/checkpoint machinery if it would otherwise force that copying.
Recipes receive no writable host slot, block device, live `/var`, ESP or host
service manager.

### Identity and authoritative records

Keep one recab state machine in `state.rs` and one boot writer in
`systemd_boot.rs`. Add focused modules for generation inventory, synchronization,
preparation, update application and the Recipe adapter; keep the public facade
and existing library entry points. Do not introduce a second updater daemon,
generic workflow engine or a separate package manager.

The shared recab state records:

- Format/compatibility version, active committed generation and rollback generation.
- Each slot's PARTUUID, current generation, completion state and boot-artifact
  identities. A slot letter is never a generation identity.
- Preparation ID, source generation, candidate revision, current operation,
  completed durable steps, pending update, conflicts and failure details.
- The generation scheduled for trial and the observed boot identity, so a later
  reuse of the same slot cannot satisfy an older pending trial.

Each system inventory records path bytes, type, content digest/size, mode,
owner, xattrs including removals, link target, hardlink relationship, modification
timestamps and sparse layout. Exclude access/change times and filesystem allocation
details from semantic equality. Do not include the inventory's own serialized
file or self-referential digest fields in the system content hash. Store path
bytes losslessly rather than using
lossy UTF-8 conversion. Inventory iteration and hashing must be deterministic
and stream large files rather than load them whole.

Keep the release baseline distinct from the personalized committed generation
and from subsequent local changes. Store baseline fingerprints and the identity
of a retained verified release payload, not another expanded root tree. Account
secrets remain in protected system files; state, logs and public status contain
digests or redacted summaries, never password hashes, private keys or file bodies.

Normalize only fields recab owns when comparing A and B: root PARTUUID,
slot-specific boot paths and the installed immutable-boot record. Never copy A's
slot identity into B or ignore arbitrary configuration differences as normalization.

### Preparation and update states

The candidate lifecycle is:

```text
stable → preparing → ready → trial pending → trial running → committed
               ↘ interrupted or conflicted ↗              ↘ failed → fallback
```

The diagram describes the candidate; the active committed system remains stable
alongside its preparation in the other slot.
Every mutation holds the existing recab lock from revalidation through durable
completion; it releases the lock between commands. A preparation is not a mount
left open between commands. Each command uses private mounts, closes its Recipe
children, flushes necessary changes and unmounts before returning.

`prepare` reopens an existing preparation without reseeding it. A new preparation
starts from the committed active generation. If that source changes through a
boot or commit, refuse to silently rebase an older preparation: require explicit
discard/recreation. Reopening a ready candidate for a new change first removes
its boot eligibility. A pending trial must be explicitly cancelled before more
changes. An uncommitted running slot never permits preparation over its fallback.

Before modifying the inactive system, durably record its loss of rollback
eligibility and incomplete state, reset stale firmware selection, and remove its
boot entry from bootloader discovery. Keep slot/partition mapping in recab state,
not solely in the entry being removed. A failed invalidation aborts before any
system write. Removing the entry protects against manual menu selection as well
as `set-next`; finalization restores it only after system and boot files are ready.

Use durable intent plus completion records for writes that cannot be atomic
together. Recovery inspects state, generation markers, inventories, boot entries
and EFI selection; it never trusts a saved progress percentage. A missing or
conflicting completion record leaves the candidate unavailable. Finalization is
idempotent for unchanged candidate contents.

### Applying a release without losing X

Define O as the previous release baseline, C as the current prepared system
(including committed local changes and pending X), and N as the incoming release.
Apply these rules to contents, metadata and deletions:

| Relationship | Result |
| --- | --- |
| C equals O | Take N, including a release deletion. |
| N equals O | Keep C, including a deliberate local deletion. |
| C equals N | Keep that result without rewriting it. |
| Both differ from O and from each other | Record a conflict; do not silently choose either side. |

Check directory/type changes and parent/child conflicts before application. No
automatic text merge is required for the first implementation: explicit local,
release or edited resolution is sufficient. Validate the complete resolved tree
before finalization, rather than treating a resolution as proof of bootability.

Persist the merge plan and captured C revision before applying nonconflicting
changes. Keep update inputs pinned until completion. After interruption, resume
that exact plan; recomputing C against O after partly applying N would mistake
the update's own changes for local edits. Advance the candidate's release
baseline to N only once the merge and all resolutions complete durably. New
ordinary changes are refused while a merge needs recovery/resolution.

Treat account files as structured configuration using their old baseline.
Keep per-field fingerprints and member-presence information sufficient to compare
the old release without storing raw account secrets in the shared state log.
Preserve local user/group removals and member removals; no unconditional union.
Validate the final complete passwd/group/shadow/gshadow relationships, duplicate
names and numeric IDs, primary groups and installed file ownership. If stable
local IDs conflict with new release IDs, stop before mutation rather than
silently remap shared user data. Fixing only the current account append branch
does not solve account correctness.

Local packages have desired selection, recipe/source digests and receipts.
Reproduce that selection through Recipe against the new base when compatibility
requires rebuilding; do not blindly copy a binary linked for an older base.
Conflicts between a local package and release-owned paths are explicit. A kernel
change includes matching modules and initramfs; it is not merely a new ESP file.

### Release discovery and download

Add one versioned installed-system release manifest shared by the builder and
recab. It binds distro, architecture, libc/init/flavour, channel, monotonically
ordered release identity, rootfs inventory, kernel release/modules, kernel and
initramfs hashes and sizes, minimum recab/boot-protocol version, and payload URLs
or media-relative paths. The ISO and online update use the same manifest.

The builder emits this manifest with its existing source/provenance records and
release outputs. Add update-source and trust configuration to the existing distro
contract; validate it through `distro/contract`. Use a distribution-controlled
verification key for release metadata, not a Fedora/Alpine package-signing key
as an assumed key for our composed system. Use established cryptographic
libraries and Recipe acquisition helpers, not bespoke signature or HTTP code.

`check` verifies the index/manifest and reports an applicable release; it does
not change a slot. `download` pins that release and obtains verified payloads
through Recipe into a reusable cache; it does not change boot selection or
erase a preparation. `apply` verifies cached inputs again and merges into the
existing preparation. Interrupted downloads resume only against the same pinned
identity and digest. Stale indexes cannot silently downgrade the installed
release; deliberate downgrades are explicit and receive the same conflict checks.

No production update endpoint or distribution release key is currently declared
in the inspected contract. Adding those is release configuration and publishing
work, not a URL to invent in implementation. Local/media manifests and a local
HTTP fixture can exercise the complete producer/consumer contract first.

## Proposed command surface

These names are implementation targets, not existing commands. Library calls
must expose the same operations; frontends use the same state/action results.

| Operation | Proposed CLI | Required behavior |
| --- | --- | --- |
| Install both initial systems | `recab init` with existing target/device arguments | Run after final personalization; populate and validate both roots and boot sets. |
| Create or reopen continuation | `recab prepare` | Infer inactive slot; preserve an existing preparation; report its ID and revision. |
| Apply software changes | `recab change install\|update\|remove <recipe>` | Delegate production to Recipe, then apply checked outputs and update the candidate receipt. |
| Change configuration | `recab edit <system-path>` | Edit a bounded temporary file and apply it to the preparation; never expose a persistent writable root. |
| Discover an update | `recab update check` | Report the applicable verified release and compatibility. |
| Obtain an update | `recab update download [--release <id>]` | Cache the pinned release without changing the preparation or next boot. |
| Apply an update | `recab update apply [--release <id>]` | Create/reopen preparation and preserve X through the baseline merge. |
| Apply release media | Existing `recab stage --from-media ...` and file inputs | Route into the same update engine; file inputs require a matching manifest. No implicit format/replacement. |
| Inspect and resolve conflicts | `recab conflicts`; `recab resolve <path> --local\|--release\|--file <path>` | Resolve a captured candidate revision; validate metadata and the final system. |
| Finalize | `recab finalize` | Validate, flush, seal and publish the candidate's boot entry last. |
| Trial and keep | Existing `set-next`, `trial`, `commit` | Bind selection and outcome to the finalized generation, not just its slot. |
| Return or cancel trial | Existing `rollback` | Select the protected known-good system or cancel a scheduled trial. It does not discard pending contents. |
| Recover interrupted work | `recab recover` | Reconcile the durable operation and actual contents; resume safely or give a precise refusal. |
| Discard preparation | `recab discard --preparation <id>` | Explicitly abandon pending work and invalidate its boot entry; do not claim the older overwritten system was restored. |

Mutation commands accept an expected preparation ID/revision for frontend use
and reject stale requests. Keep stable existing exit codes and add structured
reasons for conflicts, incomplete preparation, insufficient space and unsupported
formats. `status --json` gives generation/preparation state, action verdicts,
progress, planned/actual writes and recoverable failures. Progress has explicit
phases and byte units, not an invented percentage over unknown work.

Recipe's `--system` route delegates to recab. On immutable A/B installs, root's
ordinary software activation selects the system route by default; per-user
profiles remain independent. Explicit independent profile selection remains
available and is clearly identified as shared/user software. Builder
`--no-activate` execution never redirects into recab. List active system receipts
separately from prepared changes so a successful install does not imply the
running system already changed. After applying X, report that reboot activation
is pending instead of silently activating a root shared profile.

## Ordered implementation work

Each work item ends with a coherent implementation commit in its owning
repository. Cross-repository changes include the matching callers and contract
updates. Work stays on existing `main`/`master`; no feature branches or mandatory
per-commit build/VM pipeline. The dependencies below order implementation, not
the runtime scenario registry.

### P01 Reconcile immutability and close existing destructive-path bugs

Owners: recab `immutable.rs`, `stage.rs`, `carry.rs`, `systemd_boot.rs`, `init.rs`;
recinit `installed.rs` and `templates/init_installed.template`.

- Review the uncommitted root protection and initramfs inspection changes.
  Retain useful protection; replace general writable configuration with the
  generation policy above, accounting for existing overlays during import.
- Move predictable image, carried-path, directory-type, fstab, capacity and
  immutable-layout refusals before any invalidation or formatting in the old
  staging path while it still exists.
- Replace path-string checks followed by `fs::write` with directory-FD-confined
  operations. Apply the same rule to prepared accounts/fstab and input reads;
  absolute links in an image must not address the host's `/etc`.
- Fix complete-account collision validation, attribute-only comparison and xattr
  removal. Stop silently unioning ambiguous account changes; refuse when the
  current baseline cannot establish a safe result, until P09 supplies structured
  merging. Reject malformed entries without a loader target. Reuse these fixes
  in the later implementation rather than keep a second independent copier.

Evidence: real CLI refusals on scratch installed trees/devices leave both slot
contents and boot selection unchanged; metadata and accounts survive actual
filesystem operations. No claim that these fixes alone deliver continuation.

### P02 Add generation inventory and preparation state

Owners: recab `state.rs`, `store.rs`, `system.rs`, `target.rs`, `status.rs`; add
focused `generation.rs` and `prepare.rs` modules as needed. Depends on P01.

- Implement the records, normalized inventory and states above. Persist operation
  intent/completion and expected candidate revision under the existing lock.
- Split boot-file availability from validated generation readiness. Read slot
  mapping independently of boot entries. Reject stale generation references.
- Make major format compatibility explicit. An older reader that ignores new
  fields must not be allowed to mutate new preparation state; see deployment below.
- Keep read-only status available with bounded, redacted information and a clear
  recovery reason for damaged or incompatible state.

Evidence: actual CLI runs can create/reopen a preparation, reject stale or
uncommitted-source requests, and preserve durable incomplete state after process
termination. Scheduling refuses a candidate even if old kernel files remain.

### P03 Implement incremental system synchronization

Owners: recab generation/preparation modules and new `sync.rs`; extend
`tools/recstrap/target-root` only for shared low-level filesystem primitives.
Depends on P02.

- Implement streaming inventory comparison, range writes, additions, deletions,
  metadata-only operations, sparse files, symlinks and destination hardlink groups.
- Preserve UID/GID/modes/xattrs and remove obsolete xattrs. Set capabilities after
  ownership/mode/content changes. Apply directory metadata after child mutations.
- Detach a destination inode before mutation if its hardlink aliases are outside
  the intended changed group. Never patch a cached publication or excluded file
  through a shared inode; include any required replacement writes in accounting.
- Refuse unexpected mount boundaries, devices, sockets and FIFOs in system
  content unless a supported image contract explicitly describes them. Runtime
  `/dev`, `/proc`, `/sys`, `/run`, `/tmp`, shared `/var`, `/home`, `/root` and
  ESP contents are not recursively copied.
- Preflight destination space and inode needs using the uncompressed desired
  result, growth and bounded scratch requirements. Check both root capacities
  during initial population; do not require only that a compressed image fits.
- Record deterministic operation order and byte counters. Resume by checking
  actual completed data, never by trusting an old manifest after interruption.

Evidence: synchronize two real ext4 roots A→B→A; an identical tree writes no
system payload, a one-range edit updates only that range, metadata-only changes
write no file contents, and deletions/type/link changes converge correctly.
State/metadata/journal writes are reported separately from copied payload bytes.

### P04 Finalize and boot immutable candidates

Owners: recab `systemd_boot.rs`, `entry.rs`, `immutable.rs`, preparation/state;
recinit installed boot; builder installed-system runtime policy. Depends on P03.

- Implement entry removal before mutation and entry publication after validation,
  filesystem flush and unmount. Do not mutate a mounted active root or reuse a
  live overlay upper/work directory. Restore writable access only to the checked
  inactive device during a command; return it to read-only at finalization.
- Validate root identity, required executable init, init/libc/flavour/architecture,
  fstab/state mounts, complete accounts, required runtime directories, Recipe
  receipts and kernel/module/initramfs consistency.
- Install boot artifacts under immutable generation names; publish the slot entry
  only after all referenced files are durable. Reuse matching boot payloads rather
  than copy identical kernels on every preparation. Keep references required by
  either slot; garbage collect only unreferenced boot generations under lock.
- Bind installed root markers, state and trial metadata to the same generation.
  Sealing prevents ordinary writes; it does not pretend to prevent a privileged
  administrator deliberately bypassing block-device protection.

Evidence: reboots during finalization select the known-good system or a complete
candidate, never a half-written root/artifact set. Finalization errors and a
manual menu pick cannot expose an incomplete preparation as a bootable entry.

### P05 Populate both systems at installation

Owners: recab `init.rs`, recsetup `plan.rs`/`apply.rs`, installer finish/session
state, recpart handoff and capacity checks. Depends on P04.

- Extract and personalize A once; generate shared machine identity and host keys
  once. Place all requested users/passwords/services/network settings before the
  final recab initialization step. Initial baselines precede personalization.
- Synchronize the completed A into B using the same inventory/applier, exclude
  nested ESP/state mounts, render B's identity, and install both boot sets.
- Validate equivalence after slot normalization and report installation complete
  only after both roots and both entries are durable. A is committed/default;
  B is an equivalent ready copy, not falsely recorded as having passed a trial.
- Installer retries reopen their own installation state and do not reset a live
  installation's generation ledger or lose settings made after the first attempt.

Evidence: fresh installation on both distros has equivalent personalized A/B
systems; either boots immediately. First prepare changes only expected slot
metadata and new X, with no full-system payload rewrite.

### P06 Add the Recipe system change contract

Owners: Recipe `target.rs`, `inputs/*`, `publication.rs`, `retained.rs`,
`bin/recipe/{cli,commands}.rs`; recab Recipe adapter; distro system recipes.
Depends on P03 and P04.

- Define versioned producer receipts and intended system metadata through the
  existing Recipe target manifest. Include owned paths and removals; distinguish
  live host metadata from logical installed metadata.
- Implement install/update/removal against a prepared selection. Dependencies
  remain Recipe's responsibility. Reject overlapping incompatible ownership;
  removal does not delete another selected package's files or administrator edits.
- Scope retained work to recipe digest, target generation/revision, dependency
  inputs and architecture. A saved build from a different base cannot silently
  qualify as installed in this candidate.
- Avoid full-system input/export/checkpoint copies for a small change. Prove this
  with filesystem and write counters across the entire invocation. If Ginux lacks
  required selected-input or export behavior, extend its existing interfaces in
  its maintained delivery repository, preserving upstream ownership and tests.
- Add root/system routing and active/prepared receipt views described above.
  Keep per-user software and producer-only builder invocations working.

Evidence: while A runs, Recipe installs a service or library into B, updates it,
removes it and edits configuration without affecting A or activating shared root
packages. Failed builds publish nothing to the preparation. Reopening retains X.

### P07 Produce and consume one release manifest

Owners: `distro/contract` schema/validation; builder `pipeline/{products,source,
sources,io}.rs`, OS recipes and release artifact assembly; recab update module.
Depends on P02 and P04; software production uses P06.

- Define the typed manifest and update source/trust fields described above.
  Reuse existing source hashes, kernel release facts, flavour and architecture.
- Produce the same manifest and system inventory for media and online updates,
  preserving metadata from Recipe's intended target manifest.
- Bind the exact validated bytes through use: stable verified files/FDs or
  immutable cache objects, not a mutable pathname reopened after validation.
- Validate incompatible runtime/boot protocols before touching the candidate.
  Do not infer matching kernel/initramfs/rootfs merely because all are files.

Evidence: a produced local release imports successfully, while wrong distro,
architecture, init/libc/flavour, digest, boot protocol and mismatched artifact
sets fail before candidate mutation.

### P08 Discover and download updates through Recipe

Owners: recab update module/CLI/status; Recipe acquisition helpers and update
acquisition recipes; distro release configuration. Depends on P06 and P07.

- Implement separate check/download/apply entry points with pinned identities,
  useful progress, bounded network policy, verified caching and interruption
  recovery. Offline apply consumes cached payloads or mounted release media.
- Read producer-defined source and trust configuration; missing configuration
  returns a useful error. Provide a local HTTP fixture for the integration path.
- Do not hold writable candidate mounts or change rollback eligibility merely
  while checking/downloading. Serialize conflicting mutation commands with recab's
  lock and protect cached inputs referenced by any running operation.
- Add publishing/configuration support to the existing release workflow; actual
  production service configuration and key custody must be supplied before an
  online release can be offered to users.

Evidence: download an update while B already contains X; disconnect/retry,
restart the process and apply offline, with X and current boot selection intact.

P08 implementation:

- `update check` verifies the producer's signed index and manifest through the
  installed Recipe acquisition producer, pins exact release identity/URLs and
  persists index/release high-water marks. `update download [id]` retains and
  rehashes completed artifacts across interrupted invocations, restarts the
  incomplete artifact and uses P07 complete EROFS/boot validation before caching.
- Networking is bounded by the installed producer's configured endpoint and
  Ginux budget. The worker sees only a small request file. Check/download hold
  recab's lock and leave both roots, preparation revision, slot ledger and boot
  eligibility unchanged. Successful private work is cleaned; failure diagnostics
  remain. Progress distinguishes verified/reused bytes and incomplete accounting.
- `release index` publishes already signed bundles locally, serializes writers,
  verifies immutable objects and publishes hash-addressed index signatures before
  replacing the index. The builder packages the endpoint-specific recipe/policy.
  No production endpoint or key has been invented or configured.
- Offline `update apply` reverifies cached/media inputs, prepares/reuses the
  inactive system without reseeding X, and pins a durable selection. It reports
  `awaiting_merge` and `merge_complete: false`; P09 owns actual release merging.
  Edits/finalization remain blocked until merge or CAS-checked cancellation;
  cancellation restores the preparation and retains X. This is an explicit
  handoff, never destructive `stage` or a false completed-update claim.
- Focused real integration: `tests/finalization.py --release-builder <builder>
  --updates <recipe>`, with existing kernel/module artifacts and a matching Ginux
  launcher. Signed local HTTP, a truncated transfer, process retry, reused
  artifacts, index rollback rejection, offline cache reuse/selection and stale
  cancellation refusal passed with either A or B running, X, ledger and boot
  eligibility unchanged. Native fixture ELF/initramfs checks remain structural; no full
  distro/ISO/kernel build or VM boot was run.

### P09 Merge a release into an existing preparation

Owners: recab `carry.rs` replacement/update merge module, generation inventory,
conflict/status CLI and Recipe adapter. Depends on P06 through P08.

- Implement the O/C/N rules, persisted merge revision, metadata/deletion and
  directory/type conflicts, structured account merging and final ID validation.
- Retain local package desired selection, reproduce against the new base where
  required, and bind changed boot artifacts to the new candidate.
- Add conflict inspection and explicit resolution without logging private data.
  Finalization refuses unresolved conflicts, incomplete dependency work or a
  partially applied update. Failed resolution does not advance the baseline.
- Route existing `stage` inputs through this engine and remove unconditional
  `mkfs` and inactive-configuration deletion from ordinary staging. Existing
  raw-file inputs require the corresponding release manifest.

Evidence: X plus update succeeds in one B preparation; unrelated release changes
arrive, deliberate removals remain removed, colliding edits stop visibly, and
recovery after partial merge preserves the originally captured X.

### P10 Make trial and fallback cover assessment failures

Owners: recab `trial.rs`, `init_system.rs`, `state.rs`, trial service templates;
recinit failure behavior. Depends on P04; exercise the full candidate from P09.

- Give the complete health assessment a deadline, including helper processes;
  terminate/reap children and distinguish shutdown from assessment failure.
- A health-collection error in a confirmed trial must enter recovery, not return
  before judgment. Use checked generation/firmware facts when `/var` or ESP reads
  fail; never create recab state on an unmounted substitute directory.
- Reconcile EFI selection and durable trial records after partial commit writes.
  Do not commit a different generation because the same slot letter was reused.
- Preserve OpenRC inactive/failed service semantics and systemd isolation behavior.
  Report a missing/unbootable fallback rather than create a reboot loop.
- Keep the hard-hang watchdog limitation explicit. Add a hardware watchdog path
  only through the existing boot/init owners when supported; a VM timeout or
  software loop is not evidence that physical hard hangs recover automatically.

Evidence: healthy trials commit and degraded, timed-out and assessment-error
trials return to the correct generation on both inits. Shutdown gets no success
verdict. Missing state/ESP and failed EFI writes yield honest recoverable states.

### P11 Expose the lifecycle in CLI and existing frontends

Owners: recab `main.rs`, `status.rs`, `contract.rs`, schemas; `xtask` generation;
`tui/apps/runtime/recab-tui`; install apps/docs-content. Depends on P05, P09, P10.

- Export operations, events and readiness/action views from recab. Generate
  schemas/TypeScript through the existing contract tooling. Remove UI assumptions
  that kernel-file presence means ready or that every update formats a slot.
- Show active and prepared systems, X/pending changes, available/downloaded
  update, conflicts, finalization, recovery and reboot-needed state. Allow another
  change or update in the same preparation without reseeding.
- Keep both slot directions identical in UI and use recab's verdicts instead of
  duplicating rules. Stale frontend revision requests must be rejected by recab.
- Update installer completion to require both initial roots. Update documentation
  as operations land, preserving truthful remaining limitations.
- The planned GUI uses this contract; building the Qt app is a separate existing
  project and does not block recab's complete CLI/TUI lifecycle.

Evidence: existing frontend fixtures and real command integration cover stable,
preparing, conflicted, interrupted, ready, pending-trial and failed-trial states
in both directions; no frontend silently discards X.

### P12 Retire the old assumptions and deploy safely

Owners: recab public docs/CLI/package metadata, legacy carry/state paths and
their callers; installer and release tools. Depends on P05 through P11.

- Replace remaining boot-selection-only source descriptions and package metadata.
  Remove destructive continuation, empty-B installation assumptions and obsolete
  carried-path baseline responsibilities once the new inventory owns them.
- Retain boot/health primitives that still have a role. Do not keep two active
  staging/continuation engines or introduce speculative backend abstractions.
- Fresh installations use the new state format and boot protocol. Old development
  installations are detected explicitly, not silently treated as valid paired
  generations. Use the deployment rules below rather than an accidental schema
  upgrade that leaves an old fallback unable to enforce safety.

Evidence: the old refusal/selection paths cannot bypass new readiness checks;
documentation, help, consumers and actual state agree on update ownership.

### P13 Prove the complete lifecycle and write reduction

Owners: `testing/install-tests/src/scenarios/ab_update.rs`, install/runtime
scenarios, guest helpers and `distro/contract` scenario descriptions. Depends on
the implemented paths above; scenarios remain independently runnable.

Extend the existing named `ab-update` scenario and focused install/runtime checks;
do not create an ordered ladder of prerequisite test runs. Scenarios consume
explicit existing artifacts. Required observations are listed below.

The Recipe coordinator currently rejects non-x86_64 execution in
`tools/recipe/src/inputs/root.rs`. REQ-DESIGN-030 also requires aarch64. Add the
corresponding Recipe/Ginux launcher, worker, runtime image and boot artifact
support through their existing owners before claiming cross-architecture
completion. x86_64 results on both distros do not establish aarch64 support.

### P14 Enable production online updates

Owners: distro release configuration, builder publishing workflow and recab
installed defaults. Depends on P07, P08 and successful applicable runtime checks.

Configure the real distribution update origin/channel and release verification
keys, publish matching release metadata/payloads through the existing release
workflow, and verify installed check/download/apply against those published
artifacts. Keep key material out of the repository and logs. Local fixture
success must not be reported as a working production update service.

## Deployment and recovery rules

The first delivery targets fresh installs, whose two initial systems contain
the same compatible recab and recinit. Do not silently infer the missing B,
release baseline or package ownership of an older install.

For existing installs, implement explicit adoption only if they must be retained:
capture their actual root/configuration, including the merged view of any old
`/etc` overlay, inventory locally changed and unknown files, and preserve those
as local content. An unavailable old release baseline is a reported conflict
constraint, not permission to overwrite unknown files. An old shared root Recipe
profile can be imported into the prepared system with receipts and path-conflict
checks; keep the shared original intact for the old fallback. New system login
paths must stop selecting that shared root profile.

Old recab accepts arbitrary future numeric schema versions, so merely adding
fields cannot protect preparation. The new format must be incompatible with
that old parser. For retained legacy installs, use an explicitly restricted
bootstrap period with legacy-compatible state and boot selection while upgrading
and successfully trialing each slot's reader/boot runtime in turn. Do not enable
new preparation-state semantics until both bootable slots enforce them. Keep
unfinished bootstrap targets absent from the boot menu. If a safe two-cycle
bootstrap cannot be completed from the actual install, refuse adoption and use
recovery media/fresh installation; do not patch the running immutable root or
overwrite the only known-good fallback to satisfy the migration.

During ordinary operation:

| Interruption | Recovery behavior |
| --- | --- |
| Check/download cancelled or disconnected | Keep slots and selection unchanged; retain only verified cache objects and resumable work for the pinned release. |
| Process killed during synchronization or Recipe publication | Mark incomplete, keep entry absent, reconcile written data and receipts, then resume the same operation. |
| Crash during update merge | Resume the persisted O/C/N merge plan; do not recalculate local intent from a mixed tree. |
| Unresolved conflict or insufficient space | Keep the active committed system intact and candidate unavailable; allow resolution/recovery or explicit discard. |
| Crash during finalization | Reconcile inventories, boot files and entry before marking ready; absent/uncertain completion is not eligible. |
| Failure before init/health can run | Trial kernel/initramfs failure options cause reboot where supported; firmware default remains the known-good slot. |
| Failed trial or health checker | Select/verify fallback, record what is possible, request reboot; do not continue with a successful commit. |
| Crash after EFI commit but before state completion | Reconcile generation-specific boot selection and intent; do not overwrite the other system based on stale state. |

Garbage collection retains objects referenced by either bootable generation,
active preparation, pending update/merge, trial or recovery operation. Shared
user/application data are never candidates for recab garbage collection.

## Runtime acceptance

| Case | Required observations |
| --- | --- |
| Initial A/B install | Same normalized system/configuration, identity, users and service choices in both; complete matching boot artifacts; A committed and B immediately bootable. |
| First change X | Active A root/configuration and boot generation unchanged; B contains X; identical payloads were not recopied. |
| X plus downloaded update | Check/download retain X and selection; apply retains X, updates release-owned content and package selection, and binds the new boot artifacts. |
| Trial B fails | Automatic reboot for the exercised failure reaches unchanged known-good A; B remains uncommitted and failure is reported. |
| Trial B succeeds | B is committed/default; A remains the previous rollback generation until new preparation begins. |
| Reverse change Y | Preparing A from B retains X and the update, adds Y, reuses its filesystem, and writes differences only. Repeat another full direction change. |
| Rollback loss | Beginning preparation over old A reports that older rollback is gone, while protecting committed B. Discard does not falsely restore old A. |
| Conflict and deletion | Local/release edits to one path require resolution; intentional file/user/member/xattr removals are retained; UID/GID collisions and parent/type conflicts are refused. |
| Metadata and path fidelity | Owner/mode/capability/ACL-only edits, hardlinks, sparse files, non-UTF-8 local paths and symlinks produce equivalent intended trees without escaping to the host. |
| Failure injection | Kill/power-cycle at invalidation, range-write, merge, flush, artifact installation, entry publication and commit boundaries; incomplete candidates stay unavailable and active fallback survives. |
| Concurrency and stale requests | Concurrent recab/Recipe requests serialize or receive a clear refusal; stale preparation IDs/revisions and a changed active generation cannot mutate the wrong target. |
| Disk limits | Insufficient expanded root space, inodes, cache/scratch space and ESP capacity fail predictably where knowable; unexpected ENOSPC leaves recoverable incomplete state. |
| Assessment failures | Missing/hung/failing systemctl or rc-status, startup timeout, degraded services, state/ESP failure and shutdown obey the bounded trial/recovery rules on both inits. |
| Independent software and data | Per-user profiles/data remain available; prepared system packages are not active in the running slot; boot rollback does not claim to reverse shared data migrations. |
| Release verification | Wrong architecture/profile, mixed artifacts, altered payloads, unsupported protocol and stale release identity fail before applying a system change. |
| Write accounting | Unchanged continuation has zero system payload writes; bounded file edits avoid whole-file/root rewrites; counters include Recipe staging/checkpoints/exports and ESP work. |
| Architecture | Required lifecycle is exercised on supported x86_64 and aarch64 stacks; missing Recipe/Ginux aarch64 execution is tracked as incomplete conformance. |

Use counted application writes and isolated block-device observations to evaluate
write reduction. Filesystem journaling, metadata and controller behavior mean
these are not a promise of an exact SMART-counter change or SSD lifetime gain.

Use compilation/typechecking and focused real CLI/filesystem integration checks
where useful. Follow the repository prohibition on unit and mock-only suites;
do not rename isolated helper assertions as integration checks. Existing fixtures
must be revised where they assert empty B or fake away the actual storage path.
Preserve Ginux's upstream testing exception when its implementation changes.

Build release artifacts or run VM scenarios only when requested, covering the
accumulated implementation. Existing named commands remain:

```sh
cargo xtask test all levitate
cargo xtask test all acorn
cargo xtask test ab-update levitate
cargo xtask test ab-update acorn
```

Run VMs through the libvirt user session, with existing kernel artifacts unless
a kernel build is explicitly requested. Record actual artifact identities and
results. Historical passes and cached results for old artifacts do not prove
the new lifecycle. Desktop product runs use their own explicitly built artifacts.

## Completion and first implementation step

The implementation is complete when fresh installs provide both equivalent
systems, X plus a real obtained update survives one preparation, healthy trial
and failure recovery work, and reverse preparation preserves X/the update while
adding Y with measured write avoidance. CLI/TUI, generated contracts and docs
must expose that behavior on both distros. Cross-architecture and production
online-update completion additionally require P13's architecture work and P14's
real release configuration, respectively; report those limits separately if
delivery is staged.

Start with P01 and P02: reconcile the existing root-protection work, remove
unsafe write paths and establish generation/preparation identity. Then build the
incremental synchronization and finalization path before extending the installer
or update UI. This prevents new callers from depending on destructive staging.

## Filesystem references

Use Linux directory-FD resolution to confine access to the selected root;
`openat2` provides `RESOLVE_IN_ROOT`/`RESOLVE_BENEATH` and restrictions on symlink
and mount traversal. Choose compatible flags and do not rely on a textual
prefix check. [Linux openat2 manual](https://man7.org/linux/man-pages/man2/openat2.2.html)

Flush files and their containing directories when publishing durable names;
flushing a file alone does not establish directory-entry durability.
[Linux fsync manual](https://man7.org/linux/man-pages/man2/fsync.2.html)

When importing an older writable `/etc` overlay, read its merged configuration
and preserve deletions. Raw upperdir copying is insufficient because OverlayFS
uses whiteouts and opaque directories, and mounted upper/work trees must not be
reused by a second active overlay.
[Linux OverlayFS documentation](https://docs.kernel.org/filesystems/overlayfs.html)
