use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use recab::{
    stable_exit_code, Health, InitLayout, InitRequest, Slot, SlotView, StageRequest, StageSource, Status, TrialOutcome,
};
use recpart::types::InstallMode;

#[derive(Parser)]
#[command(name = "recab")]
#[command(about = "A/B slot + boot-selection manager")]
#[command(version)]
struct Cli {
    /// Machine-readable output: `status` and `health` print JSON, and any
    /// command that fails prints its error as one JSON object on stderr.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show detected A/B status
    Status {
        /// Read the system installed under this root (its root partition
        /// mounted here, the ESP and state partition below it) instead of the
        /// running one.
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Set up the boot layout of a freshly installed system: systemd-boot,
    /// the boot entries (both A/B slots, or the one mutable root) and the
    /// installed kernel.
    Init {
        /// The installed system's root, with its ESP at boot/ and state at var/.
        #[arg(long)]
        root: PathBuf,
        /// The disk layout: two A/B slots, or one mutable root.
        #[arg(long, value_enum, default_value_t = InstallMode::Ab)]
        mode: InstallMode,
        /// A/B: slot A's partition, holding the installed system.
        #[arg(long)]
        slot_a: Option<PathBuf>,
        /// A/B: slot B's partition.
        #[arg(long)]
        slot_b: Option<PathBuf>,
        /// Mutable: the root partition.
        #[arg(long)]
        root_device: Option<PathBuf>,
        /// Kernel image of the installed system.
        #[arg(long)]
        kernel: PathBuf,
        /// Its initramfs, if it boots through one.
        #[arg(long)]
        initrd: Option<PathBuf>,
        /// Extra kernel options for the entries (for example consoles).
        #[arg(long, default_value = "")]
        options: String,
    },

    /// Write a prebuilt system into the inactive slot (formats the slot).
    Stage {
        /// A release medium (its device, or where it is mounted): stage its
        /// rootfs, kernel and, when it carries one, the installed initramfs.
        #[arg(long, value_name = "MEDIA")]
        from_media: Option<PathBuf>,
        /// EROFS rootfs image: a file, or a partition holding it.
        #[arg(long)]
        rootfs: Option<PathBuf>,
        /// Kernel image for the slot.
        #[arg(long)]
        kernel: Option<PathBuf>,
        /// Initramfs for the slot, if it boots through one.
        #[arg(long)]
        initrd: Option<PathBuf>,
        /// Stage an image of the other flavour (the base system over a
        /// desktop install, or the reverse), which stage otherwise refuses.
        #[arg(long)]
        change_flavour: bool,
    },

    /// Stage a one-shot trial boot for a slot.
    SetNext {
        #[arg(value_name = "A|B", value_parser = str::parse::<Slot>)]
        slot: Slot,
    },

    /// Commit currently booted slot as known-good and default.
    Commit,

    /// Configure next boot to previously known-good slot.
    Rollback,

    /// After startup: commit a healthy trial boot, or reboot into the
    /// known-good slot (run by the trial service on every boot).
    Trial {
        /// Seconds to wait for startup to finish before the trial fails.
        #[arg(long, default_value_t = recab::TRIAL_TIMEOUT_SECS)]
        timeout: u64,
    },

    /// Wait for startup to finish and report how it ended: the verdict a
    /// trial judges by (read-only; needs no A/B layout).
    Health {
        /// Seconds to wait for startup to finish.
        #[arg(long, default_value_t = recab::TRIAL_TIMEOUT_SECS)]
        timeout: u64,
    },

    /// Write recab's package contents into a tree: the trial service
    /// (systemd unit or OpenRC service), enabled, and the list of machine
    /// state `stage` carries.
    InstallUnit {
        /// Root of the tree (a package tree, or a system).
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// The init system to write the service for; without it, the one the
        /// root's os-release names.
        #[arg(long, value_name = "INIT", value_parser = str::parse::<recab::InitSystem>)]
        package: Option<recab::InitSystem>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
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
    match cli.cmd {
        Command::Status { root } => {
            let st = recab::status(root.as_deref())?;
            print_status(&st, cli.json)?;
        }
        Command::Stage { from_media, rootfs, kernel, initrd, change_flavour } => {
            let source = match (from_media, rootfs, kernel, initrd) {
                (Some(media), None, None, None) => StageSource::Media(media),
                (None, Some(rootfs), Some(kernel), initrd) => StageSource::Files(StageRequest { rootfs, kernel, initrd }),
                _ => usage("stage takes --from-media, or --rootfs and --kernel (and --initrd)"),
            };
            recab::stage(&source, change_flavour)?;
        }
        Command::Init { root, mode, slot_a, slot_b, root_device, kernel, initrd, options } => {
            let layout = match (mode, slot_a, slot_b, root_device) {
                (InstallMode::Ab, Some(slot_a), Some(slot_b), None) => InitLayout::Ab { slot_a, slot_b },
                (InstallMode::Mutable, None, None, Some(root_device)) => InitLayout::Mutable { root_device },
                _ => usage("--mode ab takes --slot-a and --slot-b; --mode mutable takes --root-device"),
            };
            recab::init(&InitRequest { root, layout, kernel, initrd, options })?
        }
        Command::SetNext { slot } => recab::set_next(slot)?,
        Command::Commit => recab::commit()?,
        Command::Rollback => recab::rollback()?,
        Command::Trial { timeout } => {
            if let TrialOutcome::Committed(slot) = recab::trial(Duration::from_secs(timeout))? {
                // The record of the decision (the journal, or the kernel log on OpenRC).
                println!("recab: committed slot {slot} after a healthy trial boot");
            }
        }
        Command::Health { timeout } => {
            let health = recab::health(Duration::from_secs(timeout))?;
            if cli.json {
                println!("{}", serde_json::to_string(&health)?);
            } else {
                print_health(&health);
            }
        }
        Command::InstallUnit { root, package } => recab::install_unit(&root, package)?,
    }
    Ok(())
}

/// A command-line usage error, reported the way clap reports its own (exit 2).
fn usage(message: &str) -> ! {
    Cli::command().error(clap::error::ErrorKind::ArgumentConflict, message).exit()
}

/// One sentinel for every unset slot.
fn slot_text(slot: Option<Slot>) -> String {
    slot.map_or("none".to_string(), |s| s.to_string())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// `Status` as text: every field (no `..`, so a new field cannot be left out).
fn print_status(st: &Status, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(st)?);
        return Ok(());
    }
    let Status {
        active_slot,
        inactive_slot,
        trial_pending,
        next_boot,
        default_slot,
        known_good_slot,
        committed,
        rollback_slot,
        last_trial,
        entry_prefix,
        slots,
    } = st;
    println!("active: {active_slot}");
    println!("inactive: {inactive_slot}");
    println!("known_good: {}", slot_text(*known_good_slot));
    println!("default: {}", slot_text(*default_slot));
    println!("committed: {}", yes_no(*committed));
    println!("rollback: {}", slot_text(*rollback_slot));
    println!("trial_pending: {}", yes_no(*trial_pending));
    println!("next_boot: {}", slot_text(*next_boot));
    if let Some(trial) = last_trial {
        let outcome = if trial.committed { "committed" } else { "abandoned" };
        println!("last_trial: slot {} {outcome} ({})", trial.slot, trial.detail);
    }
    println!("entry_prefix: {entry_prefix}");
    for SlotView { slot, entry, boot_dir, root, device, bootable, trial } in slots {
        println!(
            "slot {slot}: entry {entry}, kernel /{boot_dir}, root {}, device {}, bootable {}, trial options {}",
            root.as_deref().unwrap_or("none"),
            device.as_deref().unwrap_or("none"),
            yes_no(*bootable),
            yes_no(*trial),
        );
    }
    Ok(())
}

/// `Health` as text: every public field.
fn print_health(health: &Health) {
    let Health { init, finished, state, failed, .. } = health;
    let finished = if *finished { "finished" } else { "did not finish" };
    println!("{init} startup {finished} ({state})");
    for name in failed {
        println!("failed: {name}");
    }
}
