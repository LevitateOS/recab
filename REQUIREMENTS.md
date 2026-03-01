# recab Requirements

**Version:** 0.1.0  
**Status:** Draft  
**Last Updated:** 2026-02-14

This document defines the requirements for `recab`: an A/B (slot-based) boot
selection and commit/rollback manager for LevitateOS-style immutable systems.

`recab` is intentionally small: it does not build filesystem trees, download
packages, or manage recipes. It is the slot state machine glue called by higher
level tooling (installers, `recipe`, stage harnesses).

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

This exists to support the repo's stage model (notably `Stage 07` in
`stages.md`) and the repo-wide A/B default plan (`docs/ab-default-plan.md`).

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

**REQ-BACKEND-001**: `recab` MUST define a backend abstraction that can be
mocked in tests.

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
- There SHOULD be a verification trace (how it is tested).

### 11.2 Source Traces (Initial)

Primary sources for these requirements:

- `tools/recab/README.md` and `tools/recab/CLAUDE.md` (tool scope and intent)
- `QUALITY.md` (repo-wide CLI/tooling quality expectations)
- `stages.md` (Stage 07 Slot B Trial Boot)
- `docs/ab-default-plan.md` (repo-wide A/B default plan)
- `tools/recipe/OS_UPGRADES_BRAINDUMP.md` (recipe + A/B upgrade flow)
- `RalphOS/planning.md` (A/B-only appliances and trial boot + commit model)
- `IuppiterOS/AB_IMMUTABLE.md` (A/B delta for disk images)

### 11.3 Traceability Matrix

The matrix below is the record to be maintained as implementation lands. Fill
in implementation/tests as each requirement is implemented.

