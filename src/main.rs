use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use recab::{
    stable_exit_code, Health, InitLayout, InitRequest, Slot, SlotView, StageRequest, StageSource,
    Status, TrialOutcome,
};
use recpart::types::InstallMode;

#[derive(Parser)]
#[command(name = "recab")]
#[command(about = "Manage system updates and rollback with two root slots (A/B)")]
#[command(version)]
#[command(
    after_help = "Common tasks:\n  recab status                    Show the running slot and next boot\n  recab status --root /mnt         Inspect an installed target\n  recab stage --from-media /media  Write an update to the inactive slot\n  recab set-next B                 Schedule one trial boot of slot B\n  recab rollback                  Select the known-good slot for next boot\n\nA/B keeps two system copies. stage formats the inactive slot; set-next selects\na trial boot. The trial service commits a healthy boot automatically.\nRead 'recab status' before choosing a slot. Add --json for structured status."
)]
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
    /// Record/reopen a continuation and invalidate its old boot entry (synchronization follows in P03).
    Prepare {
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
    },
    /// Explicitly abandon a matching preparation; does not restore an overwritten rollback.
    Discard {
        #[arg(long)]
        preparation: String,
        #[arg(long)]
        revision: u64,
    },
    /// Read a complete system inventory without changing it.
    Inventory {
        #[arg(long)]
        root: PathBuf,
    },
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

    /// Schedule one trial boot of the inactive slot (A or B).
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

    /// Record what the image extracted under ROOT ships at the paths its
    /// carried list names (usr/lib/recab/baseline), so the first update of
    /// the slot carries only the machine's changes. Run right after the
    /// image is extracted, before anything personalizes it (recsetup does).
    Baseline {
        /// The image tree.
        #[arg(long)]
        root: PathBuf,
    },

    /// Write recab's package contents into a tree: the trial service
    /// (systemd unit or OpenRC service), enabled, and the list of machine
    /// state `stage` carries.
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
        Command::Prepare {
            preparation,
            revision,
        } => {
            let preparation = recab::prepare(preparation.as_deref(), revision)?;
            if cli.json {
                println!("{}", serde_json::to_string(&preparation)?);
            } else {
                println!("Preparation {} for slot {}, revision {}: {:?}. Incremental synchronization is not implemented yet.", preparation.id, preparation.slot, preparation.revision, preparation.phase);
            }
        }
        Command::Discard {
            preparation,
            revision,
        } => {
            recab::discard(&preparation, revision)?;
            if !cli.json {
                println!("Preparation discarded. The inactive slot remains unavailable until a new system is completed.");
            }
        }
        Command::Inventory { root } => {
            println!("{}", serde_json::to_string(&recab::inventory(&root)?)?);
        }
        Command::Status { root } => {
            let st = recab::status(root.as_deref())?;
            print_status(&st, cli.json)?;
        }
        Command::Stage {
            from_media,
            rootfs,
            kernel,
            initrd,
            change_flavour,
        } => {
            let source = match (from_media, rootfs, kernel, initrd) {
                (Some(media), None, None, None) => StageSource::Media(media),
                (None, Some(rootfs), Some(kernel), initrd) => StageSource::Files(StageRequest {
                    rootfs,
                    kernel,
                    initrd,
                }),
                _ => usage("stage takes --from-media, or --rootfs and --kernel (and --initrd)"),
            };
            recab::stage(&source, change_flavour)?;
            if !cli.json {
                println!("Update staged in the inactive slot. Run 'recab status' to review it before scheduling a trial boot.");
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
        Command::SetNext { slot } => {
            recab::set_next(slot)?;
            if !cli.json {
                println!("Slot {slot} selected for one trial boot. Reboot when ready.");
            }
        }
        Command::Commit => {
            recab::commit()?;
            if !cli.json {
                println!("The running slot is now known-good and the default for future boots.");
            }
        }
        Command::Rollback => {
            recab::rollback()?;
            if !cli.json {
                println!("The known-good slot is selected for the next boot. Reboot when ready.");
            }
        }
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
                print_health(&health, Duration::from_secs(timeout));
            }
        }
        Command::Baseline { root } => recab::baseline(&root)?,
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

/// One sentinel for every unset slot.
fn slot_text(slot: Option<Slot>) -> String {
    slot.map_or("none".to_string(), |s| s.to_string())
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// `Status` as text: every field (no `..`, so a new field cannot be left out).
fn print_status(st: &Status, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(st)?);
        return Ok(());
    }
    let Status {
        schema_version: _,
        scheduled_trial,
        actions,
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
        preparation,
        recovery_reason,
    } = st;
    if let Some(reason) = recovery_reason {
        println!("Recovery required: {reason}");
    }
    if let Some(preparation) = preparation {
        println!(
            "Preparation: {} slot {} revision {} {:?}",
            preparation.id, preparation.slot, preparation.revision, preparation.phase
        );
    }
    println!("Running slot: {active_slot}");
    println!("Inactive slot (update target): {inactive_slot}");
    println!("Known-good slot: {}", slot_text(*known_good_slot));
    println!("Default boot slot: {}", slot_text(*default_slot));
    println!("Running slot committed: {}", yes_no(*committed));
    println!("Rollback slot: {}", slot_text(*rollback_slot));
    println!("Trial pending: {}", yes_no(*trial_pending));
    println!("Next boot slot: {}", slot_text(*next_boot));
    println!("Scheduled trial slot: {}", slot_text(*scheduled_trial));
    if let Some(trial) = last_trial {
        let outcome = if trial.committed {
            "committed"
        } else {
            "abandoned"
        };
        println!(
            "Last trial: slot {} {outcome} ({})",
            trial.slot, trial.detail
        );
    }
    println!("Boot entry prefix: {entry_prefix}");
    for SlotView {
        slot,
        entry,
        boot_dir,
        root,
        device,
        bootable,
        trial,
        generation,
        readiness,
        generation_ready,
    } in slots
    {
        println!(
            "slot {slot}: generation {}, state {:?}, generation ready {}",
            generation
                .as_ref()
                .map(|generation| generation.id.as_str())
                .unwrap_or("none"),
            readiness,
            yes_no(*generation_ready)
        );
        println!(
            "slot {slot}: entry {entry}, kernel /{boot_dir}, root {}, device {}, bootable {}, trial options {}",
            root.as_deref().unwrap_or("none"),
            device.as_deref().unwrap_or("none"),
            yes_no(*bootable),
            yes_no(*trial),
        );
    }
    for recab::ActionView {
        action,
        slot,
        verdict,
        reason,
        ..
    } in actions
    {
        let reason = reason
            .as_deref()
            .map(|r| format!(": {r}"))
            .unwrap_or_default();
        println!("{action:?} {slot}: {verdict:?}{reason}");
    }
    Ok(())
}

/// `Health` as text: every public field.
fn print_health(health: &Health, timeout: Duration) {
    println!("{} startup {}", health.init, health.describe(timeout));
}
