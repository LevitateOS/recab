use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use recab::{stable_exit_code, InitRequest, Slot, StageRequest, Status, TrialOutcome};

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
    },

    /// Set up the A/B boot layout of a freshly installed system (slot A).
    Init {
        /// The installed system's root, with its ESP at boot/ and state at var/.
        #[arg(long)]
        root: PathBuf,
        /// Slot A's partition, holding the installed system.
        #[arg(long)]
        slot_a: PathBuf,
        /// Slot B's partition.
        #[arg(long)]
        slot_b: PathBuf,
        /// Kernel image for slot A.
        #[arg(long)]
        kernel: PathBuf,
        /// Initramfs for slot A, if it boots through one.
        #[arg(long)]
        initrd: Option<PathBuf>,
        /// Extra kernel options for both slot entries (for example consoles).
        #[arg(long, default_value = "")]
        options: String,
    },

    /// Write a prebuilt system into the inactive slot (formats the slot).
    Stage {
        /// EROFS rootfs image: a file, or a partition holding it.
        #[arg(long)]
        rootfs: PathBuf,
        /// Kernel image for the slot.
        #[arg(long)]
        kernel: PathBuf,
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
        Command::Status { json } => {
            let st = recab::status()?;
            print_status(&st, json)?;
        }
        Command::Stage {
            rootfs,
            kernel,
            initrd,
        } => {
            recab::stage(&StageRequest {
                rootfs,
                kernel,
                initrd,
            })?;
        }
        Command::Init {
            root,
            slot_a,
            slot_b,
            kernel,
            initrd,
            options,
        } => recab::init(&InitRequest {
            root,
            slot_a,
            slot_b,
            kernel,
            initrd,
            options,
        })?,
        Command::SetNext { slot } => recab::set_next(slot)?,
        Command::Commit => recab::commit()?,
        Command::Rollback => recab::rollback()?,
        Command::Trial { timeout } => {
            if let TrialOutcome::Committed(slot) = recab::trial(Duration::from_secs(timeout))? {
                // The journal's record of the decision.
                println!("recab: committed slot {slot} after a healthy trial boot");
            }
        }
        Command::InstallUnit { root } => recab::install_unit(&root)?,
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
