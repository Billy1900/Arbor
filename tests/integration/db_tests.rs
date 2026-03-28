/// DB layer integration tests.
/// Requires a live PostgreSQL with DATABASE_URL set.
/// Each test uses a unique workspace/checkpoint ID so tests don't interfere.

#[cfg(test)]
mod tests {
    use arbor_common::*;
    use arbor_controller::Db;
    use chrono::Utc;
    use sqlx::postgres::PgPoolOptions;

    async fn test_db() -> Db {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgresql://arbor:arbor_dev_only@localhost/arbor".into());
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .expect("connect to test DB");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrate");
        Db::new(pool)
    }

    // ── Workspace CRUD ────────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_workspace_insert_get() {
        let db  = test_db().await;
        let now = Utc::now();
        let id  = WorkspaceId::new();

        let ws = Workspace {
            id,
            name:  "test-ws".into(),
            state: WorkspaceState::Creating,
            repo:  RepoConfig {
                provider: "github".into(), url: "git@github.com:test/repo.git".into(),
                r#ref: "refs/heads/main".into(), commit: None,
            },
            runtime: RuntimeConfig {
                runner_class: "fc-x86_64-v1".into(),
                vcpu_count: 2, memory_mib: 2048, disk_gb: 20,
            },
            compatibility_key: CompatibilityKey::new(
                "fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","ubuntu-24.04-dev-v1",1,
            ),
            current_checkpoint_id: None,
            runner_id:      None,
            identity_epoch: 0,
            network_epoch:  0,
            created_at:     now,
            updated_at:     now,
        };

