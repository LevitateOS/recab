use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "recab")]
#[command(about = "A/B slot + boot-selection manager (scaffold)")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show detected A/B status
    Status,

    /// Mark a slot as the next boot target (trial boot semantics TBD)
    SetNext {
        #[arg(value_enum)]
        slot: SlotArg,
    },

    /// Commit the currently booted slot as "good" (backend TBD)
    Commit,

    /// Roll back to the previously known-good slot (backend TBD)
    Rollback,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SlotArg {
    A,
    B,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Command::Status => {
            recab::status()?;
            println!("recab: TODO implement status backend");
        }
        Command::SetNext { slot } => {
            println!("recab: TODO set-next slot {:?}", slot);
        }
        Command::Commit => {
            println!("recab: TODO commit current slot");
        }
        Command::Rollback => {
            println!("recab: TODO rollback");
        }
    }

    Ok(())
}

