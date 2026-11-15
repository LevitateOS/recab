use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use recab::{stable_exit_code, Health, InitLayout, InitRequest, Status};
use recpart::types::InstallMode;

#[derive(Parser)]
#[command(name = "recab")]
#[command(
    about = "Manage the two root filesystems A and B: the booted root is locked, the other is unlocked"
)]
#[command(version)]
#[command(
    after_help = "The booted root is locked: mounted read-only. The other root is unlocked:\n\
mounted read-write at /run/recab/other, where changes are made. Boot the other\n\
root and the roles swap. The boot default is the root that last finished\n\
starting, so a root that does not finish starting is not booted again by default.\n\n\
Common tasks:\n  recab status        Which root is booted, which is the other, what boots next\n  \
recab copy          Make the other root a copy of the booted root\n  \
recab boot-other    Start the other root at the next boot, once"
)]
struct Cli {
    /// Print the result as JSON; a failed command prints one JSON error on stderr.
    #[arg(long, global = true)]
    json: bool,
    /// Emit start/finish/failure JSON lines on stderr; stdout remains the command result.
    #[arg(long, global = true)]
    machine_events: bool,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show which root is booted (locked), which is the other (unlocked) and
    /// where it is mounted, the boot default and any one-time pick.
    Status,

    /// Make the other root a copy of the booted root, boot files included.
    /// Creates its filesystem anew when it cannot be mounted or copied into.
    Copy,

    /// Start the other root at the next boot, once.
    BootOther {
        /// Clear the one-time pick instead.
        #[arg(long)]
        cancel: bool,
    },

    /// Run once per boot by recab-started: wait for startup to finish and,
    /// if it finished, make the booted root the boot default.
    Started {
        /// Seconds to wait for startup to finish.
        #[arg(long, default_value_t = recab::STARTUP_TIMEOUT_SECS)]
        timeout: u64,
    },

    /// Read the running system and the other root and say how alike they
    /// are; changes neither.
    Compare {
        /// Compare two system trees mounted at these paths instead of this
        /// computer's two roots: the one taken as running, then the other.
        #[arg(long, num_args = 2, value_names = ["RUNNING", "OTHER"], conflicts_with = "machine_events")]
        roots: Option<Vec<PathBuf>>,
    },

    /// Wait for startup to finish and report how it ended (read-only; needs
    /// no A/B layout).
    Health {
        /// Seconds to wait for startup to finish.
        #[arg(long, default_value_t = recab::STARTUP_TIMEOUT_SECS)]
        timeout: u64,
    },

    /// Set up a freshly installed system: for A/B, root B becomes a copy of
    /// root A, both boot entries are written and A is the default.
    Init {
        /// The installed system's root, with its ESP at boot/ and its /var partition at var/.
        #[arg(long)]
        root: PathBuf,
        /// The disk layout: two roots A and B, or one mutable root.
        #[arg(long, value_enum, default_value_t = InstallMode::Ab)]
        mode: InstallMode,
        /// A/B: root A's partition, holding the installed system.
        #[arg(long)]
        slot_a: Option<PathBuf>,
        /// A/B: root B's partition, unmounted; it becomes a copy of root A.
        #[arg(long)]
        slot_b: Option<PathBuf>,
        /// Mutable: the root partition.
        #[arg(long)]
        root_device: Option<PathBuf>,
        /// Kernel image of the installed system.
        #[arg(long)]
        kernel: PathBuf,
        /// Its initramfs (A/B needs the installed one).
        #[arg(long)]
        initrd: Option<PathBuf>,
        /// Extra kernel options for the entries (for example consoles).
        #[arg(long, default_value = "")]
        options: String,
    },

    /// Check an A/B installation before finishing: both roots, both boot
    /// entries and A the default.
    VerifyInstallation {
        /// The installed system's root, mounted as for init.
        #[arg(long)]
        root: PathBuf,
    },

