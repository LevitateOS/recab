use clap::{Parser, Subcommand};
use std::process::ExitCode;

use recab::{stable_exit_code, Slot, Status};

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

    /// Stage a one-shot trial boot for a slot.
    SetNext {
        #[arg(value_enum, ignore_case = true)]
        slot: Slot,
    },

    /// Commit currently booted slot as known-good and default.
    Commit,

    /// Configure next boot to previously known-good slot.
    Rollback,
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
        Command::SetNext { slot } => {
            recab::set_next(slot)?;
        }
        Command::Commit => {
            recab::commit()?;
        }
        Command::Rollback => {
            recab::rollback()?;
        }
    }
    Ok(())
}

fn print_status(st: &Status, json: bool) -> anyhow::Result<()> {
    if json {
        let payload = serde_json::to_string_pretty(st)?;
        println!("{payload}");
        return Ok(());
    }

    println!("active: {}", st.active_slot);
    println!("inactive: {}", st.inactive_slot);
    println!(
        "known_good: {}",
        st.known_good_slot
            .map(|slot| slot.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    println!(
        "default: {}",
        st.default_slot
            .map(|slot| slot.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    println!(
        "trial_pending: {}",
        if st.trial_pending { "yes" } else { "no" }
    );
    println!(
        "next_boot: {}",
        st.next_boot
            .map(|slot| slot.to_string())
            .unwrap_or_else(|| "none".to_string())
    );
    println!("entry_prefix: {}", st.entry_prefix);
    Ok(())
}
