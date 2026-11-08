use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use recab::{
    stable_exit_code, InitLayout, InitRequest, Slot, StageRequest, StageSource, Status, TrialOutcome,
};
use recpart::types::InstallMode;

#[derive(Parser)]
#[command(name = "recab")]
#[command(about = "A/B slot + boot-selection manager")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show detected A/B status
    Status {
        /// Emit machine-readable status JSON.
        #[arg(long)]
        json: bool,
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
        #[arg(long, required_if_eq("mode", "ab"))]
        slot_a: Option<PathBuf>,
        /// A/B: slot B's partition.
        #[arg(long, required_if_eq("mode", "ab"))]
        slot_b: Option<PathBuf>,
        /// Mutable: the root partition.
        #[arg(long, required_if_eq("mode", "mutable"), conflicts_with_all = ["slot_a", "slot_b"])]
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
        #[arg(long, value_name = "MEDIA", conflicts_with_all = ["rootfs", "kernel", "initrd"])]
        from_media: Option<PathBuf>,
        /// EROFS rootfs image: a file, or a partition holding it.
        #[arg(long, required_unless_present = "from_media")]
        rootfs: Option<PathBuf>,
        /// Kernel image for the slot.
        #[arg(long, required_unless_present = "from_media")]
        kernel: Option<PathBuf>,
        /// Initramfs for the slot, if it boots through one.
        #[arg(long)]
        initrd: Option<PathBuf>,
    },

    /// Stage a one-shot trial boot for a slot.
    SetNext {
        #[arg(value_enum, ignore_case = true)]
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
        #[arg(long, default_value_t = 900)]
        timeout: u64,
    },

    /// Install and enable the trial service (systemd unit or OpenRC service).
    InstallUnit {
        /// Root of the system to install into (for example an install target).
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// Only write this init system's service file into the root (a package
        /// tree, which has no os-release) and enable nothing.
        #[arg(long, value_name = "INIT", value_parser = str::parse::<recab::InitSystem>)]
        package: Option<recab::InitSystem>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("recab: {err:#}");
            ExitCode::from(stable_exit_code(&err))
        }
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::Status { json, root } => {
            let st = recab::status(root.as_deref())?;
            print_status(&st, json)?;
        }
        Command::Stage { from_media, rootfs, kernel, initrd } => {
            let source = match (from_media, rootfs, kernel) {
                (Some(media), _, _) => StageSource::Media(media),
                (None, Some(rootfs), Some(kernel)) => StageSource::Files(StageRequest { rootfs, kernel, initrd }),
                _ => unreachable!("clap requires --rootfs and --kernel without --from-media"),
            };
            recab::stage(&source)?;
        }
        Command::Init { root, mode, slot_a, slot_b, root_device, kernel, initrd, options } => {
            let layout = match (mode, slot_a, slot_b, root_device) {
                (InstallMode::Ab, Some(slot_a), Some(slot_b), None) => InitLayout::Ab { slot_a, slot_b },
                (InstallMode::Mutable, None, None, Some(root_device)) => InitLayout::Mutable { root_device },
                _ => anyhow::bail!("--mode ab takes --slot-a and --slot-b; --mode mutable takes --root-device"),
            };
            recab::init(&InitRequest { root, layout, kernel, initrd, options })?
        }
        Command::SetNext { slot } => recab::set_next(slot)?,
        Command::Commit => recab::commit()?,
        Command::Rollback => recab::rollback()?,
        Command::Trial { timeout } => {
            if let TrialOutcome::Committed(slot) = recab::trial(Duration::from_secs(timeout))? {
                // The journal's record of the decision.
                println!("recab: committed slot {slot} after a healthy trial boot");
            }
        }
        Command::InstallUnit { root, package } => recab::install_unit(&root, package)?,
    }
    Ok(())
}

fn print_status(st: &Status, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(st)?);
        return Ok(());
    }
    let slot = |slot: Option<Slot>, none: &str| slot.map_or(none.to_string(), |s| s.to_string());
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    println!("active: {}", st.active_slot);
    println!("inactive: {}", st.inactive_slot);
    println!("known_good: {}", slot(st.known_good_slot, "unknown"));
    println!("default: {}", slot(st.default_slot, "unknown"));
    println!("committed: {}", yes_no(st.committed));
    println!("rollback: {}", slot(st.rollback_slot, "none"));
    println!("trial_pending: {}", yes_no(st.trial_pending));
    println!("next_boot: {}", slot(st.next_boot, "none"));
    if let Some(trial) = &st.last_trial {
        let outcome = if trial.committed { "committed" } else { "abandoned" };
        println!("last_trial: slot {} {outcome} ({})", trial.slot, trial.detail);
    }
    println!("entry_prefix: {}", st.entry_prefix);
    Ok(())
}
