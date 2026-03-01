use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct TestLayout {
    _tmp: TempDir,
    boot_root: PathBuf,
    state_path: PathBuf,
    audit_path: PathBuf,
    os_release_path: PathBuf,
    proc_cmdline_path: PathBuf,
    efi_path: PathBuf,
    efivars_path: PathBuf,
    bootctl_path: PathBuf,
    bootctl_log: PathBuf,
}

fn setup_layout() -> TestLayout {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();

    let boot_root = root.join("boot");
    let entries = boot_root.join("loader/entries");
    fs::create_dir_all(&entries).expect("entries");

    fs::write(
        boot_root.join("loader/loader.conf"),
        "default iuppiter-a.conf\ntimeout 5\neditor 0\n",
    )
    .expect("loader.conf");
    fs::write(entries.join("iuppiter-a.conf"), "title Iuppiter A\n").expect("entry a");
    fs::write(entries.join("iuppiter-b.conf"), "title Iuppiter B\n").expect("entry b");

    let os_release_path = root.join("os-release");
    fs::write(&os_release_path, "ID=iuppiter\n").expect("os-release");
    let proc_cmdline_path = root.join("proc-cmdline");
    fs::write(&proc_cmdline_path, "quiet root=LABEL=system-a ro\n").expect("proc-cmdline");

    let efi_path = root.join("sys/firmware/efi");
    let efivars_path = efi_path.join("efivars");
    fs::create_dir_all(&efivars_path).expect("efivars");

    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir).expect("bin dir");
    let bootctl_path = bin_dir.join("bootctl");
    let bootctl_log = root.join("bootctl.log");
    fs::write(
        &bootctl_path,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"bootctl 255\"; exit 0; fi\nif [ -n \"$RECAB_TEST_BOOTCTL_LOG\" ]; then printf '%s\\n' \"$*\" >> \"$RECAB_TEST_BOOTCTL_LOG\"; fi\nexit 0\n",
    )
    .expect("bootctl script");

    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&bootctl_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bootctl_path, perms).expect("set executable bit");
    }

    TestLayout {
        _tmp: tmp,
        boot_root,
        state_path: root.join("state.json"),
        audit_path: root.join("audit.log"),
        os_release_path,
        proc_cmdline_path,
        efi_path,
        efivars_path,
        bootctl_path,
        bootctl_log,
    }
}

fn recab_cmd(layout: &TestLayout) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_recab"));
    cmd.env("RECAB_BOOT_ROOT", &layout.boot_root)
        .env("RECAB_STATE_PATH", &layout.state_path)
        .env("RECAB_AUDIT_PATH", &layout.audit_path)
        .env("RECAB_OS_RELEASE_PATH", &layout.os_release_path)
        .env("RECAB_PROC_CMDLINE_PATH", &layout.proc_cmdline_path)
        .env("RECAB_SYS_FIRMWARE_EFI_PATH", &layout.efi_path)
        .env("RECAB_EFIVARS_PATH", &layout.efivars_path)
        .env("RECAB_BOOTCTL_BIN", &layout.bootctl_path)
        .env("RECAB_TEST_BOOTCTL_LOG", &layout.bootctl_log);
    cmd
}

fn command_output(layout: &TestLayout, args: &[&str]) -> std::process::Output {
    recab_cmd(layout)
        .args(args)
        .output()
        .unwrap_or_else(|_| panic!("running recab {:?}", args))
}

fn stdout_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf8 stdout")
}

fn stderr_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf8 stderr")
}

fn assert_exit_code(output: &std::process::Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit code; stdout='{}' stderr='{}'",
        stdout_string(output),
        stderr_string(output)
    );
}

fn read_json_file(path: &Path) -> Value {
    let raw = fs::read_to_string(path).unwrap_or_else(|_| panic!("read '{}'", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|_| panic!("parse json '{}'", path.display()))
}

#[test]
fn status_json_reports_required_fields() {
    let layout = setup_layout();
    let output = command_output(&layout, &["status", "--json"]);
    assert!(output.status.success(), "stderr={}", stderr_string(&output));

    let value: Value = serde_json::from_slice(&output.stdout).expect("status json");
    assert_eq!(value["active_slot"], "A");
    assert_eq!(value["inactive_slot"], "B");
    assert_eq!(value["trial_pending"], false);
    assert_eq!(value["next_boot"], Value::Null);
    assert_eq!(value["default_slot"], "A");
}

#[test]
fn status_human_output_is_readable() {
    let layout = setup_layout();
    let output = command_output(&layout, &["status"]);
    assert!(output.status.success(), "stderr={}", stderr_string(&output));

    let stdout = stdout_string(&output);
    assert!(stdout.contains("active: A"));
    assert!(stdout.contains("inactive: B"));
    assert!(stdout.contains("trial_pending: no"));
}

#[test]
fn status_fails_with_backend_error_when_uefi_runtime_missing() {
    let layout = setup_layout();
    let missing_efi = layout.boot_root.join("missing-efi");
    let output = recab_cmd(&layout)
        .env("RECAB_SYS_FIRMWARE_EFI_PATH", &missing_efi)
        .arg("status")
        .output()
        .expect("run status");
    assert_exit_code(&output, 20);
    assert!(stderr_string(&output).contains("UEFI runtime"));
}

#[test]
fn status_reflects_seeded_trial_state() {
    let layout = setup_layout();
    fs::write(
        &layout.state_path,
        serde_json::to_string_pretty(&json!({
            "schema": 1,
            "known_good_slot": "A",
            "pending_trial_slot": "B",
            "last_operation": "set-next-B",
            "last_transition_unix": 1700000000u64
        }))
        .expect("serialize"),
    )
    .expect("write state");

    let output = command_output(&layout, &["status", "--json"]);
    assert!(output.status.success(), "stderr={}", stderr_string(&output));

    let value: Value = serde_json::from_slice(&output.stdout).expect("status json");
    assert_eq!(value["trial_pending"], true);
    assert_eq!(value["next_boot"], "B");
    assert_eq!(value["known_good_slot"], "A");
}

#[test]
fn set_next_respects_root_requirement_and_updates_state_when_root() {
    let layout = setup_layout();
    let output = command_output(&layout, &["set-next", "B"]);
    let uid = unsafe { libc::geteuid() };

    if uid != 0 {
        assert_exit_code(&output, 13);
        return;
    }

    assert!(output.status.success(), "stderr={}", stderr_string(&output));
    let state = read_json_file(&layout.state_path);
    assert_eq!(state["pending_trial_slot"], "B");
    assert_eq!(state["known_good_slot"], "A");

    let bootctl_log = fs::read_to_string(&layout.bootctl_log).expect("bootctl log");
    assert!(bootctl_log.contains("set-oneshot iuppiter-b.conf"));
}
