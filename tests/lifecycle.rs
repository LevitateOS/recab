//! recab against a scratch boot tree: slot entries whose `root=` names fake
//! devices, a `/proc/cmdline` stand-in, a fake `bootctl` that writes the
//! systemd-boot EFI variables in their real format, and a fake `systemctl`
//! whose startup state the test sets. No disk is touched.
//!
//! recab's mutations need root; unprivileged runs get it inside a user
//! namespace (`unshare --user --map-root-user`), which only affects the
//! scratch tree.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GUID: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

struct Tree {
    dir: tempfile::TempDir,
}

fn executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
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
        executable(
            &root.join("bootctl"),
            &format!(
                r#"#!/bin/sh
[ "$1" = --version ] && {{ echo 'bootctl 255'; exit 0; }}
case "$1" in --esp-path=*) shift ;; esac
echo "$*" >> '{root}/bootctl.log'
[ -e '{root}/fail-'"$1" ] && {{ echo simulated failure >&2; exit 1; }}
case "$1" in set-default) var=LoaderEntryDefault ;; set-oneshot) var=LoaderEntryOneShot ;; esac
file='{root}/efi/efivars/'$var-{GUID}
if [ -z "$2" ]; then rm -f "$file"; else {{ printf '\007\000\000\000'; printf '%s\000' "$2" | iconv -f UTF-8 -t UTF-16LE; }} > "$file"; fi
"#,
                root = root.display(),
            ),
        );
        executable(
            &root.join("systemctl"),
            &format!(
                r#"#!/bin/sh
case "$*" in
  "is-system-running --wait") cat '{root}/system-state' ;;
  list-units*) cat '{root}/failed-units' 2>/dev/null ;;
  "--no-block reboot") echo reboot >> '{root}/reboot.log' ;;
