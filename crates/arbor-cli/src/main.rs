/// arbor — CLI for the Arbor agentic RL execution layer.
///
/// Usage:
///   arbor run-benchmark --checkpoint <uuid> --reward "cargo test" --forks 8
///   arbor replay <trajectory_id> --from-step 20
///   arbor diff <rollout_id> <traj_a> <traj_b>
///   arbor export <trajectory_id> --output ./trajectories/
mod client;
mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};

use client::ArborClient;
use commands::{benchmark::BenchmarkArgs, diff::DiffArgs, export::ExportArgs, replay::ReplayArgs};

#[derive(Parser)]
#[command(
    name    = "arbor",
    version,
    about   = "Checkpoint-native rollout infrastructure for agentic RL.",
    long_about = None,
)]
struct Cli {
    /// Arbor API base URL.
    #[arg(long, env = "ARBOR_API_URL", default_value = "http://localhost:8080", global = true)]
    api_url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fork N isolated attempts from a checkpoint, run a reward command,
    /// and display a comparison table with per-step traces.
    #[command(name = "run-benchmark")]
    RunBenchmark(BenchmarkArgs),

    /// Replay a trajectory from a given step by forking the nearest checkpoint.
    Replay(ReplayArgs),

    /// Show a step-level diff between two trajectories in the same rollout.
    Diff(DiffArgs),

    /// Export a trajectory as NDJSON for SFT / RLHF fine-tuning.
    Export(ExportArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = ArborClient::new(&cli.api_url);

    match cli.command {
        Command::RunBenchmark(args) => commands::benchmark::run(&client, args).await,
        Command::Replay(args)       => commands::replay::run(&client, args).await,
        Command::Diff(args)         => commands::diff::run(&client, args).await,
        Command::Export(args)       => commands::export::run(&client, args).await,
    }
}
