use serde_json::Value;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tempfile::TempDir;

const DEVICE_ENV: &str = "RECAB_E2E_DEVICE";
const DEVICE_DEFAULT: &str = "/dev/sdc";
const ACK_ENV: &str = "RECAB_E2E_SDC_ACK";
const ACK_VALUE: &str = "ERASE_SDC";
const TRACE_OUT_ENV: &str = "RECAB_E2E_TRACE_OUT";
const BOOT_PREFIX: &str = "levitate";

struct TraceLog {
    path: PathBuf,
    lines: Vec<String>,
}

impl TraceLog {
    fn new(test_name: &str, device: &str) -> Self {
        let path = std::env::var(TRACE_OUT_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::temp_dir()
                    .join(format!("recab-{test_name}-{}.trace.md", std::process::id()))
            });
        let mut lines = Vec::new();
        lines.push(format!("# recab e2e trace: {test_name}"));
        lines.push(format!("- device: `{device}`"));
        lines.push(format!("- pid: `{}`", std::process::id()));
        lines.push(String::new());
        Self { path, lines }
    }

    fn step(&mut self, title: &str) {
        self.lines.push(format!("## {title}"));
        self.lines.push(String::new());
    }

    fn line(&mut self, text: impl Into<String>) {
        self.lines.push(text.into());
    }

    fn command_output(&mut self, command: &str, output: &Output) {
        self.line(format!("### `{command}`"));
        self.line(format!(
            "- exit: {}",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        ));
        self.line("- stdout:");
        self.lines.push("```text".to_string());
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            self.lines.push("<empty>".to_string());
        } else {
            self.lines.extend(stdout.lines().map(ToString::to_string));
        }
        self.lines.push("```".to_string());
        self.line("- stderr:");
        self.lines.push("```text".to_string());
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.trim().is_empty() {
            self.lines.push("<empty>".to_string());
        } else {
            self.lines.extend(stderr.lines().map(ToString::to_string));
        }
        self.lines.push("```".to_string());
        self.lines.push(String::new());
    }

    fn text_file(&mut self, label: &str, path: &Path) {
        self.line(format!("### file `{label}` (`{}`)", path.display()));
        self.lines.push("```text".to_string());
        if path.is_file() {
            let content = fs::read_to_string(path)
                .unwrap_or_else(|err| format!("<failed reading file: {err}>"));
            if content.trim().is_empty() {
                self.lines.push("<empty>".to_string());
            } else {
                self.lines.extend(content.lines().map(ToString::to_string));
            }
        } else {
            self.lines.push("<missing>".to_string());
        }
        self.lines.push("```".to_string());
        self.lines.push(String::new());
    }

    fn json_file(&mut self, label: &str, path: &Path) {
        self.line(format!("### json `{label}` (`{}`)", path.display()));
        self.lines.push("```json".to_string());
        if path.is_file() {
            let raw = fs::read_to_string(path)
                .unwrap_or_else(|err| format!("{{\"error\":\"failed reading file: {err}\"}}"));
            match serde_json::from_str::<Value>(&raw) {
                Ok(value) => {
                    let pretty = serde_json::to_string_pretty(&value).unwrap_or_else(|err| {
                        format!("{{\"error\":\"pretty print failed: {err}\"}}")
                    });
                    self.lines.extend(pretty.lines().map(ToString::to_string));
                }
                Err(err) => self
                    .lines
                    .push(format!("{{\"error\":\"invalid json: {err}\"}}")),
            }
        } else {
            self.lines.push("{\"missing\":true}".to_string());
        }
        self.lines.push("```".to_string());
        self.lines.push(String::new());
    }

    fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, self.lines.join("\n"))?;
        Ok(())
    }
}

impl Drop for TraceLog {
    fn drop(&mut self) {
        if let Err(err) = self.save() {
            eprintln!(
                "recab e2e trace write failed '{}': {err}",
                self.path.display()
            );
            return;
        }
        eprintln!("recab e2e trace: {}", self.path.display());
    }
}

struct E2eLayout {
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
    partuuid_a: String,
    partuuid_b: String,
}

fn sdc_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("sdc lock poisoned")
}

fn render_command(program: &str, args: &[&str]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(program.to_string());
    parts.extend(args.iter().map(|arg| arg.to_string()));
    parts.join(" ")
}

fn run_checked(trace: &mut TraceLog, program: &str, args: &[&str]) -> Output {
    let command_line = render_command(program, args);
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|_| panic!("failed to run command: {command_line}"));
    trace.command_output(&command_line, &output);
    assert!(
        output.status.success(),
        "command failed: {command_line}\nexit={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout_trimmed(output: &Output) -> String {
    String::from_utf8(output.stdout.clone())
        .expect("utf8 stdout")
        .trim()
        .to_string()
}

