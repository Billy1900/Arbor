use anyhow::Result;
use clap::Args;
use colored::Colorize;
use uuid::Uuid;

use arbor_common::{RolloutId, TrajectoryId};
use crate::client::ArborClient;

#[derive(Args)]
pub struct DiffArgs {
    /// Rollout ID that contains both trajectories.
    pub rollout_id: Uuid,
    /// First trajectory ID.
    pub traj_a:     Uuid,
    /// Second trajectory ID.
    pub traj_b:     Uuid,
}

pub async fn run(client: &ArborClient, args: DiffArgs) -> Result<()> {
    let diff = client.diff(
        RolloutId(args.rollout_id),
        TrajectoryId(args.traj_a),
        TrajectoryId(args.traj_b),
    ).await?;

    println!("\n{} cross-fork diff\n", "▶".green().bold());
    println!("  trajectory A : {}", format!("{}", diff.traj_a).bright_black());
    println!("  trajectory B : {}", format!("{}", diff.traj_b).bright_black());
    println!("  common steps : {}", diff.common_steps);

    match diff.divergence_step {
        None    => println!("  divergence   : {}", "none (identical paths)".green()),
        Some(s) => println!("  divergence   : step {}", s.to_string().yellow()),
    }

    if !diff.only_in_a.is_empty() {
        println!("\n{} only in A ({} steps):", "A".cyan().bold(), diff.only_in_a.len());
        for e in &diff.only_in_a {
            println!("  [{}] {} {}", e.step_seq, e.kind.yellow(), short_payload(&e.payload));
        }
    }

    if !diff.only_in_b.is_empty() {
        println!("\n{} only in B ({} steps):", "B".magenta().bold(), diff.only_in_b.len());
        for e in &diff.only_in_b {
            println!("  [{}] {} {}", e.step_seq, e.kind.yellow(), short_payload(&e.payload));
        }
    }

    println!();
    Ok(())
}

fn short_payload(v: &serde_json::Value) -> String {
    // Show a single key=value hint per event type
    if let Some(cmd) = v.get("command").and_then(|c| c.as_array()) {
        return format!("cmd={}", cmd.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "));
    }
    if let Some(host) = v.get("host").and_then(|h| h.as_str()) {
        return format!("host={host}");
    }
    if let Some(code) = v.get("exit_code") {
        return format!("exit_code={code}");
    }
    String::new()
}
