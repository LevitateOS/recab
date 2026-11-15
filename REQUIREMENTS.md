# recab Requirements

**Version:** 1.0.0

**Last Updated:** 2026-11-15

recab manages the two root filesystems of an A/B installation of LevitateOS
and AcornOS: root A and root B, two ext4 partitions. `/var` is a third,
shared, writable partition. This document is the whole contract. Nothing
outside it is part of recab.

## 1. The contract

**REQ-1. The booted root is locked.** At any moment the root that booted is
mounted read-only and its block device is set read-only.

**REQ-2. The other root is unlocked.** Its block device is writable and it is
mounted read-write at `/run/recab/other`. Changes to the system are made
there, by anyone with root. Recipe writes its system changes into it.

**REQ-3. Which root is locked follows only from which root booted.** Boot B
and B is locked, A unlocked; boot A and it reverses. No stored state decides
this.

**REQ-4. At installation the other root starts as a copy of the installed
system.** `recab init` copies root A to root B, writes both boot entries and
makes A the boot default.

**REQ-5. "Make the other root a copy of the booted root" is one standalone
action** (`recab copy`). It overwrites the other root with a fresh copy of the
booted one, including its boot files on the ESP. It works when the other root
is stale, damaged or cannot be mounted at all: then its filesystem is created
anew and filled. Anything written into the other root before is gone
afterwards.

**REQ-6. The boot default is the root that last finished starting.**
- Starting the other root is a one-time pick: the person picks it in the boot
  menu, or `recab boot-other` sets it for the next boot.
- When a root finishes starting, it becomes the boot default
  (`recab started`, run once per boot by the `recab-started` service).
- When it does not finish starting, the default is untouched, so the next boot
  starts the previous root. That is the only behaviour for a failed boot.

## 2. Commands

| Command | What it does |
|---|---|
| `recab status [--json]` | The booted root, the other root and where it is mounted, the boot default, any one-time pick. |
| `recab copy` | REQ-5. |
| `recab boot-other [--cancel]` | Pick the other root for the next boot only, or clear the pick. |
| `recab started` | REQ-6: wait for startup; when it finished and the booted root is not the default, make it the default. |
| `recab compare` | How alike the two roots are; reads only. |
| `recab health` | How startup ended; reads only. |
| `recab init` | Set up an installed system (REQ-4), or the one entry of a mutable install. |
| `recab verify-installation` | Check an installation before the installer finishes. |
| `recab install-unit` | Write the `recab-started` service and the read-only root's runtime policy into a package tree. |

With `--json` every command prints its result as JSON and a failure as one
JSON error on stderr; exit codes are stable per error class (13 permission,
20 firmware or profile unavailable, 21 refused, 22 partly done).

## 3. Layout

- Each root has its own `etc/fstab` (its root read-only, the shared `/var`)
  and its slot marker `usr/lib/recab/immutable`:
  `<A|B> PARTUUID=<var> PARTUUID=<other root>`. `copy` writes them right for
  the root it copies into.
- The ESP holds systemd-boot, `loader/loader.conf` and one entry and kernel
  directory per root (`<id>-a`, `<id>-b`). The entries are the same apart from
  the root they boot.
- The boot default lives in the boot loader: systemd-boot's
  `LoaderEntryDefault` variable and loader.conf's `default`, kept naming the
  same entry. The one-time pick is `LoaderEntryOneShot`.
- recab's only file is `/var/lib/recab/installation.json`, naming the
  partitions of root A and root B.

## 4. Boot

The installed initramfs (recinit) does REQ-1 and REQ-2 on an entry with
`recab.immutable`: it refuses `rootflags=rw`, sets the booted root's device
read-only and mounts it read-only, mounts the shared `/var` and the binds,
then makes the other root's device writable and mounts it read-write at
`/run/recab/other`. Anything wrong with the other root never stops the boot.

## 5. Mutable installs

A mutable install has one root and no A/B. recab must not be installed on it
at all; until the installer leaves it out, `recab init --mode mutable` writes
that root's one entry.
