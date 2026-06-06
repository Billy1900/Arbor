/// `arbor run-benchmark` — fork N isolated attempts from a checkpoint,
/// run a reward command in each, collect results, print a comparison table,
/// and optionally export winning trajectories as NDJSON for SFT/RLHF.
use anyhow::Result;
use clap::Args;
use colored::Colorize;
use comfy_table::{presets::UTF8_FULL, Attribute, Cell, Color, Table};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use uuid::Uuid;

use arbor_common::{CheckpointId, WorkspaceId};
use crate::client::ArborClient;

#[derive(Args)]
pub struct BenchmarkArgs {
    /// Checkpoint UUID to fork from (source execution state).
    #[arg(long)]
    pub checkpoint: Uuid,

    /// Shell command used as the reward signal (exit 0 = pass).
    #[arg(long)]
    pub reward: String,

    /// Number of parallel isolated attempts to fork.
    #[arg(long, default_value = "4")]
    pub forks: u32,

    /// Seconds before each attempt is considered timed out.
    #[arg(long, default_value = "600")]
    pub timeout: u64,

    /// Optional prefix for workspace names (e.g. "swebench-lite").
    #[arg(long)]
    pub name: Option<String>,

    /// If set, export winning trajectories as NDJSON to this directory.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

pub async fn run(client: &ArborClient, args: BenchmarkArgs) -> Result<()> {
    let ckpt_id = CheckpointId(args.checkpoint);

    println!(
        "\n{} {} forks from checkpoint {}  reward: {}\n",
        "▶".green().bold(),
        args.forks,
        format!("{}", ckpt_id).bright_black(),
        args.reward.yellow(),
    );

    let mp = MultiProgress::new();

    // ── Step 1: create rollout + fork N workspaces ────────────────────────────
    let spin_fork = mp.add(ProgressBar::new_spinner());
    spin_fork.set_style(spinner_style());
    spin_fork.set_message("creating rollout and forking workspaces…");
    spin_fork.enable_steady_tick(Duration::from_millis(80));

    let rollout = client
        .create_rollout(ckpt_id, &args.reward, args.forks, args.name.clone())
        .await?;

    spin_fork.finish_with_message(format!(
        "rollout {} — {} workspaces forked",
        format!("{}", rollout.rollout_id).bright_black(),
        rollout.workspace_ids.len(),
    ));

    // ── Step 2: wait for all workspaces to become READY ───────────────────────
    let spin_ready = mp.add(ProgressBar::new(rollout.workspace_ids.len() as u64));
    spin_ready.set_style(bar_style());
    spin_ready.set_message("waiting for workspaces to finish resealing…");

    let wait_futs: Vec<_> = rollout.workspace_ids.iter().map(|&ws_id| {
        let client_ref = client as *const ArborClient;
        let pb = spin_ready.clone();
        let timeout = args.timeout;
        async move {
            let result = unsafe { &*client_ref }.wait_ready(ws_id, timeout).await;
            pb.inc(1);
            (ws_id, result)
        }
    }).collect();

    let ready_results = futures::future::join_all(wait_futs).await;
    spin_ready.finish_with_message("all workspaces READY");

    // ── Step 3: exec reward command in each workspace ─────────────────────────
    let spin_exec = mp.add(ProgressBar::new(rollout.workspace_ids.len() as u64));
    spin_exec.set_style(bar_style());
    spin_exec.set_message("running reward command…");

    let exec_futs: Vec<_> = ready_results.iter().map(|(ws_id, ready)| {
        let ws_id = *ws_id;
        let client_ref = client as *const ArborClient;
        let pb = spin_exec.clone();
        let reward = args.reward.clone();
        let timeout = args.timeout;
        let ready = ready.is_ok();
        async move {
            if !ready {
                pb.inc(1);
                return AttemptResult { ws_id, exit_code: None, error: Some("workspace not ready".into()), wall_ms: 0 };
            }
            let t0 = Instant::now();
            let result = async {
                let sess_id = unsafe { &*client_ref }.exec(ws_id, &reward).await?;
                let code = unsafe { &*client_ref }.wait_session(ws_id, sess_id, timeout).await?;
                Ok::<i32, anyhow::Error>(code)
            }.await;
            let wall_ms = t0.elapsed().as_millis() as u64;
            pb.inc(1);
            match result {
                Ok(code) => AttemptResult { ws_id, exit_code: Some(code), error: None, wall_ms },
                Err(e)   => AttemptResult { ws_id, exit_code: None, error: Some(e.to_string()), wall_ms },
            }
        }
    }).collect();

    let attempts: Vec<AttemptResult> = futures::future::join_all(exec_futs).await;
    spin_exec.finish_with_message("all attempts complete");

    // ── Step 4: fetch trajectory summaries for cost/step counts ──────────────
    let mut traj_info: Vec<(WorkspaceId, Option<arbor_common::TrajectoryRow>)> = Vec::new();
    for r in &attempts {
        let traj = client.get_trajectory_for_workspace(r.ws_id).await.ok().flatten();
        traj_info.push((r.ws_id, traj));
    }

    // ── Step 5: print comparison table ───────────────────────────────────────
    println!();
    print_table(&attempts, &traj_info);
    println!();

    // ── Step 6: export winning trajectories ──────────────────────────────────
    if let Some(out_dir) = &args.output {
        tokio::fs::create_dir_all(out_dir).await?;
        let wins: Vec<_> = attempts.iter().zip(traj_info.iter())
            .filter(|(a, _)| a.exit_code == Some(0))
            .collect();
        if wins.is_empty() {
            println!("{} no passing attempts to export", "⚠".yellow());
        } else {
            for (attempt, (_, traj)) in &wins {
                if let Some(traj) = traj {
                    let path = out_dir.join(format!("{}.jsonl", traj.id));
                    client.export_jsonl(traj.id, &path).await?;
                    println!("{} exported trajectory → {}", "✓".green(), path.display());
                }
            }
        }
    }

    Ok(())
}

// ── Internal types ────────────────────────────────────────────────────────────

struct AttemptResult {
    ws_id:     WorkspaceId,
    exit_code: Option<i32>,
    error:     Option<String>,
    wall_ms:   u64,
}

fn print_table(attempts: &[AttemptResult], traj_info: &[(WorkspaceId, Option<arbor_common::TrajectoryRow>)]) {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(vec![
        Cell::new("attempt").add_attribute(Attribute::Bold),
        Cell::new("workspace").add_attribute(Attribute::Bold),
        Cell::new("steps").add_attribute(Attribute::Bold),
        Cell::new("cost (USD)").add_attribute(Attribute::Bold),
        Cell::new("wall time").add_attribute(Attribute::Bold),
        Cell::new("result").add_attribute(Attribute::Bold),
    ]);

    for (i, (attempt, (_, traj))) in attempts.iter().zip(traj_info.iter()).enumerate() {
        let ws_short = format!("{}", attempt.ws_id)[..8].to_string();
        let steps    = traj.as_ref().map(|t| t.step_count.to_string()).unwrap_or_else(|| "—".into());
        let cost     = traj.as_ref().map(|t| format!("${:.3}", t.total_cost_usd)).unwrap_or_else(|| "—".into());
        let wall     = format_duration(attempt.wall_ms);

        let (result_cell, hint) = match attempt.exit_code {
            Some(0) => (Cell::new("✅ PASS").fg(Color::Green), String::new()),
            Some(c) => (
                Cell::new(format!("❌ FAIL ({})", c)).fg(Color::Red),
                traj.as_ref().map(|t| format!("  ← arbor replay {}", t.id)).unwrap_or_default(),
            ),
            None    => (Cell::new("⚠  ERROR").fg(Color::Yellow), String::new()),
        };

        table.add_row(vec![
            Cell::new(i.to_string()),
            Cell::new(ws_short),
            Cell::new(steps),
            Cell::new(cost),
            Cell::new(wall),
            result_cell,
        ]);

        if !hint.is_empty() {
            println!("{}", hint.bright_black());
        }
    }

    println!("{table}");
}

fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        let secs = ms / 1_000;
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("{spinner:.green} {msg}")
        .unwrap()
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template("{spinner:.green} [{bar:30.cyan/blue}] {pos}/{len} {msg}")
        .unwrap()
        .progress_chars("=>-")
}
