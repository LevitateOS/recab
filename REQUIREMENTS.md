# recab Requirements

**Version:** 0.3.0

**Status:** Product contract; implementation incomplete

**Last Updated:** 2026-11-10

`recab` owns system updates and symmetric A/B continuation for LevitateOS and
AcornOS. It coordinates obtaining updates, preparing the inactive system,
applying system changes and updates through Recipe, finalizing a candidate,
trial boot, commit and rollback. Recipe owns software operations inside Ginux;
recab owns the system destination and lifecycle.

The current implementation supports destructive release-image staging and boot
selection. It does not yet meet the requirements for identical initial slots,
incremental continuation, preservation of pending changes through updates, or
Recipe operations directed at an inactive system. Requirements below describe
the intended behavior; they are not claims that these features already work.

[IMPLEMENTATION.md](IMPLEMENTATION.md) defines the implementation decisions,
ordered work, deployment constraints and runtime acceptance for this contract.

---

## Table of Contents

1. [Overview](#1-overview)
2. [Terms](#2-terms)
3. [Design Principles](#3-design-principles)
4. [State Machine](#4-state-machine)
5. [CLI Interface](#5-cli-interface)
6. [Rust Library API](#6-rust-library-api)
7. [Backend Requirements](#7-backend-requirements)
8. [Persistence and Audit](#8-persistence-and-audit)
9. [Error Handling](#9-error-handling)
10. [Security](#10-security)
11. [Conformance and Traceability](#11-conformance-and-traceability)

---

## 1. Overview

### 1.1 Purpose

`recab` manages the system update lifecycle:

- Install equivalent personalized systems into A and B, both bootable at first use.
- Keep the active system root immutable and prepare its continuation in the
  inactive slot, synchronizing differences rather than rewriting unchanged data.
- Apply change X and a downloaded system update to the same prepared system,
  preserving X or reporting a conflict before activation.
- Finalize and trial-boot that system; commit a healthy result or return to the
  protected known-good system after failure.
- Repeat from B to A for change Y, retaining X and the previous update.

This supports A/B update and rollback validation described in `architecture.md`.

### 1.2 Scope

This specification covers:

- Initial slot population and machine personalization
- Update discovery, download and application coordinated with Recipe
- Incremental system continuation and preparation-session behavior
- System and boot-artifact finalization, trial, commit and rollback
- CLI/library contracts and frontend status
- The systemd-boot backend and persistent state under `/var`

Recipe owns package facts, dependency resolution, acquisition, builds and
software installation/update/removal inside Ginux. Recab MUST coordinate that
existing engine for system updates rather than introduce another package
manager. Delegation of software work does not remove updates or filesystem
preparation from recab's responsibility.

---

## 2. Terms

| Term | Definition |
|------|------------|
| Slot A / Slot B | Two independent bootable system roots. Exactly one is active (booted) at a time. |
| Active slot | The slot that is currently booted and mounted as `/`. |
| Inactive slot | The non-booted slot (the target for composing the next system). |
| Continuation | The active system reproduced in the inactive slot, with destination-specific identity, ready for further system changes. |
| Preparation | A writable inactive system receiving changes and updates before finalization. It is not eligible for trial boot while incomplete. |
| Candidate | A finalized system generation with its matching kernel, initramfs and boot configuration. |
| Generation | An identity for particular system contents and boot artifacts, independent of the reusable slot letter. |
| Trial boot | Booting the inactive slot with automatic fallback if it is not committed as "good". |
| Commit | Mark the currently booted slot as "good" and make it the default boot target. |
| Rollback | Switch boot target back to the previously known-good slot. |
| Backend | The implementation used to read/write boot selection metadata (e.g. systemd-boot). |

---

## 3. Design Principles

### 3.1 Small and Explicit

**REQ-DESIGN-001**: `recab` MUST own the system update and continuation lifecycle,
with explicit operations reusable by people, installers, Recipe and frontends.
Boot selection is one part of that responsibility.

**REQ-DESIGN-002**: `recab` MUST prepare and maintain the inactive system tree
and coordinate software changes through Recipe. Release-image installation is
one update input, not the only supported form of system change. Existing
preparation work MUST NOT be discarded merely to apply a release update.

**REQ-DESIGN-003**: Every continuation, update, trial and recovery operation
MUST work symmetrically A→B and B→A. Slot letters identify destinations; they
MUST NOT confer different capabilities.

### 3.2 Conservative Boot Changes

**REQ-DESIGN-010**: `recab` MUST be conservative about making boot-affecting
changes: when prerequisites are not met, it MUST fail with an actionable error.

**REQ-DESIGN-011**: `recab` MUST be safe to call from automation (no interactive
prompts in the default path).

### 3.3 Idempotency

**REQ-DESIGN-020**: `recab` operations SHOULD be idempotent where possible.
Example: setting next slot to the already-pending slot, or committing a slot
that is committed, the default and has no trial pending, should succeed without
changing meaningfully observable state.

### 3.4 Supported target

**REQ-DESIGN-030**: `recab` MUST support native Linux `x86_64`. Additional
CPU architectures are deferred indefinitely and are not delivery prerequisites.

---

## 4. State Machine

This section defines the required behaviors, not the storage format.

### 4.1 Required States (Conceptual)

**REQ-SM-001**: `recab` MUST model the system as being in one of these conceptual
states:

- `Stable(A)` or `Stable(B)`: a known-good active slot is booted; no uncommitted
  trial is pending.
- `Preparing(A->B)` or `Preparing(B->A)`: the inactive system is writable and
  accepts changes and updates; incomplete contents cannot be selected for trial.
- `Ready(A->B)` or `Ready(B->A)`: preparation is finalized with matching boot
  artifacts and can be scheduled for a trial.
- `TrialPending(A->B)` or `TrialPending(B->A)`: the next boot is configured as a
  trial boot of the inactive slot.
- `TrialRunning(A->B)` or `TrialRunning(B->A)`: the candidate is running
  uncommitted; the other slot remains the protected fallback.
- Interrupted or failed preparation: its generation and completion state remain
  identifiable, with safe recovery or explicit discard available.

### 4.2 Required Transitions

**REQ-SM-010**: A `set-next` operation MUST select only a finalized candidate and
transition to `TrialPending(active->inactive)` (unless already pending for that
same generation). The presence of ESP files alone is insufficient.

**REQ-SM-011**: A `commit` operation MUST transition the system into
`Stable(active)` and clear any trial-pending metadata.

**REQ-SM-012**: A `rollback` operation MUST configure the next boot to use the
previously known-good slot: the known-good slot when the running slot is
uncommitted or a trial is pending, else the slot that was known-good before the
running slot was committed, while that slot still holds its system.

**REQ-SM-013**: Preparation and update application MUST write only the inactive
system, keep it ineligible for trial until complete, refuse to overwrite the
fallback of an uncommitted running slot, and prevent boot selection into a slot
being changed. Before the first write, recab MUST validate source/destination
identity, paths, capacity and required boot support. It MUST remove the old
generation's rollback eligibility before changing it. Ordinary continuation
MUST reuse the inactive filesystem; formatting and full replacement MUST NOT be
the default continuation or a way to silently discard pending changes.

**REQ-SM-014**: recab MUST NOT select (as default or one-shot) a slot whose
entry names files missing from the ESP, and MUST NOT count such an entry as the
default: systemd-boot skips it and boots another. Because such a firmware
variable takes effect once the entry can boot, `stage` (and `set-next`) MUST
first point a `LoaderEntryDefault` naming another entry than the known-good
default at it, and `stage` MUST remove a `LoaderEntryOneShot` naming its slot
while no trial is pending, refusing when it cannot.

**REQ-SM-015**: Installation through recab MUST populate A and B with equivalent
system contents and final machine configuration, matching kernels and
initramfs images, and valid slot-specific partition references and boot entries.
Both slots MUST be bootable at first use. A initially runs as the committed
default; B is its matching inactive copy. Final personalization, including
machine identity and services, MUST reach both before installation completes.
Recab MUST set up the bootloader, state and trial service. It MUST reset stale
firmware selection when installing onto the boot target from live media, warn
when unavailable, and leave a separate running installation's firmware alone.

**REQ-SM-016**: A continuation MUST synchronize inactive contents to the committed
active generation before applying the next change. After B contains X and an
update, preparing A for Y MUST retain both X and that update. Reopening the same
preparation MUST retain its pending work instead of reseeding from the active
system. Beginning a new preparation over the previous system ends that older
rollback; the committed active system remains protected.

**REQ-SM-017**: Incremental synchronization MUST account for additions, removals,
content changes, file types, ownership, modes, links and extended attributes,
including ACLs and capabilities. It MUST avoid copying unchanged system data,
exclude shared user data and runtime mounts, preserve destination-specific slot
identity, and use a consistent source view. Both destinations MUST be able to
hold the resulting system. A configuration carry allowlist is not a system delta.

**REQ-SM-018**: Recab MUST coordinate update discovery, download and application
through the existing Recipe/Ginux software machinery. A user MUST be able to
apply an update to a preparation that already contains X. Local changes,
including deliberate removals, MUST be preserved or produce actionable conflicts.
Recab MUST NOT silently overwrite X or ignore a conflicting release change.

**REQ-SM-019**: Finalization MUST validate a complete candidate generation and
its matching kernel, initramfs and boot configuration, end writable preparation
and protect the system root for boot. Failure or interruption MUST leave the
candidate unavailable for trial. The running root remains immutable; writable
configuration and shared-data policy MUST be explicit and MUST NOT substitute
for the required system-change lifecycle.

### 4.3 Trial Boot Safety

**REQ-SM-020**: Trial boot semantics MUST provide automatic fallback if the
trial slot fails to boot successfully or fails to be committed as good within a
bounded number of boot attempts.

Note: this requirement implies bootloader-level support (or equivalent) because
userspace cannot run when the system does not boot. recab gives one attempt
(a one-shot entry over a known-good default); fallback happens at the next
reboot, so REQ-SM-022 makes failed trials reboot.

**REQ-SM-021**: With the trial service enabled (every image enables it), the
trial boot `set-next` scheduled MUST be committed without operator action when
its startup ends with nothing failed, and MUST NOT be committed when something
failed, startup ended in the init's emergency state (systemd maintenance, the
OpenRC single-user runlevel), or it does not end within a bounded time. Other boots (a committed
slot, a slot picked from the boot menu, a trial an operator rolled back) and
startups without a verdict (a shutdown) MUST be left alone.

**REQ-SM-022**: A failed trial MUST reboot into the known-good slot, unless that
slot cannot boot: a trial `recab trial` judges failed (REQ-SM-021) is rebooted by
it, also when health collection fails or the ESP or /var cannot be read (the firmware's selected and
default entries tell a one-shot trial); slot entries carry `panic=` so a
panicking kernel reboots; and a trial's entry carries the initramfs's
fail-reboot word when it boots through one, and the init's crash option where
it has one, which it loses when it becomes the default. A kernel or PID 1 that hangs without
crashing is outside this requirement: it needs a hardware watchdog, which
the current implementation does not configure. Until a watchdog is provided,
hard-hang recovery is a documented limit, not a guaranteed automatic reboot.

**REQ-SM-023**: A judged trial MUST be recorded (slot, outcome, how startup
ended, failed units), shown by `recab status`, and audited.

---

## 5. CLI Interface

### 5.1 Commands

**REQ-CLI-001**: The CLI MUST provide these subcommands:

- `recab status`
- `recab stage --from-media <device|dir>` or `recab stage --rootfs <image> --kernel <file> --initrd <file>`
- `recab set-next <A|B>`
- `recab commit`
- `recab rollback`
- `recab init` (installation entry point; matching initial population is required)
- `recab trial` (decides a trial boot; run by `recab-trial.service`)
- `recab install-unit [--root DIR] --init <systemd|openrc> [--bin PATH]`
- `recab baseline --root DIR` (complete original release inventory/account baseline before personalization)

The CLI/library also exposes the lifecycle through:

- `recab prepare`, `finalize` and guarded `discard`
- `recab change install|update|remove|recover|list|check` and `recab edit`
- `recab update status|check|download|apply|cancel`
- `recab update conflicts|resolve|reproduce|recover|adopt-baseline`
- `recab release import` and `recab verify-installation`

`stage` routes verified media/manifests through the same preserving release merge
and finalization; raw files require their corresponding manifest. Baseline
adoption supplies a verified original release to an existing safe preparation;
it is not adoption or state migration of a legacy installation. Fresh installs
use the current structured state and immutable boot protocol in both roots.
Older development formats are inspectable but read-only; there is no in-place
version promotion or legacy bootstrap command.

**REQ-CLI-002**: `recab status` MUST report, at minimum:

- active slot (A/B)
- inactive slot (A/B)
- whether a trial boot is pending
- the configured "next boot" target if one exists
- each slot's generation, preparation/completion state and source generation
- pending changes, update progress or failure, conflicts and candidate readiness
- available actions and their effect on fallback and rollback eligibility

**REQ-CLI-003**: `recab set-next <A|B>` MUST configure the specified slot as the
next boot target using trial boot semantics.

**REQ-CLI-004**: `recab commit` MUST mark the currently booted slot as "good"
and make it the default boot target.

**REQ-CLI-005**: `recab rollback` MUST configure a boot of the previously known
good slot on next reboot.

### 5.2 Output Rules

**REQ-CLI-010**: Commands MUST provide concise outcomes and useful progress for
long operations, with machine-readable output for frontends, and MUST print
actionable errors to stderr on failure. `recab trial` prints one line when
it commits, the record of the decision (the journal on systemd, the kernel log
on OpenRC).

**REQ-CLI-011**: `recab status` SHOULD support a machine-readable output mode
(for example `--json`) suitable for scripts and test harnesses.

---

## 6. Rust Library API

### 6.1 Public API Shape

**REQ-LIB-001**: `recab` MUST expose a Rust library API that higher-level tools
can call without invoking the CLI.

**REQ-LIB-002**: The library API MUST expose:

- A `Slot` type (A/B)
- A `Status`-like structure that represents the same information as `recab status`
- Functions to `set_next(slot)`, `commit()`, and `rollback()`, and `init()`,
  `stage()` and `trial()`
- System preparation, continuation, finalization and update operations with
  explicit target and generation identity, coordinated with Recipe

**REQ-LIB-003**: The library MUST separate pure state representation from the
backend implementation used to read/write boot metadata. (`src/state.rs` plans
every transition without I/O; `src/systemd_boot.rs` reads and applies it.)

---

## 7. Backend Requirements

### 7.1 Backend Abstraction

**REQ-BACKEND-001**: Keep boot-backend operations separate from slot-state logic.

**REQ-BACKEND-002**: Backends MUST provide a way to:

- read current active slot
- configure trial boot of a target slot
- commit the currently booted slot
- configure rollback to the previously known-good slot

### 7.2 systemd-boot Backend (Initial Target)

**REQ-BACKEND-010**: `recab` MUST support a `systemd-boot` backend suitable for
UEFI systems using `systemd-boot` boot entries for slot A and slot B, writing
systemd-boot's EFI variables itself, so it works where only systemd-boot's EFI
binary is installed (no `bootctl`).

**REQ-BACKEND-013**: `recab` MUST support systemd and OpenRC as init systems for
everything that depends on them: deciding a trial (REQ-SM-021), rebooting, the
trial service, trial kernel options, and the configuration `stage` carries.

**REQ-BACKEND-014**: `stage` MUST NOT depend on GNU coreutils options, so busybox
systems can stage.

**REQ-BACKEND-011**: If the host is not booted in UEFI mode (or efivars are not
available), the `systemd-boot` backend MUST fail with a clear error that
explains what is missing.

**REQ-BACKEND-012**: The `systemd-boot` backend MUST implement try/commit
semantics in a way that satisfies `REQ-SM-020` (automatic fallback).

---

## 8. Persistence and Audit

### 8.1 Persistent State

**REQ-PERSIST-001**: `recab` MUST maintain a small persistent state record under
`/var` that is sufficient to support:

- determining the previously known-good slot for rollback
- recording the last requested transition (set-next/commit/rollback)
- identifying active and prepared generations and their relationship
- detecting incomplete preparation after interruption and supporting safe recovery
- preserving pending changes and update state across separate commands

**REQ-PERSIST-003**: Runs that change anything MUST hold an exclusive lock for
their whole duration, so their reads and writes never interleave.

**REQ-PERSIST-004**: Both slots' recab read shared state. Compatible unknown
fields MUST be preserved, but a reader MUST refuse mutations when it cannot
enforce the state's safety semantics. Accepting every future schema and keeping
unknown fields does not establish compatibility with preparation restrictions.

**REQ-PERSIST-005**: A boot-selection change MUST write the boot selection
before recab's state, so a crash between the two leaves the machine booting
what was asked.

**REQ-PERSIST-002**: Persistent state writes MUST be atomic and durable: a temp
file flushed to disk, renamed over the target, and the directory flushed. The
same applies to `loader.conf` and to the entries and kernels `stage` writes
(where the FAT ESP allows: its renames are not guaranteed atomic).

**REQ-PERSIST-006**: Shared user data, build caches and stored package payloads
MAY remain on `/var`. System package selection and configuration MUST belong to
their system generation so preparation does not change the running system's
selection. Rollback scope MUST identify shared state it does not restore,
including application data migrations. Recab MUST NOT copy shared data as part
of the system delta or claim that a boot rollback reverses all `/var` changes.

### 8.2 Audit Metadata

**REQ-AUDIT-001**: `recab` SHOULD record minimal audit metadata for transitions,
such as:

- timestamp
- previous slot, next slot
- operation type and affected system generation, including preparation and update

---

## 9. Error Handling

### 9.1 Exit Codes

**REQ-ERR-001**: The CLI MUST use stable, documented exit codes (README, "Exit
Codes"): 13 permission, 20 backend unavailable, 21 refused or undetectable
state, 22 partial update, 23 failed trial.

**REQ-ERR-002**: Error output MUST include enough context for operators and
automation to determine whether the failure is actionable vs a hard stop.

### 9.2 Failure Safety

**REQ-ERR-010**: Failed operations MUST preserve the protected fallback and keep
incomplete candidates unavailable for trial. Partial boot/state writes MUST be
reported accurately with recovery information. Predictable input/path/layout
refusals MUST happen before destructive changes. External helper calls used for
trial health MUST be bounded so the deadline covers the full assessment.

---

## 10. Security

**REQ-SEC-001**: `recab` MUST refuse to perform mutating operations (`init`,
`stage`, `set-next`, `commit`, `rollback`, and `trial` when it commits or
reboots) unless run with sufficient privileges (root).

**REQ-SEC-002**: `recab` MUST NOT accept arbitrary slot identifiers. Slot inputs
MUST be validated and restricted to `A` or `B`.

**REQ-SEC-003**: `recab` MUST NOT store secrets in its persistent state.

---

## 11. Conformance and Traceability

### 11.1 Traceability Rules

This repository treats requirements as SSOT. For every requirement in this
document:

- There MUST be a source trace (why the requirement exists).
- There SHOULD be an implementation trace (where it is implemented).
- Validate changed behavior with an appropriate CLI or runtime check when useful.

### 11.2 Source Traces (Initial)

Primary sources for these requirements:

- The user's 2026-11-10 lifecycle: identical initial A/B systems; change X and
  a downloaded update in one continuation; healthy trial or fallback; change Y
  in the reverse continuation using only differences
- `README.md` and `CLAUDE.md` (tool scope and intent)
- `../../architecture.md` (system ownership and A/B update lifecycle)

### 11.3 Validation

Follow the superproject's validation rules. Focused CLI and real install/boot
checks must distinguish existing release-image staging from the required full
lifecycle. Coverage must include equivalent initial slots, X plus an update in
one preparation, reversal with Y retaining both, interrupted preparation,
conflicts and deletions, unchanged-data write avoidance, and trial/fallback on
both init systems. Historical A/B scenario passes do not establish these missing
capabilities. No maintained traceability matrix or minimum coverage gate is
required during development.
