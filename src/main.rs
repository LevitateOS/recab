use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use recab::{
    Health, InitLayout, InitRequest, Slot, SlotView, StageRequest, StageSource, Status,
    TrialOutcome, stable_exit_code,
};
use recpart::types::InstallMode;

#[derive(Parser)]
#[command(name = "recab")]
#[command(about = "Manage system updates and rollback with two root slots (A/B)")]
#[command(version)]
#[command(
    after_help = "Common tasks:\n  recab status                    Show the running slot and next boot\n  recab status --root /mnt         Inspect an installed target\n  recab prepare                   Continue the running system in the inactive slot\n  recab change install ./pkg.rhai  Apply Recipe software to that preparation\n  recab edit etc/example --file X  Apply configuration to that preparation\n  recab update check              Discover signed updates through Recipe\n  recab update download           Cache the available release without changing X\n  recab finalize                  Validate, seal and publish that candidate\n  recab stage --from-media /media  Merge release media into the preparation\n  recab set-next B                 Schedule one trial boot of slot B\n  recab rollback                  Select the known-good slot for next boot\n\nA/B keeps two system copies. prepare synchronizes differences; finalize seals\nthe candidate; set-next selects a trial boot. stage merges a release and finalizes it when conflict-free. The trial service commits a healthy boot automatically.\nA and B support the same operations. Old development installs are read-only;\nuse compatible recovery media or a fresh installation. No automatic state upgrade.\nRead 'recab status' before choosing a slot. Add --json for structured status."
)]
struct Cli {
    /// Print structured status, health, inventory, preparation and finalization;
    /// any command that fails prints one JSON error on stderr.
    #[arg(long, global = true)]
    json: bool,
    /// Emit operation start/completion/failure JSON lines on stderr; stdout remains the command result.
    #[arg(long, global = true)]
    machine_events: bool,
    /// Who asked for this, recorded with a change: a short label such as
    /// "person" or "agent:codex". Front ends set it; it grants nothing.
    #[arg(long, global = true, value_name = "LABEL")]
    actor: Option<String>,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Verify both identical personalized initial roots and their boot payloads before finishing installation.
    VerifyInstallation {
        #[arg(long)]
        root: PathBuf,
    },
    /// Discover/download signed updates through Recipe; offline selection preserves prepared work.
    Update {
        #[command(subcommand)]
        command: UpdateCommand,
    },
    /// Verify/import one complete release set without changing the candidate.
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
    /// Apply Recipe software to the prepared inactive system; activation needs reboot.
    Change {
        #[arg(value_parser=["install","update","remove","recover","list","check"])]
        action: String,
        recipe: Option<PathBuf>,
        /// Current resolved producer; update otherwise uses the selected source path.
        #[arg(long)]
        producer_recipe: Option<PathBuf>,
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
        #[arg(long)]
        prepared: bool,
        #[arg(last = true)]
        recipe_args: Vec<String>,
    },
    /// Edit configuration through a bounded temporary file, then apply to the inactive system.
    Edit {
        path: PathBuf,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
    },
    /// Validate, flush and seal the candidate, then publish its immutable boot entry.
    Finalize {
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
        #[arg(long, requires = "initrd")]
        kernel: Option<PathBuf>,
        #[arg(long, requires = "kernel")]
        initrd: Option<PathBuf>,
        #[arg(long)]
        change_flavour: bool,
    },
    /// Synchronize the inactive slot in place, or reopen its existing preparation.
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
    /// Read the running system and the other one and say how alike they are;
    /// changes neither.
    Compare {
        /// Compare two system trees mounted at these paths instead of this
        /// computer's two slots: the one taken as running, then the other.
        #[arg(long, num_args = 2, value_names = ["RUNNING", "OTHER"], conflicts_with = "machine_events")]
        roots: Option<Vec<PathBuf>>,
    },
    /// Read a complete system inventory without changing it.
    Inventory {
        #[arg(long)]
        root: PathBuf,
    },
    /// List what happened to this computer's systems, newest first, from
    /// recab's audit log and state (read-only; --json for structured output).
    History,
    /// Show detected A/B status
    Status {
        /// Read the system installed under this root (its root partition
        /// mounted here, the ESP and state partition below it) instead of the
        /// running one.
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Populate and seal both personalized A/B systems, or set up one mutable root.
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
        /// A/B: the unmounted, formatted ext4 destination for the matching copy.
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

    /// Merge a verified release into the inactive preparation and finalize when conflict-free.
    Stage {
        /// Verify one release manifest and its bound payloads before staging.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Explicitly trust unsigned local media when no distribution key is configured.
        #[arg(long)]
        trusted_local: bool,
        /// A release medium (its device, or where it is mounted): stage its
        /// rootfs, kernel and, when it carries one, the installed initramfs.
        #[arg(long, value_name = "MEDIA")]
        from_media: Option<PathBuf>,
        /// EROFS rootfs file; raw inputs require their corresponding release.json.
        #[arg(long)]
        rootfs: Option<PathBuf>,
        /// Kernel image for the slot.
        #[arg(long)]
        kernel: Option<PathBuf>,
        /// Matching installed initramfs (required for raw manifest-bound inputs).
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
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
        #[arg(long)]
        generation: Option<String>,
    },

    /// Commit currently booted slot as known-good and default.
    Commit {
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
        #[arg(long)]
        generation: Option<String>,
    },

    /// Configure next boot to previously known-good slot.
    Rollback {
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
        #[arg(long)]
        generation: Option<String>,
    },

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

    /// Capture the complete release inventory and account baseline before
    /// personalizing an extracted image (recsetup does this). Existing baselines
    /// are retained; a personalized generation cannot invent its original release.
    Baseline {
        /// The image tree.
        #[arg(long)]
        root: PathBuf,
    },

    /// Write recab's package contents into a tree: the trial service
    /// (systemd unit or OpenRC service), enabled, immutable runtime policy
    /// and executable-bound state/boot compatibility record.
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

#[derive(Subcommand)]
enum ReleaseCommand {
    /// Verify identity, signature, inventory and boot bindings, then cache the exact set.
    Import {
        manifest: PathBuf,
        #[arg(long)]
        trusted_local: bool,
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum UpdateCommand {
    /// Reproduce a selected package through Recipe for the pinned release.
    Reproduce {
        package: String,
        #[arg(long)]
        recipe: Option<PathBuf>,
        #[arg(long)]
        preparation: String,
        #[arg(long)]
        revision: u64,
    },
    /// Inspect redacted conflicts and the captured merge revision.
    Conflicts,
    /// Resume the exact persisted merge; never recalculate local intent.
    Recover {
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
    },
    /// Resolve a conflict; edited resolutions import a bounded regular file.
    Resolve {
        conflict: String,
        #[arg(long)]
        choice: String,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        preparation: String,
        #[arg(long)]
        revision: u64,
    },
    /// Adopt a known, verified original release for an older installation.
    AdoptBaseline {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        trusted_local: bool,
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
    },
    Status,
    Check,
    Download {
        release: Option<String>,
    },
    /// Merge verified cached/media inputs without discarding prepared changes.
    Apply {
        release: Option<String>,
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long)]
        trusted_local: bool,
        #[arg(long, requires = "revision")]
        preparation: Option<String>,
        #[arg(long, requires = "preparation")]
        revision: Option<u64>,
    },
    /// Detach a selected release without discarding prepared changes.
    Cancel {
        #[arg(long)]
        preparation: String,
        #[arg(long)]
        revision: u64,
    },
}

fn main() -> ExitCode {
    let mut cli = Cli::parse();
    let json = cli.json;
    recab::set_actor(cli.actor.take());
    let events = cli.machine_events;
    let operation = match &cli.cmd {
        Command::VerifyInstallation { .. } => "verify-installation",
        Command::Update { .. } => "update",
        Command::Release { .. } => "release",
        Command::Change { .. } => "change",
        Command::Edit { .. } => "edit",
        Command::Finalize { .. } => "finalize",
        Command::Prepare { .. } => "prepare",
        Command::Discard { .. } => "discard",
        Command::Compare { .. } => "compare",
        Command::History => "history",
        Command::Inventory { .. } => "inventory",
        Command::Status { .. } => "status",
        Command::Stage { .. } => "stage",
        Command::Init { .. } => "init",
        Command::SetNext { .. } => "set-next",
        Command::Commit { .. } => "commit",
        Command::Rollback { .. } => "rollback",
        Command::Trial { .. } => "trial",
        Command::Health { .. } => "health",
        Command::Baseline { .. } => "baseline",
        Command::InstallUnit { .. } => "install-unit",
    };
    let event = |kind: &str, message: String, code: Option<u8>| {
        if events {
            eprintln!(
                "{}",
                serde_json::to_string(&recab::operations::Event {
                    schema_version: 1,
                    event: kind.into(),
                    operation: operation.into(),
                    message,
                    code
                })
                .unwrap()
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
    match cli.cmd {
        Command::VerifyInstallation { root } => println!(
            "{}",
            serde_json::to_string(&recab::verify_installation(&root)?)?
        ),
        Command::Update { command } => {
            match &command {
                UpdateCommand::Reproduce {
                    package,
                    recipe,
                    preparation,
                    revision,
                } => {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &recab::merge::reproduce(
                                preparation,
                                *revision,
                                package,
                                recipe.as_deref()
                            )
                            .map_err(recab::update_error)?
                        )?
                    );
                    return Ok(());
                }
                UpdateCommand::Conflicts => {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &recab::merge::conflicts().map_err(recab::update_error)?
                        )?
                    );
                    return Ok(());
                }
                UpdateCommand::Recover {
                    preparation,
                    revision,
                } => {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &recab::merge::recover(preparation.as_deref(), *revision)
                                .map_err(recab::update_error)?
                        )?
                    );
                    return Ok(());
                }
                UpdateCommand::Resolve {
                    conflict,
                    choice,
                    file,
                    preparation,
                    revision,
                } => {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &recab::merge::resolve(
                                preparation,
                                *revision,
                                conflict,
                                choice,
                                file.as_deref()
                            )
                            .map_err(recab::update_error)?
                        )?
                    );
                    return Ok(());
                }
                UpdateCommand::AdoptBaseline {
                    manifest,
                    trusted_local,
                    preparation,
                    revision,
                } => {
                    recab::merge::adopt_baseline(
                        manifest,
                        *trusted_local,
                        preparation.as_deref(),
                        *revision,
                    )
                    .map_err(recab::update_error)?;
                    println!("{}", serde_json::to_string(&recab::update::status()?)?);
                    return Ok(());
                }
                _ => {}
            }
            let result = match command {
                UpdateCommand::Reproduce { .. }
                | UpdateCommand::Conflicts
                | UpdateCommand::Recover { .. }
                | UpdateCommand::Resolve { .. }
                | UpdateCommand::AdoptBaseline { .. } => unreachable!(),
                UpdateCommand::Status => recab::update::status(),
                UpdateCommand::Check => recab::update::check(),
                UpdateCommand::Download { release } => recab::update::download(release.as_deref()),
                UpdateCommand::Apply {
                    release,
                    manifest,
                    trusted_local,
                    preparation,
                    revision,
                } => recab::update::apply(
                    release.as_deref(),
                    manifest.as_deref(),
                    trusted_local,
                    preparation.as_deref(),
                    revision,
                ),
                UpdateCommand::Cancel {
                    preparation,
                    revision,
                } => recab::update::cancel(&preparation, revision),
            }
            .map_err(|e| recab::update_error(e))?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Command::Release {
            command:
                ReleaseCommand::Import {
                    manifest,
                    trusted_local,
                    root,
                },
        } => {
            let result = recab::release::import(&manifest, trusted_local, root.as_deref())?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Command::Change {
            action,
            recipe,
            producer_recipe,
            preparation,
            revision,
            prepared,
            recipe_args,
        } => {
            if action == "list" || action == "check" {
                let view = recab::changes()?;
                let result = if action == "check" {
                    let name = recipe.context("check requires a package name")?;
                    let selected = &view[if prepared { "prepared" } else { "active" }]["packages"];
                    selected
                        .get(name.to_str().context("invalid package name")?)
                        .context("package is not selected in that system")?
                        .clone()
                } else {
                    view
                };
                println!("{}", serde_json::to_string(&result)?);
            } else {
                let result = recab::change(&recab::ChangeRequest {
                    action,
                    recipe: recipe.unwrap_or_default(),
                    producer_recipe,
                    preparation,
                    revision,
                    recipe_args,
                })?;
                if cli.json {
                    println!("{}", serde_json::to_string(&result)?);
                } else {
                    println!(
                        "Prepared {}: system changes are pending reboot. Finalize before selecting the candidate.",
                        result.package
                    );
                }
            }
        }
        Command::Edit {
            path,
            file,
            preparation,
            revision,
        } => {
            let result = recab::edit(&path, file.as_deref(), preparation.as_deref(), revision)?;
            if cli.json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                println!("Configuration prepared; finalize and reboot to activate it.");
            }
        }
        Command::Finalize {
            preparation,
            revision,
            kernel,
            initrd,
            change_flavour,
        } => {
            let result = recab::finalize(&recab::FinalizeRequest {
                preparation,
                revision,
                kernel,
                initrd,
                change_flavour,
            })?;
            if cli.json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                println!(
                    "Finalized generation {}: {} boot payload bytes written, {} files reused. Ready for a trial boot.",
                    result.generation.id, result.boot_bytes_written, result.reused_boot_files
                );
                if !result.accounting_complete {
                    println!(
                        "Earlier interrupted writes are unknown; counters cover the completed attempt."
                    );
                }
            }
        }
        Command::Prepare {
            preparation,
            revision,
        } => {
            let preparation = recab::prepare(preparation.as_deref(), revision)?;
            if cli.json {
                println!("{}", serde_json::to_string(&preparation)?);
            } else {
                println!(
                    "Preparation {} for slot {}, revision {}: {:?}.",
                    preparation.id, preparation.slot, preparation.revision, preparation.phase
                );
                if let Some(report) = preparation.synchronization {
                    println!(
                        "Last synchronization: {} payload bytes written in {} ranges; {} metadata updates. Finalize before trial boot.",
                        report.payload_bytes_written, report.range_writes, report.metadata_updates
                    );
                    if !report.accounting_complete {
                        println!(
                            "Earlier interrupted writes are unknown; counters cover the completed attempt."
                        );
                    }
                }
            }
        }
        Command::Discard {
            preparation,
            revision,
        } => {
            recab::discard(&preparation, revision)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"operation":"discard","completed":true})
                );
            }
            if !cli.json {
                println!(
                    "Preparation discarded. The inactive slot remains unavailable until a new system is completed."
                );
            }
        }
        Command::Compare { roots } => {
            let comparison = match roots.as_deref() {
                Some([running, other]) => recab::compare_roots(running, other)?,
                _ => recab::compare(cli.machine_events)?,
            };
            if cli.json {
                println!("{}", serde_json::to_string(&comparison)?);
            } else {
                print_comparison(&comparison);
            }
        }
        Command::History => {
            let history = recab::history::history()?;
            if cli.json {
                println!("{}", serde_json::to_string(&history)?);
            } else {
                recab::history::print(&history);
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
            manifest,
            trusted_local,
            from_media,
            rootfs,
            kernel,
            initrd,
            change_flavour,
        } => {
            let source = match (manifest, from_media, rootfs, kernel, initrd) {
                (Some(path), None, None, None, None) => StageSource::Manifest {
                    path,
                    trusted_local,
                },
                (None, Some(media), None, None, None) => {
                    if trusted_local {
                        StageSource::LocalMedia(media)
                    } else {
                        StageSource::Media(media)
                    }
                }
                (None, None, Some(rootfs), Some(kernel), initrd) if !trusted_local => {
                    StageSource::Files(StageRequest {
                        rootfs,
                        kernel,
                        initrd,
                    })
                }
                _ => usage(
                    "stage takes --manifest, --from-media, or explicit legacy --rootfs/--kernel/--initrd; --trusted-local applies only to a release manifest/media",
                ),
            };
            recab::stage(&source, change_flavour)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"operation":"stage","completed":true})
                );
            }
            if !cli.json {
                println!(
                    "Update staged in the inactive slot. Run 'recab status' to review it before scheduling a trial boot."
                );
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
        Command::SetNext {
            slot,
            preparation,
            revision,
            generation,
        } => {
            recab::checked_selection(
                "set-next",
                Some(slot),
                preparation.as_deref(),
                revision,
                generation.as_deref(),
            )?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"operation":"set-next","completed":true})
                );
            }
            if !cli.json {
                println!("Slot {slot} selected for one trial boot. Reboot when ready.");
            }
        }
        Command::Commit {
            generation,
            preparation,
            revision,
        } => {
            recab::checked_selection(
                "commit",
                None,
                preparation.as_deref(),
                revision,
                generation.as_deref(),
            )?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"operation":"commit","completed":true})
                );
            }
            if !cli.json {
                println!("The running slot is now known-good and the default for future boots.");
            }
        }
        Command::Rollback {
            generation,
            preparation,
            revision,
        } => {
            recab::checked_selection(
                "rollback",
                None,
                preparation.as_deref(),
                revision,
                generation.as_deref(),
            )?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"operation":"rollback","completed":true})
                );
            }
            if !cli.json {
                println!("The known-good slot is selected for the next boot. Reboot when ready.");
            }
        }
        Command::Trial { timeout } => {
            let result = recab::trial(Duration::from_secs(timeout))?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"operation":"trial","outcome":match result {TrialOutcome::Committed(_)=>"committed",TrialOutcome::NotATrial=>"no_verdict"}})
                );
            } else if let TrialOutcome::Committed(slot) = result {
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
    if value { "yes" } else { "no" }
}

