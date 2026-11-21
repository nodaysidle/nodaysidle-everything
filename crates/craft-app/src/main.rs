mod doctor;
mod server;

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use craft_agents::{AgentBuilder, detect_installed_agents};
use craft_compare::DefaultComparator;
use craft_core::{
    CancellationToken, CraftRequest, DeepAuthorization, EventKind, EventSink, InspectionDepth,
    Pipeline, RunConfig, TargetKind, TargetLocation,
};
use craft_inspect::CompositeInspector;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "craft-anything",
    about = "Study a target, recreate it with your coding agent, compare, and refine until close."
)]
struct Cli {
    /// Directory where run state, specs, and evidence artifacts are stored.
    #[arg(long, default_value = ".craft-runs", global = true)]
    runs_dir: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch the local Web UI (Simple Mode + Advanced Mode).
    Ui {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 7420)]
        port: u16,
    },
    /// List installed local coding agents detected in PATH.
    Agents,
    /// Check coding agents, the headless browser, and optional inspection tools.
    Doctor,
    /// Run a study -> build -> check -> ready loop from the terminal.
    Run {
        #[arg(long, value_enum)]
        target: CliTargetKind,
        #[arg(long)]
        source: String,
        #[arg(long)]
        prompt: String,
        /// Coding agent to use: claude, codex, opencode, gemini, agy, or your own command.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        output_dir: Option<PathBuf>,
        /// Enable deeper inspection (requires --confirm-authorized).
        #[arg(long)]
        deep: bool,
        /// Explicit confirmation that you are authorized to inspect this target deeply.
        #[arg(long)]
        confirm_authorized: bool,
        #[arg(long, default_value_t = 3)]
        max_iterations: u32,
        /// Write the generated agent prompt instead of invoking the coding agent.
        #[arg(long)]
        dry_run: bool,
        /// Print detailed logs (Advanced Mode output in CLI).
        #[arg(long)]
        advanced: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CliTargetKind {
    Website,
    DesktopApp,
    PhoneApp,
    Game,
    File,
}

impl From<CliTargetKind> for TargetKind {
    fn from(value: CliTargetKind) -> Self {
        match value {
            CliTargetKind::Website => TargetKind::Website,
            CliTargetKind::DesktopApp => TargetKind::DesktopApp,
            CliTargetKind::PhoneApp => TargetKind::PhoneApp,
            CliTargetKind::Game => TargetKind::Game,
            CliTargetKind::File => TargetKind::File,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("craft_app=info".parse()?))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Agents => {
            let agents = detect_installed_agents();
            if agents.is_empty() {
                println!("No local coding agents detected in PATH.");
                println!("Supported: claude, codex, opencode, gemini, agy");
            } else {
                println!("Detected coding agents:");
                for agent in agents {
                    println!(
                        "  - {} ({}) -> {}",
                        agent.kind.display_name(),
                        agent.binary,
                        agent.path.display()
                    );
                }
            }
        }
        Commands::Doctor => doctor::print(&doctor::collect().await),
        Commands::Ui { host, port } => {
            server::serve(&host, port, cli.runs_dir).await?;
        }
        Commands::Run {
            target,
            source,
            prompt,
            agent,
            output_dir,
            deep,
            confirm_authorized,
            max_iterations,
            dry_run,
            advanced,
        } => {
            let depth = if deep {
                InspectionDepth::Deep {
                    authorization: DeepAuthorization {
                        confirmed_by_user: confirm_authorized,
                        reason: "CLI --deep --confirm-authorized".into(),
                        confirmed_at: Utc::now(),
                    },
                }
            } else {
                InspectionDepth::CleanRoom
            };

            let request = CraftRequest {
                target_kind: target.into(),
                location: TargetLocation::parse(&source),
                prompt,
                depth,
                preferred_agent: agent,
                output_dir,
            };

            let events = EventSink::new(4096).persist_to(cli.runs_dir.clone());
            let mut rx = events.subscribe();
            let printer = tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    match ev.kind {
                        EventKind::StageChanged { stage, message } => {
                            println!("[{}] {}", stage.label().to_uppercase(), message);
                        }
                        EventKind::Log {
                            level,
                            source,
                            message,
                        } if advanced => {
                            println!("  [{level:?}][{source}] {message}");
                        }
                        EventKind::EvidenceAdded {
                            evidence_id,
                            label,
                        } if advanced => {
                            println!("  [EVIDENCE] {evidence_id}: {label}");
                        }
                        EventKind::ParityEvaluated {
                            iteration,
                            score,
                            gaps,
                        } => {
                            println!(
                                "  [CHECK #{iteration}] parity: {:.0}% ({} gap(s))",
                                score * 100.0,
                                gaps.len()
                            );
                            if advanced {
                                for gap in gaps {
                                    println!("    - {gap}");
                                }
                            }
                        }
                        EventKind::Finished { message, .. } => println!("{message}"),
                        _ => {}
                    }
                }
            });

            let builder = AgentBuilder {
                dry_run,
                ..Default::default()
            };
            let pipeline = Pipeline::new(
                CompositeInspector,
                builder,
                DefaultComparator,
                RunConfig {
                    max_iterations,
                    ..Default::default()
                },
            );

            // Ctrl-C cancels the run cleanly (the agent subprocess is stopped) instead of orphaning it.
            let cancel = CancellationToken::new();
            let on_interrupt = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    on_interrupt.cancel();
                }
            });

            let result = pipeline
                .execute_with_id(
                    uuid::Uuid::new_v4(),
                    request,
                    &cli.runs_dir,
                    &events,
                    cancel,
                )
                .await;
            drop(events);
            let _ = printer.await;
            let result = result?;
            println!(
                "Run ID: {}\nOutput: {}",
                result.state.id,
                result.state.workspace_dir.display()
            );
        }
    }

    Ok(())
}
