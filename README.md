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
```

To change the system: write into `/run/recab/other` (Recipe does this), then
`recab boot-other` and reboot. If the other root finishes starting, it becomes
the boot default and the root you came from is now the unlocked one. If it
does not, the machine starts the previous root again. To start over from the
running system, run `recab copy`: it overwrites the other root, and creates
its filesystem anew when the old one cannot be mounted or copied into.

`recab compare` shows how the two roots differ. `recab health` shows how
startup ended.

## Installing

The installer runs:

```sh
recab init --root /mnt --slot-a /dev/sda2 --slot-b /dev/sda3 \
    --kernel /mnt/boot-files/vmlinuz --initrd /mnt/boot-files/initramfs.img
recab --json verify-installation --root /mnt
```

with root A mounted at `/mnt`, its ESP at `/mnt/boot` and the shared `/var`
partition at `/mnt/var`. `init` sets root A up for the locked boot (fstab,
slot marker, runtime policy), copies it to root B, writes systemd-boot, both
entries and their kernels, and makes A the default.

`recab install-unit --root <tree> --init systemd|openrc` writes the
`recab-started` service, enabled, into recab's package tree. It runs
`recab started` once per boot.

## What is where

| | |
|---|---|
| `/var/lib/recab/installation.json` | the partitions of root A and root B; recab's only file |
| `/usr/lib/recab/immutable` (each root) | slot marker: `<A\|B> PARTUUID=<var> PARTUUID=<other root>` |
| ESP `loader/entries/<id>-a.conf`, `<id>-b.conf` | one entry per root, the same apart from the root |
| ESP `<id>-a/`, `<id>-b/` | each root's kernel and initramfs |
| EFI `LoaderEntryDefault`, loader.conf `default` | the boot default |
| EFI `LoaderEntryOneShot` | the one-time pick |

## Code

| File | |
|---|---|
| `src/main.rs` | the CLI |
| `src/ab.rs` | `status`, `copy`, `boot-other`, `started` |
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