| Requirement | Source | Implementation | Verification |
|---|---|---|---|
| REQ-DESIGN-001 | `tools/recab/README.md`, `tools/recab/CLAUDE.md` | `tools/recab/src/lib.rs` public API (`status`, `set_next`, `commit`, `rollback`) and narrow state/backend model | `cargo check -p recab`; code-scope review in this crate |
| REQ-DESIGN-002 | `tools/recab/README.md`, `tools/recab/CLAUDE.md` | No package/fs-tree composition logic in `tools/recab/src/lib.rs`; A/B-only operations | `cargo test -p recab`; code-scope review in this crate |
| REQ-DESIGN-010 | `tools/recab/CLAUDE.md`, `QUALITY.md` | Conservative preconditions in `set_next_with`, `ensure_entry_exists`, `ensure_uefi_ready` | `tests::set_next_requires_default_to_match_known_good_for_fallback`; `tests::validate_uefi_paths_fails_without_efi_runtime` |
| REQ-DESIGN-011 | `tools/recab/CLAUDE.md`, `QUALITY.md` | Non-interactive CLI in `tools/recab/src/main.rs` (`clap` parsing only; no prompts) | `cargo check -p recab`; manual run paths |
| REQ-DESIGN-020 | `QUALITY.md` | Idempotent `set-next` handling in `set_next_with` when same trial slot already pending | `tests::set_next_is_idempotent_when_same_trial_already_pending` |
| REQ-DESIGN-030 | `RalphOS/planning.md`, `stages.md` | Architecture-neutral Rust code in `tools/recab/src/*.rs` (no x86_64-only branches) | `cargo check -p recab` on host; target-agnostic code review |
| REQ-SM-001 | `docs/ab-default-plan.md`, `stages.md` | Conceptual stable/trial states represented by `PersistentState` + `compose_status` | `tests::set_next_commit_rollback_flow` |
| REQ-SM-010 | `docs/ab-default-plan.md`, `stages.md` | `set_next_with` sets `pending_trial_slot` and writes trial transition state | `tests::set_next_commit_rollback_flow` |
| REQ-SM-011 | `docs/ab-default-plan.md`, `stages.md` | `commit_with` clears `pending_trial_slot`, sets `known_good_slot`, and default entry | `tests::set_next_commit_rollback_flow` |
| REQ-SM-012 | `docs/ab-default-plan.md`, `stages.md` | `rollback_with` targets `known_good_slot` as next/default boot | `tests::set_next_commit_rollback_flow` |
| REQ-SM-020 | `docs/ab-default-plan.md`, `stages.md` | Trial path uses `bootctl set-oneshot` and requires default aligned to known-good fallback (`set_next_with`) | `tests::set_next_requires_default_to_match_known_good_for_fallback`; code review of `set_trial_slot` |
| REQ-CLI-001 | `tools/recab/src/main.rs`, `tools/recab/README.md` | `Command::{Status,SetNext,Commit,Rollback}` in `tools/recab/src/main.rs` | `cargo check -p recab`; CLI parser build verification |
| REQ-CLI-002 | `tools/recab/README.md` | `print_status` emits active/inactive/trial/next/default/known-good | `cargo check -p recab`; manual `recab status` inspection |
| REQ-CLI-003 | `tools/recab/README.md`, `docs/ab-default-plan.md` | `Command::SetNext` -> `recab::set_next` -> backend one-shot trial | `tests::set_next_commit_rollback_flow` |
| REQ-CLI-004 | `docs/ab-default-plan.md`, `stages.md` | `Command::Commit` -> `recab::commit` (`known_good` + default slot update) | `tests::set_next_commit_rollback_flow` |
| REQ-CLI-005 | `docs/ab-default-plan.md` | `Command::Rollback` -> `recab::rollback` (known-good default target) | `tests::set_next_commit_rollback_flow` |
| REQ-CLI-010 | `QUALITY.md` | Success path is quiet for mutating commands; failures emit actionable stderr in `main` | `cargo check -p recab`; manual CLI behavior check |
| REQ-CLI-011 | `stages.md` | `recab status --json` in `Command::Status { json }` + `print_status` JSON branch | `cargo check -p recab`; manual `status --json` output |
| REQ-LIB-001 | `tools/recab/README.md` | Public library functions in `tools/recab/src/lib.rs` | `cargo check -p recab`; CLI consumes library functions |
| REQ-LIB-002 | `tools/recab/src/lib.rs`, `tools/recab/src/main.rs` | Public `Slot`, `Status`, `set_next`, `commit`, `rollback` exported from library | `cargo check -p recab`; `tests::set_next_commit_rollback_flow` |
| REQ-LIB-003 | `tools/recab/README.md` | Separation via `BootBackend` + `StateStore` traits vs pure state structures | `tests` module `MockBackend` + `TempStore` |
| REQ-BACKEND-001 | `tools/recab/README.md` | Mockable backend trait `BootBackend` | `tests` module backend mock usage |
| REQ-BACKEND-002 | `docs/ab-default-plan.md` | `BootBackend` methods cover read active/default + set trial/default | `tests::set_next_commit_rollback_flow` |
| REQ-BACKEND-010 | `docs/ab-default-plan.md`, `IuppiterOS/AB_IMMUTABLE.md` | `SystemdBootBackend` implementation in `tools/recab/src/lib.rs` | `cargo check -p recab`; backend unit coverage via helper tests |
| REQ-BACKEND-011 | `IuppiterOS/AB_IMMUTABLE.md` | `ensure_uefi_ready` + `validate_uefi_paths` hard-fail on missing UEFI/efivars | `tests::validate_uefi_paths_fails_without_efi_runtime` |
| REQ-BACKEND-012 | `docs/ab-default-plan.md`, `stages.md` | `set_trial_slot` uses `bootctl set-oneshot`; fallback invariant enforced in `set_next_with` | `tests::set_next_requires_default_to_match_known_good_for_fallback` |
| REQ-PERSIST-001 | `tools/recab/README.md`, `docs/ab-default-plan.md` | `PersistentState` tracks known-good + pending trial + last transition; persisted in `JsonStateStore` | `tests::state_store_roundtrip_and_atomic_write`; `tests::set_next_commit_rollback_flow` |
| REQ-PERSIST-002 | `QUALITY.md` | Atomic writes via `write_json_atomic` (`temp + rename`) | `tests::state_store_roundtrip_and_atomic_write` |
| REQ-AUDIT-001 | `RalphOS/planning.md` | `AuditEvent` + `append_audit` JSONL records operation/timestamp/slot/state delta | `tests::set_next_commit_rollback_flow` (audit path exercised on transitions) |
| REQ-ERR-001 | `QUALITY.md` | Stable exit mapping via `stable_exit_code`; documented in `README.md` exit-code section | `tests::stable_exit_code_maps_tagged_errors` |
| REQ-ERR-002 | `QUALITY.md` | Actionable error messages with path/operation context across backend/state functions | `cargo test -p recab`; manual failure-path review |
| REQ-ERR-010 | `docs/ab-default-plan.md` | Mutation safety: state-first writes + `revert_state_or_escalate` rollback on backend failure | `tests::set_next_requires_default_to_match_known_good_for_fallback`; `tests::set_next_commit_rollback_flow` |
| REQ-SEC-001 | `tools/recab/CLAUDE.md`, `SECURITY.md` | `ensure_root` gate for mutating operations | `tests::stable_exit_code_maps_tagged_errors` + manual non-root CLI run |
| REQ-SEC-002 | `tools/recab/src/main.rs` | Slot input constrained by `SlotArg` (`clap::ValueEnum`) + `Slot` type | `cargo check -p recab`; clap parser constraints |
| REQ-SEC-003 | `SECURITY.md`, `RalphOS/planning.md` | `PersistentState` and `AuditEvent` contain no credential/secret fields | code review + `tests::state_store_roundtrip_and_atomic_write` |
