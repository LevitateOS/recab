# recab Requirements

**Version:** 0.1.0  
**Status:** Draft  
**Last Updated:** 2026-02-14

This document defines the requirements for `recab`: an A/B (slot-based) boot
selection and commit/rollback manager for LevitateOS-style immutable systems.

`recab` is intentionally small: it does not build filesystem trees, download
packages, or manage recipes. It is the slot state machine glue called by higher
level tooling (installers, `recipe`, checkpoint harnesses).

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

`recab` manages slot selection for A/B immutable systems:

- Identify active vs inactive slot (A vs B)
- Stage a trial boot of the inactive slot ("try")
- Commit the booted slot as good ("commit") or roll back ("rollback")

This exists to support the repo's checkpoint ladder (notably `07Update` in
`checkpoints.md`) and the repo-wide A/B default plan (`docs/ab-default-plan.md`).

### 1.2 Scope

This specification covers:

- The slot state machine behavior
- CLI command behavior and output
- A backend abstraction (systemd-boot is the initial target)
- Persistent state/audit recording under `/var`

This specification does not cover:

- Building slot filesystems (handled by installers / `recipe`)
- Downloading update payloads (handled elsewhere)
- Package dependency resolution (handled by `tools/recipe`)

---

## 2. Terms

| Term | Definition |
|------|------------|
| Slot A / Slot B | Two independent bootable system roots. Exactly one is active (booted) at a time. |
| Active slot | The slot that is currently booted and mounted as `/`. |
| Inactive slot | The non-booted slot (the target for composing the next system). |
| Trial boot | Booting the inactive slot with automatic fallback if it is not committed as "good". |
| Commit | Mark the currently booted slot as "good" and make it the default boot target. |
| Rollback | Switch boot target back to the previously known-good slot. |
| Backend | The implementation used to read/write boot selection metadata (e.g. systemd-boot). |

---

## 3. Design Principles

### 3.1 Small and Explicit

**REQ-DESIGN-001**: `recab` MUST remain a small tool/library focused only on
A/B slot selection and commit/rollback semantics.

**REQ-DESIGN-002**: `recab` MUST NOT perform package management actions (install,
remove, upgrade) and MUST NOT compose filesystem trees.

### 3.2 Conservative Boot Changes

**REQ-DESIGN-010**: `recab` MUST be conservative about making boot-affecting
changes: when prerequisites are not met, it MUST fail with an actionable error.

**REQ-DESIGN-011**: `recab` MUST be safe to call from automation (no interactive
prompts in the default path).

### 3.3 Idempotency

**REQ-DESIGN-020**: `recab` operations SHOULD be idempotent where possible.
Example: setting next slot to the already-pending slot should succeed without
changing meaningfully observable state.

### 3.4 Cross-Architecture

**REQ-DESIGN-030**: `recab` MUST support `x86_64` and `aarch64` targets.

---

## 4. State Machine

This section defines the required behaviors, not the storage format.

### 4.1 Required States (Conceptual)

**REQ-SM-001**: `recab` MUST model the system as being in one of these conceptual
states:

- `Stable(A)` or `Stable(B)`: a known-good active slot is booted; no uncommitted
  trial is pending.
- `TrialPending(A->B)` or `TrialPending(B->A)`: the next boot is configured as a
  trial boot of the inactive slot.

### 4.2 Required Transitions

**REQ-SM-010**: A `set-next` operation MUST transition the system into
`TrialPending(active->inactive)` (unless already pending for that slot).

**REQ-SM-011**: A `commit` operation MUST transition the system into
`Stable(active)` and clear any trial-pending metadata.

**REQ-SM-012**: A `rollback` operation MUST configure the next boot to use the
previously known-good slot.

### 4.3 Trial Boot Safety

**REQ-SM-020**: Trial boot semantics MUST provide automatic fallback if the
trial slot fails to boot successfully or fails to be committed as good within a
bounded number of boot attempts.

Note: this requirement implies bootloader-level support (or equivalent) because
userspace cannot run when the system does not boot.

---

## 5. CLI Interface

### 5.1 Commands

**REQ-CLI-001**: The CLI MUST provide these subcommands:

- `recab status`
- `recab set-next <A|B>`
- `recab commit`
- `recab rollback`

**REQ-CLI-002**: `recab status` MUST report, at minimum:

- active slot (A/B)
- inactive slot (A/B)
- whether a trial boot is pending
- the configured "next boot" target if one exists

**REQ-CLI-003**: `recab set-next <A|B>` MUST configure the specified slot as the
next boot target using trial boot semantics.

**REQ-CLI-004**: `recab commit` MUST mark the currently booted slot as "good"
and make it the default boot target.

**REQ-CLI-005**: `recab rollback` MUST configure a boot of the previously known
good slot on next reboot.

### 5.2 Output Rules

**REQ-CLI-010**: Commands MUST be quiet on success (no extraneous logs), and MUST
print actionable errors to stderr on failure.

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
- Functions to `set_next(slot)`, `commit()`, and `rollback()`

**REQ-LIB-003**: The library MUST separate pure state representation from the
backend implementation used to read/write boot metadata.

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
UEFI systems using `systemd-boot` boot entries for slot A and slot B.

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

**REQ-PERSIST-002**: Persistent state writes MUST be atomic (temp file + rename).

### 8.2 Audit Metadata

**REQ-AUDIT-001**: `recab` SHOULD record minimal audit metadata for transitions,
such as:

- timestamp
- previous slot, next slot
- operation type (set-next/commit/rollback)

---

## 9. Error Handling

### 9.1 Exit Codes

**REQ-ERR-001**: The CLI MUST use stable, documented exit codes.

**REQ-ERR-002**: Error output MUST include enough context for operators and
automation to determine whether the failure is actionable vs a hard stop.

### 9.2 Failure Safety

**REQ-ERR-010**: Failed operations MUST NOT leave boot configuration in a
partially updated state.

---

## 10. Security

**REQ-SEC-001**: `recab` MUST refuse to perform mutating operations unless run
with sufficient privileges (root).

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

- `tools/recab/README.md` and `tools/recab/CLAUDE.md` (tool scope and intent)
- `QUALITY.md` (repo-wide CLI/tooling quality expectations)
- `checkpoints.md` (07Update Slot B Trial Boot)
- `docs/ab-default-plan.md` (repo-wide A/B default plan)
- `tools/recipe/OS_UPGRADES_BRAINDUMP.md` (recipe + A/B upgrade flow)
- `RalphOS/planning.md` (A/B-only appliances and trial boot + commit model)
- `IuppiterOS/AB_IMMUTABLE.md` (A/B delta for disk images)

### 11.3 Validation

Unit tests are prohibited. Use `cargo check -p recab` or the explicit real-disk lifecycle check in `tests/e2e_sdc.rs` as appropriate. Real-disk checks retain their destructive-operation acknowledgement requirements. No maintained traceability matrix or minimum coverage gate is required during development.
