use anyhow::Result;
use clap::Args;
use colored::Colorize;
use uuid::Uuid;

use arbor_common::TrajectoryId;
use crate::client::ArborClient;

#[derive(Args)]
pub struct ReplayArgs {
    /// Trajectory ID to replay.
    pub trajectory_id: Uuid,

    /// Step number to rewind to (forks from the nearest checkpoint at or before this step).
    #[arg(long, default_value = "0")]
    pub from_step: i64,

    /// Optional name for the replayed workspace.
    #[arg(long)]
    pub name: Option<String>,
}

pub async fn run(client: &ArborClient, args: ReplayArgs) -> Result<()> {
    println!(
        "{} replaying trajectory {} from step {}",
        "▶".green().bold(),
        format!("{}", args.trajectory_id).bright_black(),
        args.from_step,
    );

    #[derive(serde::Serialize)]
    struct Req { from_step: i64, workspace_name: Option<String> }
    #[derive(serde::Deserialize)]
    struct Resp { workspace_id: arbor_common::WorkspaceId, operation_id: arbor_common::OperationId }

    let resp: Resp = client.post(
        &format!("/v1/trajectories/{}/replay", args.trajectory_id),
        &Req { from_step: args.from_step, workspace_name: args.name },
    ).await?;

    println!("{} replay workspace: {}", "✓".green(), resp.workspace_id);
    println!("  operation:  {}", resp.operation_id);
    println!(
        "  poll state: {}",
        format!("GET /v1/workspaces/{}", resp.workspace_id).bright_black()
    );
    Ok(())
}
