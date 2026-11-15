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
is stale, damaged or cannot be mounted or read at all: then its filesystem is
created anew and filled. Anything written into the other root before is gone
afterwards.
- Everything is read and checked before anything is written. A problem with
  the booted root, or a copy that does not fit, leaves the other root as it
  was. A new filesystem is made only when the other root cannot be mounted or
  read.
- The other root's boot entry is removed before the first write and written
  back as the last step, so a copy that did not finish cannot be started.
  Running `recab copy` again finishes it.
- Only the root filesystem is copied: every mount point inside a root keeps
  its folder, and nothing below it is copied or removed.

**REQ-6. The boot default is the root that last finished starting.**
- Starting the other root is a one-time pick: the person picks it in the boot
  menu, or `recab boot-other` sets it for the next boot.
- When a root finishes starting, it becomes the boot default
  (`recab started`, run once per boot by the `recab-started` service).
- When it does not finish starting, the default is untouched, so the next boot
  starts the previous root. When startup ends in the init's emergency state or
  does not finish within 30 minutes, and the booted root is not the default,
  `recab started` restarts the machine, so the default starts. When the booted
  root is the default, it changes nothing. That is the only behaviour for a
  failed boot. A root that fails inside the initramfs waits at its emergency
  shell until someone restarts it.

## 2. Commands

| Command | What it does |
|---|---|
| `recab status [--json]` | The booted root, the other root, where it is mounted and whether it can be started, the boot default, any one-time pick. |
| `recab copy` | REQ-5. |
| `recab boot-other [--cancel]` | Pick the other root for the next boot only, or clear the pick. |
| `recab started` | REQ-6: wait for startup; when it finished and the booted root is not the default, make it the default; when it did not finish, restart into the default. |
| `recab compare` | How alike the two roots are; reads only. |
| `recab health` | How startup ended; reads only. |
| `recab init` | Set up an installed system (REQ-4), or the one entry of a mutable install. It refuses an initramfs that is not recinit's installed init for this boot protocol. |
| `recab verify-installation` | Check an installation before the installer finishes. |
| `recab install-unit` | Write the `recab-started` service and the read-only root's runtime policy into a package tree. |

With `--json` every command prints its result as JSON and a failure as one
JSON error on stderr; exit codes are stable per error class (13 permission,
20 firmware or profile unavailable, 21 refused, 22 partly done).

## 3. Layout

- Each root has its own `etc/fstab` (its root read-only, the shared `/var`)
  and its root marker `usr/lib/recab/immutable` (three fields):
  `<A|B> PARTUUID=<var> PARTUUID=<other root>`. `copy` writes them right for
  the root it copies into.
- The ESP holds systemd-boot, `loader/loader.conf` and one entry and kernel
  directory per root (`<id>-a`, `<id>-b`). The entries are the same apart from
  the root they boot.
- The boot default lives in the boot loader: loader.conf's `default` and
  systemd-boot's `LoaderEntryDefault` variable, kept naming the same entry.
  loader.conf is written first; when the firmware refuses the variable, the
  variable is removed so loader.conf decides. The one-time pick is
  `LoaderEntryOneShot`.
- recab's only file is `/var/lib/recab/installation.json`, naming the
  partitions of root A and root B.

## 4. Boot

The installed initramfs (recinit) does REQ-1 and REQ-2 on an entry with
`recab.immutable`: it refuses `rootflags=rw`, sets the booted root's device
read-only and mounts it read-only, mounts the shared `/var` and the binds,
then makes the other root's device writable and mounts it read-write at
`/run/recab/other`. Its filesystem check may take at most 60 seconds. Anything
wrong with the other root never stops the boot.

## 5. Mutable installs

A mutable install has one root and no A/B. recab must not be installed on it
at all; until the installer leaves it out, `recab init --mode mutable` writes
that root's one entry.
