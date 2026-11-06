//! recab against a scratch boot tree: slot entries whose `root=` names fake
//! devices, a `/proc/cmdline` stand-in, a fake `bootctl` that writes the
//! systemd-boot EFI variables in their real format, and a fake `systemctl`
//! whose startup state the test sets. Mutations need root.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let entries = root.join("boot/loader/entries");
        fs::create_dir_all(&entries).unwrap();
        fs::create_dir_all(root.join("efi/efivars")).unwrap();
        for slot in ["a", "b"] {
            fs::write(
                entries.join(format!("lev-{slot}.conf")),
                format!("title {slot}\nlinux /vmlinuz\noptions root=/dev/fake-{slot} rw\n"),
            )
            .unwrap();
        }
        fs::write(root.join("boot/vmlinuz"), "kernel").unwrap();
        fs::write(root.join("os-release"), "ID=lev\n").unwrap();
        let bootctl = root.join("bootctl");
        fs::write(
            &bootctl,
            format!(
                r#"#!/bin/sh
[ "$1" = --version ] && {{ echo 'bootctl 255'; exit 0; }}
echo "$*" >> '{root}/bootctl.log'
[ -e '{root}/fail-'"$1" ] && {{ echo simulated failure >&2; exit 1; }}
case "$1" in set-default) var=LoaderEntryDefault ;; set-oneshot) var=LoaderEntryOneShot ;; esac
file='{root}/efi/efivars/'$var-{GUID}
if [ -z "$2" ]; then rm -f "$file"; else {{ printf '\007\000\000\000'; printf '%s\000' "$2" | iconv -f UTF-8 -t UTF-16LE; }} > "$file"; fi
"#,
                root = root.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&bootctl, fs::Permissions::from_mode(0o755)).unwrap();
        let systemctl = root.join("systemctl");
        fs::write(
            &systemctl,
            format!(
                r#"#!/bin/sh
case "$*" in
  "is-system-running --wait") cat '{root}/system-state' ;;
  list-units*) cat '{root}/failed-units' 2>/dev/null ;;
  "--no-block reboot") echo reboot >> '{root}/reboot.log' ;;