/// `Status` as text: every field (no `..`, so a new field cannot be left out).
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
        lifecycle,
        updates,
        operations,
        running_release,
        prepared_release,
    } = st;
    println!(
        "Running release: {}; prepared release: {}",
        running_release.as_deref().unwrap_or("unknown"),
        prepared_release.as_deref().unwrap_or("none")
    );
    println!("Reboot needed: {}", yes_no(lifecycle.reboot_needed));
    if let Some(u) = updates {
        println!(
            "Update: {}{}",
            u.phase,
            u.available
                .as_ref()
                .map(|r| format!(" {} ({})", r.version, r.id))
                .unwrap_or_default()
        );
        if let Some(d) = &u.download {
            println!(
                "Downloaded: {} / {} verified bytes; complete: {}",
                d.verified_bytes, d.total_bytes, d.downloaded
            );
        }
        if let Some(e) = &u.last_error {
            println!("Update error: {e}");
        }
        if let Some(m) = &u.merge {
            println!(
                "Merge: {} revision {}; {} unresolved conflicts",
                m.phase, m.revision, m.unresolved
            );
            for c in &m.conflicts {
                println!(
                    "  {}: {} ({}) {}",
                    c.id,
                    c.path,
                    c.kind,
                    c.resolution.as_deref().unwrap_or("unresolved")
                );
            }
        }
    }
    for op in operations {
        println!(
            "Operation {}: {:?}{}",
            op.id,
            op.verdict,
            op.reason
                .as_ref()
                .map(|r| format!(" — {r}"))
                .unwrap_or_default()
        );
    }
    println!(
        "State: {:?}; the other system: {:?}; a command is running: {}",
        lifecycle.state,
        lifecycle.other,
        lifecycle.working.map(yes_no).unwrap_or("not known")
    );
    if let Some(reason) = recovery_reason {
        println!("Recovery required: {reason}");
    }
    if let Some(preparation) = preparation {
        println!(
            "Preparation: {} slot {} revision {} {:?}",
            preparation.id, preparation.slot, preparation.revision, preparation.phase
        );
        for change in &preparation.changes {
            println!(
                "  change: {} {}{}",
                change.action,
                change.package,
                change
                    .actor
                    .as_ref()
                    .map(|actor| format!(" (by {actor})"))
                    .unwrap_or_default()
            );
        }
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
        seal,
        kernel,
        initrd,
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
        if let Some(seal) = seal {
            println!(
                "slot {slot}: sealed {} {}, kernel {}, initramfs {}",
                seal.architecture,
                seal.kernel_release,
                kernel.as_deref().unwrap_or("none"),
                initrd.as_deref().unwrap_or("none")
            );
        }
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