    /// Write recab's package contents into a tree: the recab-started service
    /// (systemd unit or OpenRC service), enabled, and the read-only root's
    /// runtime policy.
    InstallUnit {
        /// Root of the tree (a package tree, or a system).
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// The init system to write the service for.
        #[arg(long, value_name = "INIT", value_parser = str::parse::<recab::InitSystem>)]
        init: recab::InitSystem,
        /// Where recab is installed in the tree, as the service runs it.
        #[arg(long, value_name = "PATH", default_value = recab::RECAB_BIN)]
        bin: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    let events = cli.machine_events;
    let operation = match &cli.cmd {
        Command::Status => "status",
        Command::Copy => "copy",
        Command::BootOther { .. } => "boot-other",
        Command::Started { .. } => "started",
        Command::Compare { .. } => "compare",
        Command::Health { .. } => "health",
        Command::Init { .. } => "init",
        Command::VerifyInstallation { .. } => "verify-installation",
        Command::InstallUnit { .. } => "install-unit",
    };
    let event = |kind: &str, message: String, code: Option<u8>| {
        if events {
            let line = recab::Event {
                schema_version: 1,
                event: kind.into(),
                operation: operation.into(),
                message,
                code,
            };
            eprintln!(
                "{}",
                serde_json::to_string(&line).expect("plain data serializes")
            );
        }
    };
    event("started", format!("Starting {operation}"), None);
    match run(cli) {
        Ok(()) => {
            event("finished", format!("Completed {operation}"), Some(0));
            ExitCode::SUCCESS
        }
        Err(err) => {
            event("failed", format!("{err:#}"), Some(stable_exit_code(&err)));
            if json {
                eprintln!("{}", recab::ErrorPayload::of(&err).to_json());
            } else {
                eprintln!("recab: {}", recab::error_text(&err));
            }
            ExitCode::from(stable_exit_code(&err))
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let json = cli.json;
    match cli.cmd {
        Command::Status => {
            let status = recab::status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
        }
        Command::Copy => {
            let copied = recab::copy(json || cli.machine_events)?;
            if json {
                println!("{}", serde_json::to_string(&copied)?);
            } else {
                let r = &copied.report;
                println!(
                    "Root {} is now a copy of root {}{}: {} bytes written in {} ranges, {} entries removed, {} metadata updates.",
                    copied.to,
                    copied.from,
                    if copied.recreated { " (its filesystem was created anew)" } else { "" },
                    r.payload_bytes_written,
                    r.range_writes,
                    r.entries_removed,
                    r.metadata_updates
                );
            }
        }
        Command::BootOther { cancel } => {
            let other = recab::boot_other(cancel)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"next_boot_once": if cancel { None } else { Some(other) }})
                );
            } else if cancel {
                println!("No root is picked for the next boot; the boot default starts.");
            } else {
                println!("Root {other} starts at the next boot, once. Reboot when ready.");
            }
        }
        Command::Started { timeout } => {
            let started = recab::started(Duration::from_secs(timeout))?;
            if json {
                println!("{}", serde_json::to_string(&started)?);
            } else {
                println!(
                    "recab: root {} {}{}",
                    started.booted,
                    started.health.describe(Duration::from_secs(timeout)),
                    if started.made_default {
                        "; it is now the boot default"
                    } else if started.restarting {
                        "; restarting into the boot default"
                    } else {
                        ""
                    }
                );
            }
        }
        Command::Compare { roots } => {
            let comparison = match roots.as_deref() {
                Some([running, other]) => recab::compare_roots(running, other)?,
                _ => recab::compare(cli.machine_events)?,
            };
            if json {
                println!("{}", serde_json::to_string(&comparison)?);
            } else {
                print_comparison(&comparison);
            }
        }
        Command::Health { timeout } => {
            let health = recab::health(Duration::from_secs(timeout))?;
            if json {
                println!("{}", serde_json::to_string(&health)?);
            } else {
                print_health(&health, Duration::from_secs(timeout));
            }
        }
        Command::Init {
            root,
            mode,
            slot_a,
            slot_b,
            root_device,
            kernel,
            initrd,
            options,
        } => {
            let layout = match (mode, slot_a, slot_b, root_device) {
                (InstallMode::Ab, Some(slot_a), Some(slot_b), None) => {
                    InitLayout::Ab { slot_a, slot_b }
                }
                (InstallMode::Mutable, None, None, Some(root_device)) => {
                    InitLayout::Mutable { root_device }
                }
                _ => usage(
                    "--mode ab takes --slot-a and --slot-b; --mode mutable takes --root-device",
                ),
            };
            recab::init(&InitRequest {
                root,
                layout,
                kernel,
                initrd,
                options,
            })?
        }
        Command::VerifyInstallation { root } => println!(
            "{}",
            serde_json::to_string(&recab::verify_installation(&root)?)?
        ),
        Command::InstallUnit { root, init, bin } => recab::install_unit(&root, init, &bin)?,
    }
    Ok(())
}

/// A command-line usage error, reported the way clap reports its own (exit 2).
fn usage(message: &str) -> ! {
    Cli::command()
        .error(clap::error::ErrorKind::ArgumentConflict, message)
        .exit()
}

/// `Status` as text: every field (no `..`, so a new field cannot be left out).
fn print_status(status: &Status) {
    let Status {
        schema_version: _,
        booted,
        other,
        other_mount,
        other_bootable,
        boot_default,
        next_boot_once,
    } = status;
    println!("Booted root (locked, read-only): {booted}");
    match other_mount {
        Some(point) => println!("Other root (unlocked, read-write): {other}, mounted at {point}"),
        None => println!("Other root (unlocked): {other}, not mounted"),
    }
    if !other_bootable {
        println!(
            "Root {other} cannot be started: it has no complete boot entry (a copy did not finish; run 'recab copy')"
        );
    }
    match boot_default {
        Some(slot) => println!("Boot default: {slot}"),
        None => println!("Boot default: neither root"),
    }
    match next_boot_once {
        Some(slot) => println!("Next boot, once: {slot}"),
        None => println!("Next boot, once: nothing picked"),
    }
}

fn print_comparison(comparison: &recab::Comparison) {
    let differing = comparison.differences.len() as u64 + comparison.more;
    println!(
        "Running system: {} files, {} bytes",
        comparison.running.files, comparison.running.bytes
    );
    println!(
        "Other system:   {} files, {} bytes",
        comparison.other.files, comparison.other.bytes
    );
    println!(
        "The same in both: {} files, {} bytes",
        comparison.same.files, comparison.same.bytes
    );
    if differing == 0 {
        println!("Nothing differs.");
        return;
    }
    println!("Differences: {differing}");
    for difference in &comparison.differences {
        let change = match difference.change {
            recab::DifferenceKind::OnlyRunning => "only in the running system",
            recab::DifferenceKind::OnlyOther => "only in the other system",
            recab::DifferenceKind::Content => "content differs",
            recab::DifferenceKind::Metadata => "same content",
        };
        match &difference.detail {
            Some(detail) => println!("  {}: {change}; {detail}", difference.path),
            None => println!("  {}: {change}", difference.path),
        }
    }
    if comparison.more > 0 {
        println!("  and {} more", comparison.more);
    }
}

fn print_health(health: &Health, timeout: Duration) {
    println!("{} startup {}", health.init, health.describe(timeout));
}