esac
"#,
                root = root.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("system-state"), "running\n").unwrap();
        let tree = Self { dir };
        tree.boot_into("a");
        tree
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Simulate a boot into `slot`: the running root changes and systemd-boot
    /// consumes the one-shot entry.
    fn boot_into(&self, slot: &str) {
        fs::write(self.path("cmdline"), format!("quiet root=/dev/fake-{slot} ro\n")).unwrap();
        let _ = fs::remove_file(self.path(&format!("efi/efivars/LoaderEntryOneShot-{GUID}")));
    }

    fn recab(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_recab"))
            .args(args)
            .env("RECAB_BOOT_ROOT", self.path("boot"))
            .env("RECAB_STATE_PATH", self.path("state/state.json"))
            .env("RECAB_AUDIT_PATH", self.path("state/audit.log"))
            .env("RECAB_OS_RELEASE_PATH", self.path("os-release"))
            .env("RECAB_PROC_CMDLINE_PATH", self.path("cmdline"))
            .env("RECAB_SYS_FIRMWARE_EFI_PATH", self.path("efi"))
            .env("RECAB_EFIVARS_PATH", self.path("efi/efivars"))
            .env("RECAB_BOOTCTL_BIN", self.path("bootctl"))
            .env("RECAB_SYSTEMCTL_BIN", self.path("systemctl"))
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) {
        let out = self.recab(args);
        assert!(
            out.status.success(),
            "recab {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn status(&self) -> serde_json::Value {
        let out = self.recab(&["status", "--json"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn has_efivar(&self, name: &str) -> bool {
        self.path(&format!("efi/efivars/{name}-{GUID}")).exists()
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// status only reads, so it needs neither root nor UEFI.
#[test]
fn status_reads_the_running_slot_without_root_or_uefi() {
    let tree = Tree::new();
    fs::remove_dir_all(tree.path("efi")).unwrap();
    let status = tree.status();
    assert_eq!(status["active_slot"], "A");
    assert_eq!(status["entry_prefix"], "lev");
    assert_eq!(status["trial_pending"], false);
}

#[test]
fn mutations_refuse_without_root() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root");
        return;
    }
    let out = Tree::new().recab(&["commit"]);
    assert_eq!(
        out.status.code(),
        Some(13),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "needs root: recab mutations check for root"]
fn trial_commit_and_rollback_drive_the_firmware_variables() {
    let tree = Tree::new();
    tree.ok(&["commit"]);
    assert_eq!(tree.status()["default_slot"], "A");

    let out = tree.recab(&["set-next", "A"]);
    assert_eq!(out.status.code(), Some(21), "set-next on the running slot is refused");

    tree.ok(&["set-next", "B"]);
    assert!(tree.has_efivar("LoaderEntryOneShot"));
    assert_eq!(tree.status()["next_boot"], "B");

    tree.ok(&["rollback"]);
    assert!(
        !tree.has_efivar("LoaderEntryOneShot"),
        "rollback clears the pending trial"
    );
    assert_eq!(tree.status()["trial_pending"], false);

    tree.ok(&["set-next", "B"]);
    tree.boot_into("b");
    let status = tree.status();
    assert_eq!(status["active_slot"], "B");
    assert_eq!(status["default_slot"], "A");
    assert_eq!(status["trial_pending"], false, "the boot took the one-shot entry");
    assert_eq!(status["committed"], false);
    assert_eq!(status["next_boot"], "A", "an uncommitted trial falls back");
    tree.ok(&["commit"]);
    let status = tree.status();
    assert_eq!(status["default_slot"], "B");
    assert_eq!(status["known_good_slot"], "B");
    assert_eq!(status["committed"], true);
    assert_eq!(status["rollback_slot"], "A");
    assert!(read(&tree.path("boot/loader/loader.conf")).starts_with("default lev-b.conf\n"));

    // Rollback from a committed slot returns to the slot that was good before.
    tree.ok(&["rollback"]);
    assert_eq!(tree.status()["default_slot"], "A");
    tree.boot_into("a");
    let status = tree.status();
    assert_eq!(status["committed"], true);
    assert_eq!(status["rollback_slot"], "B");

    let ops: Vec<String> = read(&tree.path("state/audit.log"))
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["operation"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        ops,
        ["commit", "set-next", "rollback", "set-next", "commit", "rollback"]
    );
}

#[test]
#[ignore = "needs root: recab mutations check for root"]
fn set_next_refuses_a_slot_without_its_kernel() {
    let tree = Tree::new();
    tree.ok(&["commit"]);
    fs::write(
        tree.path("boot/loader/entries/lev-b.conf"),
        "title b\nlinux /lev-b/vmlinuz\noptions root=/dev/fake-b rw\n",
    )
    .unwrap();
    let out = tree.recab(&["set-next", "B"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(21), "{stderr}");
    assert!(stderr.contains("not bootable"), "{stderr}");
    assert!(!tree.has_efivar("LoaderEntryOneShot"));
}

/// A committed slot is not a trial: `trial` leaves it alone, without root.
#[test]
fn trial_leaves_a_committed_slot_alone() {
    let tree = Tree::new();
    fs::create_dir_all(tree.path("boot/loader")).unwrap();
    fs::write(tree.path("boot/loader/loader.conf"), "default lev-a.conf\n").unwrap();
    fs::write(tree.path("system-state"), "degraded\n").unwrap();
    tree.ok(&["trial", "--timeout", "5"]);
    assert!(!tree.path("reboot.log").exists());
}

#[test]
#[ignore = "needs root: recab mutations check for root"]
fn trial_commits_a_healthy_trial_and_reboots_out_of_a_failed_one() {
    let tree = Tree::new();
    tree.ok(&["commit"]);

    // Healthy: committed, no reboot.
    tree.ok(&["set-next", "B"]);
    tree.boot_into("b");
    let out = tree.recab(&["trial", "--timeout", "5"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("committed slot B"));
    let status = tree.status();
    assert_eq!(status["committed"], true);
    assert_eq!(status["default_slot"], "B");
    assert!(!tree.path("reboot.log").exists());

    // Degraded: not committed, reboot into the known-good slot B.
    tree.ok(&["set-next", "A"]);
    tree.boot_into("a");
    fs::write(tree.path("system-state"), "degraded\n").unwrap();
    fs::write(tree.path("failed-units"), "broken.service loaded failed failed Broken\n").unwrap();
    let out = tree.recab(&["trial", "--timeout", "5"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(23), "{stderr}");
    assert!(stderr.contains("broken.service") && stderr.contains("Rebooting into slot B"), "{stderr}");
    assert_eq!(read(&tree.path("reboot.log")), "reboot\n");
    let status = tree.status();
    assert_eq!(status["committed"], false);
    assert_eq!(status["next_boot"], "B");
}

#[test]
#[ignore = "needs root: recab mutations check for root"]
fn trial_that_never_finishes_starting_fails() {
    let tree = Tree::new();
    tree.ok(&["commit"]);
    tree.ok(&["set-next", "B"]);
    tree.boot_into("b");
    fs::write(
        tree.path("systemctl"),
        format!(
            "#!/bin/sh\ncase \"$*\" in \"is-system-running --wait\") sleep 30 ;; \"--no-block reboot\") echo reboot >> '{}' ;; esac\n",
            tree.path("reboot.log").display()
        ),
    )
    .unwrap();
    let out = tree.recab(&["trial", "--timeout", "1"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(23), "{stderr}");
    assert!(stderr.contains("did not finish starting"), "{stderr}");
    assert!(tree.path("reboot.log").exists());
}

#[test]
fn install_unit_writes_and_enables_the_trial_service() {
    let tree = Tree::new();
    tree.ok(&["install-unit", "--root", tree.path("target").to_str().unwrap()]);
    let unit = read(&tree.path("target/etc/systemd/system/recab-trial.service"));
    assert!(unit.contains("ExecStart=/usr/bin/recab trial"), "{unit}");
    let link = fs::read_link(tree.path(
        "target/etc/systemd/system/sysinit.target.wants/recab-trial.service",
    ))
    .unwrap();
    assert_eq!(link, Path::new("/etc/systemd/system/recab-trial.service"));
}

#[test]
#[ignore = "needs root: recab mutations check for root"]
fn failed_bootctl_restores_state_and_loader_conf() {
    let tree = Tree::new();
    tree.ok(&["commit"]);
    tree.ok(&["set-next", "B"]);
    let loader = read(&tree.path("boot/loader/loader.conf"));
    let state = read(&tree.path("state/state.json"));

    // Cancelling the trial fails at set-default: nothing changed.
    fs::write(tree.path("fail-set-default"), "").unwrap();
    let out = tree.recab(&["rollback"]);
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(read(&tree.path("boot/loader/loader.conf")), loader);
    assert_eq!(read(&tree.path("state/state.json")), state);
    assert!(tree.has_efivar("LoaderEntryOneShot"));

    // A failure after a firmware variable changed is a partial update.
    fs::remove_file(tree.path("fail-set-default")).unwrap();
    fs::write(tree.path("fail-set-oneshot"), "").unwrap();
    let out = tree.recab(&["rollback"]);
    assert_eq!(
        out.status.code(),
        Some(22),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