fn require_root_and_ack(trace: &mut TraceLog) -> String {
    let device = std::env::var(DEVICE_ENV).unwrap_or_else(|_| DEVICE_DEFAULT.to_string());
    let ack = std::env::var(ACK_ENV).unwrap_or_default();

    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    trace.line(format!("- euid: `{euid}`"));
    trace.line(format!("- target device: `{device}`"));
    trace.line(format!("- ack env present: `{}`", !ack.is_empty()));
    trace.lines.push(String::new());

    assert!(
        euid == 0,
        "destructive SDC e2e test requires root. Run with sudo."
    );
    assert!(
        ack == ACK_VALUE,
        "destructive SDC e2e test is gated.\n\
Set {}={} to acknowledge repartition/format of '{}'.",
        ACK_ENV,
        ACK_VALUE,
        device
    );

    device
}

fn assert_safe_target(device: &str, trace: &mut TraceLog) {
    let device_path = Path::new(device);
    assert!(
        device_path == Path::new(DEVICE_DEFAULT),
        "expected target device '{}', got '{}'",
        DEVICE_DEFAULT,
        device
    );
    let meta = fs::metadata(device_path)
        .unwrap_or_else(|_| panic!("target device '{}' does not exist", device));
    #[cfg(unix)]
    assert!(
        meta.file_type().is_block_device(),
        "target '{}' is not a block device",
        device
    );

    let root_source = run_checked(trace, "findmnt", &["-n", "-o", "SOURCE", "/"]);
    let root_source = stdout_trimmed(&root_source);
    assert!(
        !root_source.starts_with(device),
        "refusing destructive test: root filesystem source '{}' is on target '{}'",
        root_source,
        device
    );

    let mounts = run_checked(trace, "lsblk", &["-nr", "-o", "MOUNTPOINT", device]);
    let mounts_stdout = stdout_trimmed(&mounts);
    let mounted_points = mounts_stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert!(
        mounted_points.is_empty(),
        "refusing destructive test: '{}' has mounted paths: {:?}",
        device,
        mounted_points
    );
}

fn partition_and_format_sdc(device: &str, trace: &mut TraceLog) -> (String, String) {
    run_checked(trace, "parted", &["-s", device, "mklabel", "gpt"]);
    run_checked(
        trace,
        "parted",
        &["-s", device, "mkpart", "system-a", "ext4", "1MiB", "50%"],
    );
    run_checked(
        trace,
        "parted",
        &["-s", device, "mkpart", "system-b", "ext4", "50%", "100%"],
    );
    run_checked(trace, "partprobe", &[device]);

    let p1 = format!("{device}1");
    let p2 = format!("{device}2");
    wait_for_path(Path::new(&p1));
    wait_for_path(Path::new(&p2));

    run_checked(trace, "mkfs.ext4", &["-F", "-L", "system-a", &p1]);
    run_checked(trace, "mkfs.ext4", &["-F", "-L", "system-b", &p2]);

    let partuuid_a = stdout_trimmed(&run_checked(
        trace,
        "blkid",
        &["-s", "PARTUUID", "-o", "value", &p1],
    ));
    let partuuid_b = stdout_trimmed(&run_checked(
        trace,
        "blkid",
        &["-s", "PARTUUID", "-o", "value", &p2],
    ));

    assert!(!partuuid_a.is_empty(), "missing PARTUUID for {}", p1);
    assert!(!partuuid_b.is_empty(), "missing PARTUUID for {}", p2);
    (partuuid_a, partuuid_b)
}

fn wait_for_path(path: &Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for '{}'", path.display());
}

fn setup_layout(partuuid_a: &str, partuuid_b: &str) -> E2eLayout {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();

    let boot_root = root.join("boot");
    let entries = boot_root.join("loader/entries");
    fs::create_dir_all(&entries).expect("entries");
    fs::write(
        boot_root.join("loader/loader.conf"),
        format!("default {BOOT_PREFIX}-a.conf\ntimeout 5\neditor 0\n"),
    )
    .expect("loader.conf");
    fs::write(
        entries.join(format!("{BOOT_PREFIX}-a.conf")),
        format!(
            "title {} A\nlinux /vmlinuz\noptions root=PARTUUID={} ro console=ttyS0\n",
            BOOT_PREFIX, partuuid_a
        ),
    )
    .expect("entry a");
    fs::write(
        entries.join(format!("{BOOT_PREFIX}-b.conf")),
        format!(
            "title {} B\nlinux /vmlinuz\noptions root=PARTUUID={} ro console=ttyS0\n",
            BOOT_PREFIX, partuuid_b
        ),
    )
    .expect("entry b");

    let os_release_path = root.join("os-release");
    fs::write(&os_release_path, format!("ID={BOOT_PREFIX}\n")).expect("os-release");
    let proc_cmdline_path = root.join("proc-cmdline");
    fs::write(
        &proc_cmdline_path,
        format!("quiet root=PARTUUID={} ro console=ttyS0\n", partuuid_a),
    )
    .expect("proc-cmdline");

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
        fs::set_permissions(&bootctl_path, perms).expect("chmod");
    }

    E2eLayout {
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
        partuuid_a: partuuid_a.to_ascii_lowercase(),
        partuuid_b: partuuid_b.to_ascii_lowercase(),
    }
}

