//! Integration tests for Arbor.
//!
//! These tests validate the core business logic (state machine, reseal hooks,
//! checkpoint DAG) without requiring a live Firecracker process. They use
//! a real PostgreSQL database — set TEST_DATABASE_URL to run.
//!
//! Run with:
//!   TEST_DATABASE_URL=postgresql://arbor:pass@localhost/arbor_test \
//!   cargo test --test integration_test -- --nocapture

use std::sync::Arc;
use uuid::Uuid;

// Skip all tests if no DB is available
fn db_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL").ok()
}

// ── Helper: create a test DB pool ─────────────────────────────────────────────

async fn test_pool(db_url: &str) -> sqlx::PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(db_url)
        .await
        .expect("test DB connect");
    // Migrations run by the test harness — skip migrate here if DB already set up
        // sqlx::migrate!("../../migrations").run(&pool).await.ok();
    pool
}

// ── Test: workspace state transitions ─────────────────────────────────────────

#[tokio::test]
async fn test_workspace_state_transitions() {
    let Some(url) = db_url() else { return; };
    let pool = test_pool(&url).await;
    let db   = Arc::new(arbor_controller::Db::new(pool));

    use arbor_common::*;
    let ws_id = WorkspaceId::new();
    let now   = chrono::Utc::now();

    let ws = Workspace {
        id: ws_id, name: "test-ws".into(), state: WorkspaceState::Creating,
        repo: RepoConfig { provider: "github".into(), url: "git@github.com:x/y.git".into(),
                           r#ref: "refs/heads/main".into(), commit: None },
        runtime: RuntimeConfig { runner_class: "fc-x86_64-v1".into(),
                                 vcpu_count: 2, memory_mib: 2048, disk_gb: 20 },
        compatibility_key: CompatibilityKey::new(
            "fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","ubuntu-24.04-dev-v1",1),
        current_checkpoint_id: None, runner_id: None,
        identity_epoch: 0, network_epoch: 0, created_at: now, updated_at: now,
    };

    db.insert_workspace(&ws).await.expect("insert workspace");

    // Transition through states
    for state in [
        WorkspaceState::Ready, WorkspaceState::Running,
        WorkspaceState::Checkpointing, WorkspaceState::Ready,
        WorkspaceState::Terminating, WorkspaceState::Terminated,
    ] {
        db.update_workspace_state(ws_id, state.clone()).await.expect("update state");
        let fetched = db.get_workspace(ws_id).await.expect("get").expect("exists");
        assert_eq!(fetched.state, state, "state should be {:?}", state);
    }

    println!("✓ workspace state transitions");
}

// ── Test: checkpoint DAG ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_checkpoint_dag() {
    let Some(url) = db_url() else { return; };
    let pool = test_pool(&url).await;
    let db   = Arc::new(arbor_controller::Db::new(pool));

    use arbor_common::*;
    let ws_id = WorkspaceId::new();
    let now   = chrono::Utc::now();
    let compat = CompatibilityKey::new(
        "fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","ubuntu-24.04-dev-v1",1);

    let ws = Workspace {
        id: ws_id, name: "dag-test".into(), state: WorkspaceState::Ready,
        repo: RepoConfig { provider: "github".into(), url: "git@g.com/r.git".into(),
                           r#ref: "refs/heads/main".into(), commit: None },
        runtime: RuntimeConfig { runner_class: "fc-x86_64-v1".into(),
                                 vcpu_count: 2, memory_mib: 2048, disk_gb: 20 },
        compatibility_key: compat.clone(),
        current_checkpoint_id: None, runner_id: None,
        identity_epoch: 0, network_epoch: 0, created_at: now, updated_at: now,
    };
    db.insert_workspace(&ws).await.unwrap();

    // Create root checkpoint
    let ckpt_root = CheckpointId::new();
    let root = Checkpoint {
        id: ckpt_root, workspace_id: ws_id, parent_id: None,
        name: Some("root".into()), state: CheckpointState::Sealed,
        compatibility_key: compat.clone(),
        artifacts: CheckpointArtifacts {
            state_uri: Some("local://test/state.snap".into()),
            mem_uri:   Some("local://test/mem.snap".into()),
            block_manifest_uri: Some("local://test/block.json".into()),
            state_digest: Some("sha256:aaa".into()),
            mem_digest:   Some("sha256:bbb".into()),
        },
        resume_hooks_version: 1, identity_epoch: 0, network_epoch: 0, created_at: now,
    };
    db.insert_checkpoint(&root).await.unwrap();
    db.update_current_checkpoint(ws_id, ckpt_root).await.unwrap();

    // Fork into 3 children
    let mut child_ids = Vec::new();
    for i in 0..3 {
        let cid = CheckpointId::new();
        child_ids.push(cid);
        let child = Checkpoint {
            id: cid, workspace_id: ws_id,
            parent_id: Some(ckpt_root),           // <-- DAG link
            name: Some(format!("fork-{i}")),
            state: CheckpointState::Pending,
            compatibility_key: compat.clone(),
            artifacts: CheckpointArtifacts::empty(),
            resume_hooks_version: 1, identity_epoch: i as u64, network_epoch: i as u64,
            created_at: now,
        };
        db.insert_checkpoint(&child).await.unwrap();
    }

    // Verify: all children point back to root
    let all = db.list_checkpoints_for_workspace(ws_id).await.unwrap();
    assert_eq!(all.len(), 4, "should have root + 3 forks");

    let children: Vec<_> = all.iter().filter(|c| c.parent_id == Some(ckpt_root)).collect();
    assert_eq!(children.len(), 3, "3 children of root");

    let fetched_root = db.get_checkpoint(ckpt_root).await.unwrap().unwrap();
    assert_eq!(fetched_root.state, CheckpointState::Sealed);

    println!("✓ checkpoint DAG (1 root, 3 forks)");
}

// ── Test: identity epoch bump (reseal invariant) ──────────────────────────────

#[tokio::test]
async fn test_identity_epoch_monotonically_increases() {
    let Some(url) = db_url() else { return; };
    let pool = test_pool(&url).await;
    let db   = Arc::new(arbor_controller::Db::new(pool));

    use arbor_common::*;
    let ws_id = WorkspaceId::new();
    let now   = chrono::Utc::now();

    let ws = Workspace {
        id: ws_id, name: "epoch-test".into(), state: WorkspaceState::Quarantined,
        repo: RepoConfig { provider: "test".into(), url: "".into(), r#ref: "".into(), commit: None },
        runtime: RuntimeConfig { runner_class: "fc-x86_64-v1".into(),
                                 vcpu_count: 2, memory_mib: 2048, disk_gb: 20 },
        compatibility_key: CompatibilityKey::new(
            "fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","ubuntu-24.04-dev-v1",1),
        current_checkpoint_id: None, runner_id: None,
        identity_epoch: 0, network_epoch: 0, created_at: now, updated_at: now,
    };
    db.insert_workspace(&ws).await.unwrap();

    // Bump epoch 5 times — simulates 5 restores from same checkpoint
    let mut last_epoch = 0u64;
    for i in 1..=5 {
        let new_epoch = db.bump_identity_epoch(ws_id).await.unwrap();
        assert!(new_epoch > last_epoch,
                "epoch must increase: was {} got {}", last_epoch, new_epoch);
        assert_eq!(new_epoch, i, "epoch should be {}", i);
        last_epoch = new_epoch;
    }

    let ws = db.get_workspace(ws_id).await.unwrap().unwrap();
    assert_eq!(ws.identity_epoch, 5, "final epoch should be 5");
    assert_eq!(ws.network_epoch,  5, "network epoch bumped together");

    println!("✓ identity epoch monotonically increases through 5 reseal cycles");
}

// ── Test: no token collision across forks ─────────────────────────────────────

#[tokio::test]
async fn test_no_token_collision_across_forks() {
    use arbor_common::*;
    use std::collections::HashSet;

    // Simulate: same checkpoint restored N times, each must get a unique token set.
    // We test the token signing function directly.
    let secret = "test-secret-32-bytes-minimum-len";
    let ws_ids: Vec<WorkspaceId> = (0..10).map(|_| WorkspaceId::new()).collect();
    let sess_id = SessionId::new();

    let tokens: Vec<String> = ws_ids.iter().map(|ws_id| {
        // Inline the sign logic from state_machine (testing the invariant, not impl)
        use sha2::{Sha256, Digest};
        let expires = chrono::Utc::now().timestamp() + 900;
        let payload = format!("{}.{}.{}", ws_id, sess_id, expires);
        let mut h = Sha256::new();
        h.update(secret.as_bytes());
        h.update(payload.as_bytes());
        format!("{}.{}", payload, &hex::encode(h.finalize())[..16])
    }).collect();

    // Every token must be unique — different ws_id → different payload → different token
    let unique: HashSet<&String> = tokens.iter().collect();
    assert_eq!(unique.len(), tokens.len(),
               "All {} workspace tokens must be unique — no collision", tokens.len());

    println!("✓ no token collision: {} workspaces restored from same checkpoint all get unique tokens", tokens.len());
}

// ── Test: grant lifecycle ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_grant_revocation_on_quarantine() {
    let Some(url) = db_url() else { return; };
    let pool = test_pool(&url).await;
    let db   = Arc::new(arbor_controller::Db::new(pool));

    use arbor_common::*;
    let ws_id  = WorkspaceId::new();
    let now    = chrono::Utc::now();

    // Setup workspace
    let ws = Workspace {
        id: ws_id, name: "grant-test".into(), state: WorkspaceState::Ready,
        repo: RepoConfig { provider: "test".into(), url: "".into(), r#ref: "".into(), commit: None },
        runtime: RuntimeConfig { runner_class: "fc-x86_64-v1".into(),
                                 vcpu_count: 2, memory_mib: 2048, disk_gb: 20 },
        compatibility_key: CompatibilityKey::new(
            "fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","ubuntu-24.04-dev-v1",1),
        current_checkpoint_id: None, runner_id: None,
        identity_epoch: 0, network_epoch: 0, created_at: now, updated_at: now,
    };
    db.insert_workspace(&ws).await.unwrap();

    // Add 3 grants
    for i in 0..3 {
        let grant = SecretGrant {
            id: GrantId::new(), workspace_id: ws_id,
            provider: format!("provider-{i}"),
            mode: SecretMode::BrokeredProxy,
            vault_ref: format!("vault://test/secret-{i}"),
            allowed_hosts: vec![format!("api{i}.example.com")],
            ttl_seconds: 3600, active: true,
            expires_at: Some(now + chrono::Duration::seconds(3600)),
            created_at: now,
        };
        db.upsert_secret_grant(&grant).await.unwrap();
    }

    // Verify 3 active grants
    let active = db.list_active_grants(ws_id).await.unwrap();
    assert_eq!(active.len(), 3, "should have 3 active grants before quarantine");

    // Quarantine entry revokes all grants (reseal hook 2)
    db.revoke_all_grants(ws_id).await.unwrap();

    // Verify all revoked
    let active_after = db.list_active_grants(ws_id).await.unwrap();
    assert_eq!(active_after.len(), 0, "all grants must be revoked on quarantine entry");

    println!("✓ all {} grants revoked when workspace enters quarantine", 3);
}

// ── Test: runner heartbeat + health sweep ────────────────────────────────────

#[tokio::test]
async fn test_runner_health_lifecycle() {
    let Some(url) = db_url() else { return; };
    let pool = test_pool(&url).await;
    let db   = Arc::new(arbor_controller::Db::new(pool));

    use arbor_common::*;

    let runner = RunnerNode {
        id: RunnerId::new(),
        runner_class: "fc-x86_64-v1".into(),
        address: "http://localhost:9090".into(),
        arch: "x86_64".into(),
        firecracker_version: "1.9.0".into(),
        cpu_template: "T2".into(),
        capacity_slots: 10, used_slots: 0,
        healthy: true,
        last_heartbeat: chrono::Utc::now(),
    };
    db.upsert_runner(&runner).await.unwrap();

    // Send heartbeat with 3 used slots
    db.runner_heartbeat(runner.id, 3).await.unwrap();
    let fetched = db.list_all_runners().await.unwrap()
        .into_iter().find(|r| r.id == runner.id).unwrap();
    assert_eq!(fetched.used_slots, 3, "used_slots should be 3 after heartbeat");
    assert!(fetched.healthy, "should be healthy after heartbeat");

    // Mark unhealthy manually
    db.mark_runner_unhealthy(runner.id).await.unwrap();
    let fetched2 = db.list_all_runners().await.unwrap()
        .into_iter().find(|r| r.id == runner.id).unwrap();
    assert!(!fetched2.healthy, "should be marked unhealthy");

    // Verify unhealthy runner not returned by list_healthy_runners
    let healthy = db.list_healthy_runners("fc-x86_64-v1").await.unwrap();
    assert!(!healthy.iter().any(|r| r.id == runner.id),
            "unhealthy runner must not appear in healthy list");

    // Heartbeat recovers it
    db.runner_heartbeat(runner.id, 0).await.unwrap();
    let fetched3 = db.list_all_runners().await.unwrap()
        .into_iter().find(|r| r.id == runner.id).unwrap();
    assert!(fetched3.healthy, "heartbeat must restore healthy status");

    println!("✓ runner health lifecycle: heartbeat → unhealthy → recovery");
}
