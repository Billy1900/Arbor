use anyhow::Result;
use clap::Args;
use colored::Colorize;
use std::path::PathBuf;
use uuid::Uuid;

use arbor_common::TrajectoryId;
use crate::client::ArborClient;

#[derive(Args)]
pub struct ExportArgs {
    /// Trajectory ID to export.
    pub trajectory_id: Uuid,
    /// Output file path (defaults to <trajectory_id>.jsonl in current directory).
    #[arg(long)]
    pub output: Option<PathBuf>,
}

pub async fn run(client: &ArborClient, args: ExportArgs) -> Result<()> {
    let tid  = TrajectoryId(args.trajectory_id);
    let path = args.output.unwrap_or_else(|| PathBuf::from(format!("{}.jsonl", tid)));

    println!("{} exporting trajectory {} → {}", "▶".green().bold(), format!("{tid}").bright_black(), path.display());
    client.export_jsonl(tid, &path).await?;
    let size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
    println!("{} done ({} bytes)", "✓".green(), size);
    Ok(())
}