        db.insert_workspace(&ws).await.expect("insert");
        let got = db.get_workspace(id).await.expect("get").expect("exists");
        assert_eq!(got.id, id);
        assert_eq!(got.name, "test-ws");
        assert_eq!(got.state, WorkspaceState::Creating);
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_workspace_state_transitions() {
        let db  = test_db().await;
        let now = Utc::now();
        let id  = WorkspaceId::new();

        let ws = Workspace {
            id, name: "state-test".into(), state: WorkspaceState::Creating,
            repo: RepoConfig { provider: "".into(), url: "".into(), r#ref: "".into(), commit: None },
            runtime: RuntimeConfig { runner_class: "fc-x86_64-v1".into(), vcpu_count: 2, memory_mib: 2048, disk_gb: 20 },
            compatibility_key: CompatibilityKey::new("fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","img",1),
            current_checkpoint_id: None, runner_id: None,
            identity_epoch: 0, network_epoch: 0, created_at: now, updated_at: now,
        };
        db.insert_workspace(&ws).await.unwrap();

        for state in [WorkspaceState::Ready, WorkspaceState::Running,
                      WorkspaceState::Checkpointing, WorkspaceState::Quarantined] {
            db.update_workspace_state(id, state.clone()).await.unwrap();
            let got = db.get_workspace(id).await.unwrap().unwrap();
            assert_eq!(got.state, state);
        }
    }

    // ── Checkpoint lifecycle ──────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_checkpoint_seal() {
        let db    = test_db().await;
        let now   = Utc::now();
        let ws_id = WorkspaceId::new();
        let ck_id = CheckpointId::new();

        // Create parent workspace
        let ws = Workspace {
            id: ws_id, name: "ckpt-ws".into(), state: WorkspaceState::Ready,
            repo: RepoConfig { provider: "".into(), url: "".into(), r#ref: "".into(), commit: None },
            runtime: RuntimeConfig { runner_class: "fc-x86_64-v1".into(), vcpu_count: 2, memory_mib: 2048, disk_gb: 20 },
            compatibility_key: CompatibilityKey::new("fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","img",1),
            current_checkpoint_id: None, runner_id: None,
            identity_epoch: 3, network_epoch: 3, created_at: now, updated_at: now,
        };
        db.insert_workspace(&ws).await.unwrap();

        // Create pending checkpoint
        let ckpt = Checkpoint {
            id: ck_id, workspace_id: ws_id, parent_id: None, name: Some("before-test".into()),
            state: CheckpointState::Pending,
            compatibility_key: CompatibilityKey::new("fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","img",1),
            artifacts: CheckpointArtifacts::empty(),
            resume_hooks_version: 1, identity_epoch: 3, network_epoch: 3, created_at: now,
        };
        db.insert_checkpoint(&ckpt).await.unwrap();

        let got = db.get_checkpoint(ck_id).await.unwrap().unwrap();
        assert_eq!(got.state, CheckpointState::Pending);
        assert!(!got.artifacts.is_complete());

        // Seal with artifacts
        let artifacts = CheckpointArtifacts {
            state_uri:          Some("objstore://bucket/ckpt/state.snap".into()),
            mem_uri:            Some("objstore://bucket/ckpt/mem.snap".into()),
            block_manifest_uri: Some("objstore://bucket/ckpt/block.json".into()),
            state_digest:       Some("sha256:aabbcc".into()),
            mem_digest:         Some("sha256:ddeeff".into()),
        };
        db.seal_checkpoint(ck_id, &artifacts).await.unwrap();
        db.update_current_checkpoint(ws_id, ck_id).await.unwrap();

        let sealed = db.get_checkpoint(ck_id).await.unwrap().unwrap();
        assert_eq!(sealed.state, CheckpointState::Sealed);
        assert!(sealed.artifacts.is_complete());

        let updated_ws = db.get_workspace(ws_id).await.unwrap().unwrap();
        assert_eq!(updated_ws.current_checkpoint_id, Some(ck_id));
    }

    // ── Identity epoch ────────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_epoch_bump() {
        let db  = test_db().await;
        let now = Utc::now();
        let id  = WorkspaceId::new();

        let ws = Workspace {
            id, name: "epoch-test".into(), state: WorkspaceState::Ready,
            repo: RepoConfig { provider: "".into(), url: "".into(), r#ref: "".into(), commit: None },
            runtime: RuntimeConfig { runner_class: "fc-x86_64-v1".into(), vcpu_count: 2, memory_mib: 2048, disk_gb: 20 },
            compatibility_key: CompatibilityKey::new("fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","img",1),
            current_checkpoint_id: None, runner_id: None,
            identity_epoch: 5, network_epoch: 5, created_at: now, updated_at: now,
        };
        db.insert_workspace(&ws).await.unwrap();

        let new_epoch = db.bump_identity_epoch(id).await.unwrap();
        assert_eq!(new_epoch, 6);

        let got = db.get_workspace(id).await.unwrap().unwrap();
        assert_eq!(got.identity_epoch, 6);
        assert_eq!(got.network_epoch,  6);
    }

    // ── Secret grants ─────────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_grant_lifecycle() {
        let db    = test_db().await;
        let now   = Utc::now();
        let ws_id = WorkspaceId::new();

        // Need a workspace row first (FK constraint)
        let ws = Workspace {
            id: ws_id, name: "grant-test".into(), state: WorkspaceState::Ready,
            repo: RepoConfig { provider: "".into(), url: "".into(), r#ref: "".into(), commit: None },
            runtime: RuntimeConfig { runner_class: "fc-x86_64-v1".into(), vcpu_count: 2, memory_mib: 2048, disk_gb: 20 },
            compatibility_key: CompatibilityKey::new("fc-x86_64-v1","x86_64","T2","1.9.0","sha256:abc","img",1),
            current_checkpoint_id: None, runner_id: None,
            identity_epoch: 0, network_epoch: 0, created_at: now, updated_at: now,
        };
        db.insert_workspace(&ws).await.unwrap();

        let grant_id = GrantId::new();
        let grant = SecretGrant {
            id: grant_id, workspace_id: ws_id, provider: "openai".into(),
            mode: SecretMode::BrokeredProxy,
            vault_ref: "vault://org/prod/openai".into(),
            allowed_hosts: vec!["api.openai.com".into()],
            ttl_seconds: 3600, active: true,
            expires_at: Some(now + chrono::Duration::hours(1)),
            created_at: now,
        };
        db.upsert_secret_grant(&grant).await.unwrap();

        let grants = db.list_active_grants(ws_id).await.unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].provider, "openai");
        assert!(grants[0].active);

        // Revoke all
        db.revoke_all_grants(ws_id).await.unwrap();
        let grants_after = db.list_active_grants(ws_id).await.unwrap();
        assert!(grants_after.is_empty(), "all grants should be revoked");
    }

    // ── Runner heartbeat ──────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_runner_registration_and_heartbeat() {
        let db = test_db().await;
        let id = RunnerId::new();

        let node = RunnerNode {
            id, runner_class: "fc-x86_64-v1".into(),
            address: "http://localhost:9090".into(),
            arch: "x86_64".into(),
            firecracker_version: "1.9.0".into(),
            cpu_template: "T2".into(),
            capacity_slots: 10, used_slots: 0,
            healthy: true, last_heartbeat: chrono::Utc::now(),
        };
        db.upsert_runner(&node).await.unwrap();

        // Simulate heartbeat with 3 active VMs
        db.runner_heartbeat(id, 3).await.unwrap();

        let got = db.get_runner(id).await.unwrap().expect("runner exists");
        assert_eq!(got.used_slots, 3);
        assert!(got.healthy);
        assert_eq!(got.available_slots(), 7);

        // Mark unhealthy
        db.mark_runner_unhealthy(id).await.unwrap();
        let unhealthy = db.get_runner(id).await.unwrap().unwrap();
        assert!(!unhealthy.healthy);
    }
}
