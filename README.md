# recab

recab manages the two root filesystems of an A/B installation, root A and
root B. [REQUIREMENTS.md](REQUIREMENTS.md) is the contract.

- The booted root is locked: read-only, its block device read-only.
- The other root is unlocked: writable, mounted read-write at
  `/run/recab/other`. Changes are made there.
- Boot the other root and the roles swap. Nothing else decides which is which.
- The boot default is the root that last finished starting. A root that does
  not finish starting is not booted again by default: the next boot starts
  the previous root.

## Using it

```sh
recab status          # which root is booted, the other one, what boots next
recab copy            # make the other root a copy of the booted root
recab boot-other      # start the other root at the next boot, once
recab boot-other --cancel
recab update          # install the newest release into the other root, start it once
```

To change the system: write into `/run/recab/other` (Recipe does this), then
`recab boot-other` and reboot. If the other root finishes starting, it becomes
the boot default and the root you came from is now the unlocked one. If it
does not, the machine starts the previous root again. To start over from the
running system, run `recab copy`: it overwrites the other root, and creates
its filesystem anew when the old one cannot be mounted or read. While a copy
runs, the other root has no boot entry; if the copy stops, `recab status`
says the other root cannot be started and `recab copy` finishes it. Mount
points inside a root are left alone. If the other root ends in the emergency
state or does not finish starting within 30 minutes, `recab started` restarts
the machine into the boot default.

`recab compare` shows how the two roots differ. `recab health` shows how
startup ended.

## Updates

`recab update [--json]` (root, A/B only) runs five steps, and nothing in them
touches the booted root, its boot entry or the boot default:

1. `recupdate fetch --json` downloads and verifies the release into
   `/var/cache/recupdate/<serial>/`; recab checks `boot.tar.zst` against that
   `release.json` (SHA-256) and unpacks it. The archive holds exactly two
   regular files at its top level, `vmlinuz` and `initramfs.img`, the same
   files `init` takes from the install medium.
2. `recab copy`. The other root's boot entry is then removed again.
3. `recipe --system apply-release <folder>` writes the release into the other
   root.
4. The other root's kernel, initramfs and entry are written, with the kernel
   options the booted root's entry has.
5. `recab boot-other`. If the other root finishes starting it becomes the
   default; if not, the machine starts the previous root again.

A failure in steps 2–4 leaves the other root without a boot entry
(`other_bootable` false), so it cannot be started; the next `recab update` or
`recab copy` overwrites it. With `--json`, stdout carries the JSON lines of
`docs/release-engineering.md` §10 (`step`, the programs' `progress`, `done`,
`failed`). `RECAB_RECUPDATE_BIN` and `RECAB_RECIPE_BIN` name other programs
than `recupdate` and `recipe` on `PATH`; `RECUPDATE_CACHE_DIR` moves the
release folder as it does for recupdate. The whole run holds the recab lock.

Not done: `recab update` has never been run, against stand-ins or a real
release.

## Installing

The installer runs:

```sh
recab init --root /mnt --slot-a /dev/sda2 --slot-b /dev/sda3 \
    --kernel /mnt/boot-files/vmlinuz --initrd /mnt/boot-files/initramfs.img
recab --json verify-installation --root /mnt
```

with root A mounted at `/mnt`, its ESP at `/mnt/boot` and the shared `/var`
partition at `/mnt/var`. `init` sets root A up for the locked boot (fstab,
root marker, runtime policy), copies it to root B, writes systemd-boot, both
entries and their kernels, and makes A the default.

`recab install-unit --root <tree> --init systemd|openrc` writes the
`recab-started` service, enabled, into a system; recsetup's A/B boot step runs
it on the target, before `init`. It runs `recab started` once per boot.

## What is where

| | |
|---|---|
| `/var/lib/recab/installation.json` | the partitions of root A and root B; recab's only file |
| `/usr/lib/recab/immutable` (each root) | root marker, three fields: `<A\|B> PARTUUID=<var> PARTUUID=<other root>` |
| ESP `loader/entries/<id>-a.conf`, `<id>-b.conf` | one entry per root, the same apart from the root |
| ESP `<id>-a/`, `<id>-b/` | each root's kernel and initramfs |
| EFI `LoaderEntryDefault`, loader.conf `default` | the boot default |
| EFI `LoaderEntryOneShot` | the one-time pick |

## Code

| File | |
|---|---|
| `src/main.rs` | the CLI |
| `src/contract.rs`, `src/error.rs` | the JSON Schemas of `--json` output (`cargo xtask gen`), the error classes |
| `src/filesystem.rs`, `src/target.rs` | file access confined to one root, the system a run looks at |
| `src/ab.rs` | `status`, `copy`, `boot-other`, `started` |
| `src/update.rs` | `update` |
| `src/installation.rs`, `src/init.rs` | the installation record, `init`, `verify-installation` |
| `src/boot.rs`, `src/entry.rs`, `src/efivars.rs` | the ESP, entries, loader.conf and the boot loader variables |
| `src/other.rs` | finding, unlocking, mounting and recreating the other root |
| `src/sync.rs`, `src/inventory.rs` | the exact file-level copy |
| `src/immutable.rs`, `src/runtime.rs`, `src/fstab.rs` | what a root needs for the locked boot |
| `src/init_system.rs` | how startup ended (systemd, OpenRC) |
| `src/compare.rs` | `compare` |
| `src/package.rs`, `src/services/` | `install-unit` and the `recab-started` service |

The installed initramfs that locks and unlocks the roots at boot is recinit's
`templates/init_installed.template`.