esac
"#,
                root = root.display(),
            ),
        );
        fs::write(root.join("system-state"), "running\n").unwrap();
        let tree = Self { dir };
        tree.boot_into("a");
        tree
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Simulate a boot into `slot`: the running root changes, systemd-boot
    /// records the entry it booted and consumes the one-shot entry.
    fn boot_into(&self, slot: &str) {
        fs::write(self.path("cmdline"), format!("quiet root=/dev/fake-{slot} ro\n")).unwrap();
        self.set_efivar("LoaderEntrySelected", Some(&format!("lev-{slot}.conf")));
        self.set_efivar("LoaderEntryOneShot", None);
    }

    fn set_efivar(&self, name: &str, value: Option<&str>) {
        let path = self.path(&format!("efi/efivars/{name}-{GUID}"));
        match value {
            None => {
                let _ = fs::remove_file(path);
            }
            Some(value) => {
                let mut bytes = vec![7, 0, 0, 0];
                for unit in value.encode_utf16().chain([0]) {
                    bytes.extend(unit.to_le_bytes());
                }
                fs::write(path, bytes).unwrap();
            }
        }
    }

    fn command(&self, as_root: bool) -> Command {
        // SAFETY: geteuid has no preconditions.
        let mut cmd = if as_root && unsafe { libc::geteuid() } != 0 {
            let mut cmd = Command::new("unshare");
            cmd.args(["--user", "--map-root-user", env!("CARGO_BIN_EXE_recab")]);
            cmd
        } else {
            Command::new(env!("CARGO_BIN_EXE_recab"))
        };
        cmd.env("RECAB_BOOT_ROOT", self.path("boot"))
            .env("RECAB_STATE_PATH", self.path("state/state.json"))
            .env("RECAB_AUDIT_PATH", self.path("state/audit.log"))
            .env("RECAB_LOCK_PATH", self.path("state/recab.lock"))
            .env("RECAB_OS_RELEASE_PATH", self.path("os-release"))
            .env("RECAB_PROC_CMDLINE_PATH", self.path("cmdline"))
            .env("RECAB_SYS_FIRMWARE_EFI_PATH", self.path("efi"))
            .env("RECAB_EFIVARS_PATH", self.path("efi/efivars"))
            .env("RECAB_BOOTCTL_BIN", self.path("bootctl"))
            .env("RECAB_SYSTEMCTL_BIN", self.path("systemctl"));
        cmd
    }

    /// Run recab as root (in a user namespace when the test is not root).
    fn recab(&self, args: &[&str]) -> Output {
        self.command(true).args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.recab(args);
        assert!(
            out.status.success(),
            "recab {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn fails(&self, args: &[&str], code: i32, needle: &str) {
        let out = self.recab(args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(code), "recab {args:?}: {stderr}");
        assert!(stderr.contains(needle), "recab {args:?}: expected '{needle}' in: {stderr}");
    }

    fn status(&self) -> serde_json::Value {
        let out = self.ok(&["status", "--json"]);
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn has_efivar(&self, name: &str) -> bool {
        self.path(&format!("efi/efivars/{name}-{GUID}")).exists()
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap_or_default()
    }

    /// A committed slot A with slot B scheduled and booted as a trial.
    fn trial_of_b() -> Self {
        let tree = Self::new();
        tree.ok(&["commit"]);
        tree.ok(&["set-next", "B"]);
        tree.boot_into("b");
        tree
    }
}

/// status only reads, so it needs neither root nor UEFI.
#[test]
fn status_reads_the_running_slot_without_root_or_uefi() {
    let tree = Tree::new();
    fs::remove_dir_all(tree.path("efi")).unwrap();
    let out = tree.command(false).args(["status", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
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
    let out = Tree::new().command(false).arg("commit").output().unwrap();
    assert_eq!(out.status.code(), Some(13), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn trial_commit_and_rollback_drive_the_firmware_variables() {
    let tree = Tree::new();
    tree.ok(&["commit"]);
    assert_eq!(tree.status()["default_slot"], "A");
    tree.fails(&["set-next", "A"], 21, "running slot");

    tree.ok(&["set-next", "B"]);
    assert!(tree.has_efivar("LoaderEntryOneShot"));
    assert_eq!(tree.status()["next_boot"], "B");
    assert!(tree.read("boot/loader/entries/lev-b.conf").contains("rd.emergency=reboot"));

    tree.ok(&["rollback"]);
    assert!(!tree.has_efivar("LoaderEntryOneShot"), "rollback clears the pending trial");
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
    assert_eq!(status["committed"], true);
    assert_eq!(status["rollback_slot"], "A");
    assert!(tree.read("boot/loader/loader.conf").starts_with("default lev-b.conf\n"));
    let entry = tree.read("boot/loader/entries/lev-b.conf");
    assert!(!entry.contains("rd.emergency=reboot"), "a default loses the trial options: {entry}");

    // Rollback from a committed slot returns to the slot that was good before.
    tree.ok(&["rollback"]);
    assert_eq!(tree.status()["default_slot"], "A");
    tree.boot_into("a");
    let status = tree.status();
    assert_eq!(status["committed"], true);
    assert_eq!(status["rollback_slot"], "B");

    // Committing what is already committed changes nothing.
    let audit = tree.read("state/audit.log");
    tree.ok(&["commit"]);
    assert_eq!(tree.read("state/audit.log"), audit);

    let ops: Vec<String> = tree
        .read("state/audit.log")
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["operation"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ops, ["commit", "set-next", "rollback", "set-next", "commit", "rollback"]);
}

#[test]
fn failed_bootctl_leaves_the_state_alone() {
    let tree = Tree::new();
    tree.ok(&["commit"]);
    tree.ok(&["set-next", "B"]);
    let loader = tree.read("boot/loader/loader.conf");
    let state = tree.read("state/state.json");

    // Cancelling the trial fails before anything changed.
    fs::write(tree.path("fail-set-oneshot"), "").unwrap();
    tree.fails(&["rollback"], 1, "unchanged");
    assert_eq!(tree.read("boot/loader/loader.conf"), loader);
    assert_eq!(tree.read("state/state.json"), state);
    assert!(tree.has_efivar("LoaderEntryOneShot"));

    // The one-shot is cleared first; set-default then fails: a partial
    // update whose next boot is the old, safe default.
    fs::remove_file(tree.path("fail-set-oneshot")).unwrap();
    fs::write(tree.path("fail-set-default"), "").unwrap();
    tree.fails(&["rollback"], 22, "one-shot boot was cleared");
    assert_eq!(tree.read("boot/loader/loader.conf"), loader, "loader.conf is put back");
    assert_eq!(tree.read("state/state.json"), state, "the state is written last");
    assert!(!tree.has_efivar("LoaderEntryOneShot"));
}

#[test]
fn set_next_refuses_a_slot_without_its_kernel() {
    let tree = Tree::new();
    tree.ok(&["commit"]);
    fs::write(
        tree.path("boot/loader/entries/lev-b.conf"),
        "title b\nlinux /lev-b/vmlinuz\noptions root=/dev/fake-b rw\n",
    )
    .unwrap();
    tree.fails(&["set-next", "B"], 21, "not bootable");
    assert!(!tree.has_efivar("LoaderEntryOneShot"));
}

/// A firmware default left by an earlier install, naming a slot without a
/// kernel, is skipped as systemd-boot skips it.
#[test]
fn a_default_naming_an_unbootable_slot_is_ignored() {
    let tree = Tree::new();
    fs::write(
        tree.path("boot/loader/entries/lev-b.conf"),
        "title b\nlinux /lev-b/vmlinuz\noptions root=/dev/fake-b rw\n",
    )
    .unwrap();
    fs::write(tree.path("boot/loader/loader.conf"), "default lev-a.conf\n").unwrap();
    tree.set_efivar("LoaderEntryDefault", Some("lev-b.conf"));
    let status = tree.status();
    assert_eq!(status["default_slot"], "A");
    assert_eq!(status["committed"], true);
}

#[test]
fn stage_refuses_while_its_slot_is_the_fallback_or_next_boot() {
    let tree = Tree::new();
    let args = ["stage", "--rootfs", "/nonexistent", "--kernel", "/nonexistent"];
    tree.fails(&args, 21, "running uncommitted");
    tree.ok(&["commit"]);
    tree.ok(&["set-next", "B"]);
    tree.fails(&args, 21, "next boot is slot B");
}

#[test]
fn trial_commits_a_healthy_trial() {
    let tree = Tree::trial_of_b();
    let out = tree.ok(&["trial", "--timeout", "5"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("committed slot B"));
    let status = tree.status();
    assert_eq!(status["committed"], true);
    assert_eq!(status["default_slot"], "B");
    assert_eq!(status["last_trial"]["committed"], true);
    assert!(!tree.path("reboot.log").exists());
}

#[test]
fn trial_reboots_out_of_a_degraded_trial() {
    let tree = Tree::trial_of_b();
    fs::write(tree.path("system-state"), "degraded\n").unwrap();
    fs::write(tree.path("failed-units"), "broken.service loaded failed failed Broken\n").unwrap();
    tree.fails(&["trial", "--timeout", "5"], 23, "Rebooting into slot A");
    assert_eq!(tree.read("reboot.log"), "reboot\n");
    let status = tree.status();
    assert_eq!(status["committed"], false);
    assert_eq!(status["next_boot"], "A");
    assert_eq!(status["last_trial"]["committed"], false);
    assert!(status["last_trial"]["detail"].as_str().unwrap().contains("broken.service"));
}

#[test]
fn trial_that_never_finishes_starting_fails() {
    let tree = Tree::trial_of_b();
    executable(
        &tree.path("systemctl"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in \"is-system-running --wait\") sleep 30 ;; \"--no-block reboot\") echo reboot >> '{}' ;; esac\n",
            tree.path("reboot.log").display()
        ),
    );
    tree.fails(&["trial", "--timeout", "1"], 23, "did not finish starting");
    assert!(tree.path("reboot.log").exists());
}

/// A committed slot, a slot picked from the boot menu, a trial an operator
/// rolled back, and a startup cut short by shutdown are not judged.
#[test]
fn trial_judges_only_the_trial_set_next_scheduled() {
    let committed = Tree::new();
    fs::write(committed.path("boot/loader/loader.conf"), "default lev-a.conf\n").unwrap();
    fs::write(committed.path("system-state"), "degraded\n").unwrap();
    committed.ok(&["trial", "--timeout", "5"]);

    let picked = Tree::new();
    picked.ok(&["commit"]);
    picked.boot_into("b");
    picked.ok(&["trial", "--timeout", "5"]);
    assert_eq!(picked.status()["committed"], false, "a menu pick is not committed");

    let rolled_back = Tree::trial_of_b();
    rolled_back.ok(&["rollback"]);
    rolled_back.ok(&["trial", "--timeout", "5"]);
    let status = rolled_back.status();
    assert_eq!(status["committed"], false, "the rollback stands");
    assert_eq!(status["default_slot"], "A");

    let stopping = Tree::trial_of_b();
    fs::write(stopping.path("system-state"), "stopping\n").unwrap();
    stopping.ok(&["trial", "--timeout", "5"]);

    for tree in [committed, picked, rolled_back, stopping] {
        assert!(!tree.path("reboot.log").exists());
    }
}

/// With the ESP unmounted the slots cannot be read, but the firmware says
/// this boot was a one-shot trial: a failed one still reboots.
#[test]
fn trial_without_the_esp_still_reboots_a_failed_trial() {
    let tree = Tree::trial_of_b();
    fs::remove_dir_all(tree.path("boot/loader/entries")).unwrap();
    fs::write(tree.path("system-state"), "maintenance\n").unwrap();
    tree.fails(&["trial", "--timeout", "5"], 23, "one-shot trial boot");
    assert!(tree.path("reboot.log").exists());
}

#[test]
fn install_unit_writes_and_enables_the_trial_service() {
    let tree = Tree::new();
    tree.ok(&["install-unit", "--root", tree.path("target").to_str().unwrap()]);
    let unit = tree.read("target/etc/systemd/system/recab-trial.service");
    assert!(unit.contains("ExecStart=/usr/bin/recab trial"), "{unit}");
    let link = fs::read_link(tree.path("target/etc/systemd/system/sysinit.target.wants/recab-trial.service")).unwrap();
    assert_eq!(link, Path::new("/etc/systemd/system/recab-trial.service"));
}

#[test]
fn init_sets_up_the_boot_layout_of_an_installed_system() {
    let tree = Tree::new();
    let target = tree.path("target");
    fs::create_dir_all(target.join("boot/loader")).unwrap();
    fs::create_dir_all(target.join("etc")).unwrap();
    fs::write(target.join("etc/os-release"), "NAME=\"Lev OS\"\nID=lev\n").unwrap();
    fs::write(tree.path("kernel"), "new kernel").unwrap();
    fs::write(tree.path("initrd"), "new initrd").unwrap();
    fs::create_dir_all(tree.path("bin")).unwrap();
    executable(
        &tree.path("bin/blkid"),
        "#!/bin/sh\ncase \"$*\" in *part-a) echo AAAA-1 ;; *part-b) echo BBBB-2 ;; esac\n",
    );
    tree.set_efivar("LoaderEntryDefault", Some("lev-b.conf"));

    let out = tree
        .command(true)
        .env("PATH", format!("{}:{}", tree.path("bin").display(), std::env::var("PATH").unwrap()))
        .args(["init", "--root", target.to_str().unwrap(), "--slot-a", "/dev/part-a", "--slot-b", "/dev/part-b"])
        .args(["--kernel", tree.path("kernel").to_str().unwrap(), "--initrd", tree.path("initrd").to_str().unwrap()])
        .args(["--options", "console=ttyS0"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let a = tree.read("target/boot/loader/entries/lev-a.conf");
    assert!(a.contains("title Lev OS (Slot A)\nlinux /lev-a/vmlinuz\ninitrd /lev-a/initramfs.img\n"), "{a}");
    assert!(a.contains("root=PARTUUID=aaaa-1 ro rootwait panic=10 console=ttyS0"), "{a}");
    assert!(tree.read("target/boot/loader/entries/lev-b.conf").contains("root=PARTUUID=bbbb-2"));
    assert_eq!(tree.read("target/boot/lev-a/vmlinuz"), "new kernel");
    assert!(!tree.path("target/boot/lev-b").exists(), "slot B has no kernel until stage");
    assert!(tree.read("target/boot/loader/loader.conf").starts_with("default lev-a.conf\n"));
    let state: serde_json::Value = serde_json::from_str(&tree.read("target/var/lib/recab/state.json")).unwrap();
    assert_eq!(state["known_good_slot"], "A");
    assert!(tree.path("target/etc/systemd/system/sysinit.target.wants/recab-trial.service").symlink_metadata().is_ok());
    // The stale firmware default from an earlier install now names slot A.
    assert!(tree.read("bootctl.log").contains("set-default lev-a.conf"));
}

/// Fields a newer recab wrote survive an older recab's write.
#[test]
fn state_keeps_fields_it_does_not_know() {
    let tree = Tree::new();
    fs::create_dir_all(tree.path("state")).unwrap();
    fs::write(
        tree.path("state/state.json"),
        r#"{"schema": 2, "known_good_slot": "A", "future_field": [1, 2]}"#,
    )
    .unwrap();
    tree.ok(&["commit"]);
    tree.ok(&["set-next", "B"]);
    let state: serde_json::Value = serde_json::from_str(&tree.read("state/state.json")).unwrap();
    assert_eq!(state["schema"], 2);
    assert_eq!(state["future_field"], serde_json::json!([1, 2]));
    assert_eq!(state["pending_trial_slot"], "B");
}