fn recab_cmd(layout: &E2eLayout) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_recab"));
    cmd.env("RECAB_BOOT_ROOT", &layout.boot_root)
        .env("RECAB_STATE_PATH", &layout.state_path)
        .env("RECAB_AUDIT_PATH", &layout.audit_path)
        .env("RECAB_OS_RELEASE_PATH", &layout.os_release_path)
        .env("RECAB_PROC_CMDLINE_PATH", &layout.proc_cmdline_path)
        .env("RECAB_SYS_FIRMWARE_EFI_PATH", &layout.efi_path)
        .env("RECAB_EFIVARS_PATH", &layout.efivars_path)
        .env("RECAB_BOOTCTL_BIN", &layout.bootctl_path)
        .env("RECAB_TEST_BOOTCTL_LOG", &layout.bootctl_log)
        .env("RECAB_ENTRY_PREFIX", BOOT_PREFIX);
    cmd
}

fn run_recab(layout: &E2eLayout, args: &[&str], trace: &mut TraceLog) -> Output {
    let output = recab_cmd(layout)
        .args(args)
        .output()
        .unwrap_or_else(|_| panic!("running recab {:?}", args));
    trace.command_output(&format!("recab {}", args.join(" ")), &output);
    output
}

fn run_recab_ok(layout: &E2eLayout, args: &[&str], trace: &mut TraceLog) -> Output {
    let output = run_recab(layout, args, trace);
    assert!(
        output.status.success(),
        "recab {:?} failed\nexit={:?}\nstdout={}\nstderr={}",
        args,
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn read_status_json(layout: &E2eLayout, trace: &mut TraceLog) -> Value {
    let output = run_recab_ok(layout, &["status", "--json"], trace);
    serde_json::from_slice(&output.stdout).expect("status json")
}

fn read_state_json(layout: &E2eLayout) -> Value {
    let raw = fs::read_to_string(&layout.state_path).expect("read state");
    serde_json::from_str(&raw).expect("parse state json")
}

fn snapshot_runtime_artifacts(layout: &E2eLayout, trace: &mut TraceLog, title: &str) {
    trace.step(title);
    trace.text_file("loader.conf", &layout.boot_root.join("loader/loader.conf"));
    trace.text_file(
        "entry-a",
        &layout
            .boot_root
            .join("loader/entries")
            .join(format!("{BOOT_PREFIX}-a.conf")),
    );
    trace.text_file(
        "entry-b",
        &layout
            .boot_root
            .join("loader/entries")
            .join(format!("{BOOT_PREFIX}-b.conf")),
    );
    trace.text_file("proc-cmdline", &layout.proc_cmdline_path);
    trace.json_file("state", &layout.state_path);
    trace.text_file("audit", &layout.audit_path);
    trace.text_file("bootctl-log", &layout.bootctl_log);
}

#[test]
#[ignore = "destructive e2e: repartitions and formats /dev/sdc"]
fn e2e_sdc_full_ab_lifecycle() {
    let _guard = sdc_lock();
    let mut trace = TraceLog::new("full-ab-lifecycle", DEVICE_DEFAULT);
    trace.step("preflight");
    let device = require_root_and_ack(&mut trace);
    assert_safe_target(&device, &mut trace);
    run_checked(
        &mut trace,
        "lsblk",
        &[
            "-o",
            "NAME,SIZE,TYPE,FSTYPE,LABEL,PARTUUID,MOUNTPOINT",
            &device,
        ],
    );

    trace.step("partition-and-format");
    let (partuuid_a, partuuid_b) = partition_and_format_sdc(&device, &mut trace);
    run_checked(
        &mut trace,
        "lsblk",
        &[
            "-o",
            "NAME,SIZE,TYPE,FSTYPE,LABEL,PARTUUID,MOUNTPOINT",
            &device,
        ],
    );

    trace.step("setup-recab-artifacts");
    let layout = setup_layout(&partuuid_a, &partuuid_b);
    snapshot_runtime_artifacts(&layout, &mut trace, "artifact-snapshot-initial");

    trace.step("status-initial");
    let status = read_status_json(&layout, &mut trace);
    assert_eq!(status["active_slot"], "A");
    assert_eq!(status["inactive_slot"], "B");
    assert_eq!(status["default_slot"], "A");
    assert_eq!(status["known_good_slot"], "A");
    assert_eq!(status["trial_pending"], false);

    trace.step("set-next-b");
    run_recab_ok(&layout, &["set-next", "B"], &mut trace);
    let state_after_set_next = read_state_json(&layout);
    assert_eq!(state_after_set_next["known_good_slot"], "A");
    assert_eq!(state_after_set_next["pending_trial_slot"], "B");
    snapshot_runtime_artifacts(&layout, &mut trace, "artifact-snapshot-after-set-next");

    trace.step("simulate-boot-on-b");
    fs::write(
        &layout.proc_cmdline_path,
        format!(
            "quiet root=PARTUUID={} ro console=ttyS0\n",
            layout.partuuid_b
        ),
    )
    .expect("set cmdline to B");
    let trial_status = read_status_json(&layout, &mut trace);
    assert_eq!(trial_status["active_slot"], "B");
    assert_eq!(trial_status["default_slot"], "A");
    assert_eq!(trial_status["trial_pending"], true);
    assert_eq!(trial_status["next_boot"], "B");

    trace.step("commit-on-b");
    run_recab_ok(&layout, &["commit"], &mut trace);
    let post_commit = read_status_json(&layout, &mut trace);
    assert_eq!(post_commit["active_slot"], "B");
    assert_eq!(post_commit["default_slot"], "B");
    assert_eq!(post_commit["known_good_slot"], "B");
    assert_eq!(post_commit["trial_pending"], false);
    assert_eq!(post_commit["next_boot"], Value::Null);
    let loader_conf =
        fs::read_to_string(layout.boot_root.join("loader/loader.conf")).expect("loader.conf");
    assert!(
        loader_conf
            .lines()
            .any(|line| line.trim() == "default levitate-b.conf"),
        "expected loader.conf default to levitate-b.conf, got:\n{}",
        loader_conf
    );
    snapshot_runtime_artifacts(&layout, &mut trace, "artifact-snapshot-after-commit");

    trace.step("simulate-boot-on-a-and-rollback");
    fs::write(
        &layout.proc_cmdline_path,
        format!(
            "quiet root=PARTUUID={} ro console=ttyS0\n",
            layout.partuuid_a
        ),
    )
    .expect("set cmdline to A");
    run_recab_ok(&layout, &["rollback"], &mut trace);
    let post_rollback = read_status_json(&layout, &mut trace);
    assert_eq!(post_rollback["active_slot"], "A");
    assert_eq!(post_rollback["default_slot"], "B");
    assert_eq!(post_rollback["known_good_slot"], "B");
    assert_eq!(post_rollback["trial_pending"], false);
    snapshot_runtime_artifacts(&layout, &mut trace, "artifact-snapshot-after-rollback");

    trace.step("assert-bootctl-and-audit");
    let bootctl_log = fs::read_to_string(&layout.bootctl_log).expect("bootctl log");
    assert!(bootctl_log.contains("set-oneshot levitate-b.conf"));
    assert!(bootctl_log.contains("set-default levitate-b.conf"));

    let audit_raw = fs::read_to_string(&layout.audit_path).expect("audit");
    let ops = audit_raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| {
            value
                .get("operation")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    assert!(
        ops.iter().any(|op| op == "set-next"),
        "audit missing set-next operation: {:?}",
        ops
    );
    assert!(
        ops.iter().any(|op| op == "commit"),
        "audit missing commit operation: {:?}",
        ops
    );
    assert!(
        ops.iter().any(|op| op == "rollback"),
        "audit missing rollback operation: {:?}",
        ops
    );
}

#[test]
#[ignore = "destructive e2e: repartitions and formats /dev/sdc"]
fn e2e_sdc_status_fails_when_cmdline_root_is_unknown() {
    let _guard = sdc_lock();
    let mut trace = TraceLog::new("unknown-root-selector", DEVICE_DEFAULT);
    trace.step("preflight");
    let device = require_root_and_ack(&mut trace);
    assert_safe_target(&device, &mut trace);

    trace.step("partition-and-format");
    let (partuuid_a, partuuid_b) = partition_and_format_sdc(&device, &mut trace);
    let layout = setup_layout(&partuuid_a, &partuuid_b);
    snapshot_runtime_artifacts(&layout, &mut trace, "artifact-snapshot-initial");

    trace.step("write-unknown-root-and-run-status");
    fs::write(
        &layout.proc_cmdline_path,
        "quiet root=PARTUUID=ffffffff-ffff-ffff-ffff-ffffffffffff ro\n",
    )
    .expect("write unknown partuuid");
    let output = run_recab(&layout, &["status"], &mut trace);
    assert_eq!(output.status.code(), Some(21));
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("unable to map active root selector"));
    snapshot_runtime_artifacts(&layout, &mut trace, "artifact-snapshot-after-failure");
}
