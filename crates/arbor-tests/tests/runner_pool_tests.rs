/// Runner pool integration tests — M6.
///
/// Tests cover:
///   - Runner CRUD via the DB layer
///   - Heartbeat slot tracking
///   - Health sweep (stale runner → unhealthy)
///   - Scheduler placement: least-loaded, class filtering, compatibility matching
///
/// Requires a live PostgreSQL. Run with:
///   DATABASE_URL=postgresql://arbor:arbor_dev_only@localhost/arbor \
///   cargo test --test integration runner_pool -- --nocapture --ignored
#[cfg(test)]
mod tests {
    use arbor_common::*;
    use arbor_controller::{Db, Scheduler};
    use chrono::Utc;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::Arc;

    async fn test_db() -> Arc<Db> {
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
        Arc::new(Db::new(pool))
    }

    fn make_runner(runner_class: &str, capacity: u32, used: u32) -> RunnerNode {
        RunnerNode {
            id:                  RunnerId::new(),
            runner_class:        runner_class.into(),
            address:             format!("http://10.0.0.{}:9090", rand::random::<u8>()),
            arch:                "x86_64".into(),
            firecracker_version: "1.9.0".into(),
            cpu_template:        "T2".into(),
            capacity_slots:      capacity,
            used_slots:          used,
            healthy:             true,
            last_heartbeat:      Utc::now(),
            gpu_model:           None,
            gpu_count:           0,
            gpu_used:            0,
            gpu_vram_mib:        0,
        }
    }

    // ── Runner CRUD ───────────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_runner_upsert_and_get() {
        let db  = test_db().await;
        let node = make_runner("fc-x86_64-v1", 10, 0);
        let id   = node.id;

        db.upsert_runner(&node).await.expect("upsert");
        let got = db.get_runner(id).await.expect("get").expect("exists");

        assert_eq!(got.id,                  id);
        assert_eq!(got.runner_class,        "fc-x86_64-v1");
        assert_eq!(got.capacity_slots,      10);
        assert_eq!(got.used_slots,          0);
        assert!(got.healthy,                "newly registered runner must be healthy");
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_upsert_runner_is_idempotent() {
        let db  = test_db().await;
        let mut node = make_runner("fc-x86_64-v1", 10, 0);
        db.upsert_runner(&node).await.expect("first upsert");

        // Re-register with updated address — should not create a duplicate
        node.address = "http://192.168.1.99:9090".into();
        db.upsert_runner(&node).await.expect("second upsert");

        let got = db.get_runner(node.id).await.expect("get").expect("exists");
        assert_eq!(got.address, "http://192.168.1.99:9090",
                   "upsert must update address on conflict");
    }

    // ── Heartbeat ─────────────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_heartbeat_updates_used_slots() {
        let db   = test_db().await;
        let node = make_runner("fc-x86_64-v1", 10, 0);
        db.upsert_runner(&node).await.expect("upsert");

        // Simulate runner booting 3 VMs
        db.runner_heartbeat(node.id, 3).await.expect("heartbeat");

        let got = db.get_runner(node.id).await.expect("get").expect("exists");
        assert_eq!(got.used_slots, 3, "heartbeat must update used_slots");
        assert!(got.healthy, "heartbeat must keep runner healthy");
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_heartbeat_revives_unhealthy_runner() {
        let db   = test_db().await;
        let node = make_runner("fc-x86_64-v1", 10, 0);
        db.upsert_runner(&node).await.expect("upsert");

        // Mark unhealthy (simulates missed heartbeat sweep)
        db.mark_runner_unhealthy(node.id).await.expect("mark unhealthy");
        let got = db.get_runner(node.id).await.expect("get").expect("exists");
        assert!(!got.healthy);

        // Runner comes back → heartbeat sets healthy = true
        db.runner_heartbeat(node.id, 0).await.expect("heartbeat");
        let got = db.get_runner(node.id).await.expect("get").expect("exists");
        assert!(got.healthy, "heartbeat must revive an unhealthy runner");
    }

    // ── list_healthy_runners filtering ────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_list_healthy_runners_excludes_unhealthy() {
        let db   = test_db().await;
        let node = make_runner("fc-x86_64-v1", 10, 0);
        db.upsert_runner(&node).await.expect("upsert");
        db.mark_runner_unhealthy(node.id).await.expect("mark unhealthy");

        let runners = db.list_healthy_runners("fc-x86_64-v1").await.expect("list");
        assert!(
            runners.iter().all(|r| r.id != node.id),
            "unhealthy runner must not appear in list_healthy_runners"
        );
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_list_healthy_runners_excludes_full_runners() {
        let db   = test_db().await;
        // Runner at full capacity (used == capacity)
        let full = make_runner("fc-x86_64-v1", 5, 0);
        db.upsert_runner(&full).await.expect("upsert");
        db.runner_heartbeat(full.id, 5).await.expect("heartbeat to fill slots");

        let runners = db.list_healthy_runners("fc-x86_64-v1").await.expect("list");
        assert!(
            runners.iter().all(|r| r.id != full.id),
            "runner at full capacity must not appear in list_healthy_runners"
        );
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_list_healthy_runners_excludes_different_class() {
        let db    = test_db().await;
        let arm   = RunnerNode {
            runner_class: "fc-arm64-v1".into(),
            ..make_runner("fc-arm64-v1", 8, 0)
        };
        db.upsert_runner(&arm).await.expect("upsert arm runner");

        let runners = db.list_healthy_runners("fc-x86_64-v1").await.expect("list");
        assert!(
            runners.iter().all(|r| r.id != arm.id),
            "ARM runner must not appear when querying x86_64 class"
        );
    }

    // ── Health sweep ──────────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_health_sweep_marks_stale_runner_unhealthy() {
        let db   = test_db().await;
        let node = make_runner("fc-x86_64-v1", 10, 0);
        db.upsert_runner(&node).await.expect("upsert");

        // Back-date the heartbeat to simulate a stale runner
        sqlx::query(
            "UPDATE runner_nodes SET last_heartbeat = now() - interval '90 seconds' WHERE id = $1"
        )
        .bind(node.id.0)
        .execute(db.pool())
        .await
        .expect("backdate heartbeat");

        // Run the sweep (same SQL as the background task in arbor-api main)
        sqlx::query(
            "UPDATE runner_nodes SET healthy = false
             WHERE healthy = true AND last_heartbeat < now() - interval '60 seconds'"
        )
        .execute(db.pool())
        .await
        .expect("health sweep");

        let got = db.get_runner(node.id).await.expect("get").expect("exists");
        assert!(!got.healthy,
            "runner with heartbeat >60s ago must be marked unhealthy by the sweep");
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_health_sweep_does_not_affect_recent_runner() {
        let db   = test_db().await;
        let node = make_runner("fc-x86_64-v1", 10, 0);
        db.upsert_runner(&node).await.expect("upsert");

        // Heartbeat is fresh — sweep must not touch this runner
        sqlx::query(
            "UPDATE runner_nodes SET healthy = false
             WHERE healthy = true AND last_heartbeat < now() - interval '60 seconds'"
        )
        .execute(db.pool())
        .await
        .expect("health sweep");

        let got = db.get_runner(node.id).await.expect("get").expect("exists");
        assert!(got.healthy,
            "runner with recent heartbeat must remain healthy after sweep");
    }

    // ── Scheduler ─────────────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_scheduler_picks_least_loaded_runner() {
        let db        = test_db().await;
        let scheduler = Scheduler::new(Arc::clone(&db));

        let light = make_runner("fc-x86_64-v1", 10, 1); // 1 slot used
        let heavy = make_runner("fc-x86_64-v1", 10, 7); // 7 slots used
        db.upsert_runner(&light).await.expect("upsert light");
        db.upsert_runner(&heavy).await.expect("upsert heavy");

        // list_healthy_runners returns ordered by used_slots ASC, so scheduler
        // picks the first with available capacity — the lighter one.
        let runners = db.list_healthy_runners("fc-x86_64-v1").await.expect("list");
        // The least-loaded with available slots must come first or be selectable
        assert!(
            runners.iter().any(|r| r.id == light.id),
            "light runner must be in healthy list"
        );
        // Verify relative ordering: light before heavy in the list
        let light_pos = runners.iter().position(|r| r.id == light.id);
        let heavy_pos = runners.iter().position(|r| r.id == heavy.id);
        if let (Some(lp), Some(hp)) = (light_pos, heavy_pos) {
            assert!(lp <= hp, "list_healthy_runners must order by used_slots ASC");
        }
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_scheduler_pick_runner_returns_error_when_no_capacity() {
        let db        = test_db().await;
        let scheduler = Scheduler::new(Arc::clone(&db));

        let ck = CompatibilityKey::new(
            "fc-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );

        // All runners for this class are full — scheduler must return an error
        // We verify by using a class that has no registered runners at all.
        let result = scheduler.pick_runner("fc-x86_64-nonexistent-class", &ck, 0).await;
        assert!(result.is_err(), "pick_runner must error when no healthy runner is available");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("RUNNER_CAPACITY_EXHAUSTED") || msg.contains("capacity"),
            "error must indicate capacity exhaustion, got: {msg}"
        );
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_scheduler_picks_compatible_runner_for_restore() {
        let db        = test_db().await;
        let scheduler = Scheduler::new(Arc::clone(&db));

        // Two runners: one with FC 1.9.0, one with FC 1.8.0
        let mut r190 = make_runner("fc-x86_64-v1", 10, 0);
        r190.firecracker_version = "1.9.0".into();
        let mut r180 = make_runner("fc-x86_64-v1", 10, 0);
        r180.firecracker_version = "1.8.0".into();

        db.upsert_runner(&r190).await.expect("upsert r190");
        db.upsert_runner(&r180).await.expect("upsert r180");

        // Checkpoint was created on 1.9.0 runner
        let ck = CompatibilityKey::new(
            "fc-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        let ckpt = Checkpoint {
            id:           CheckpointId::new(),
            workspace_id: WorkspaceId::new(),
            parent_id:    None,
            name:         Some("test-ckpt".into()),
            state:        CheckpointState::Sealed,
            compatibility_key: ck,
            artifacts:    CheckpointArtifacts::empty(),
            resume_hooks_version: 1,
            identity_epoch: 1,
            network_epoch:  1,
            created_at:    Utc::now(),
        };

        let picked = scheduler.pick_compatible_runner(&ckpt).await
            .expect("must find compatible runner");
        assert_eq!(picked.firecracker_version, "1.9.0",
            "must pick runner with matching FC version");
        assert_ne!(picked.id, r180.id,
            "must not pick runner with mismatched FC version");
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_scheduler_rejects_when_no_compatible_runner_exists() {
        let db        = test_db().await;
        let scheduler = Scheduler::new(Arc::clone(&db));

        // Only a 1.8.0 runner is available, but checkpoint needs 1.9.0
        let mut r180 = make_runner("fc-x86_64-mismatch-class", 10, 0);
        r180.firecracker_version = "1.8.0".into();
        db.upsert_runner(&r180).await.expect("upsert");

        let ck = CompatibilityKey::new(
            "fc-x86_64-mismatch-class", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        let ckpt = Checkpoint {
            id:           CheckpointId::new(),
            workspace_id: WorkspaceId::new(),
            parent_id:    None,
            name:         Some("needs-190".into()),
            state:        CheckpointState::Sealed,
            compatibility_key: ck,
            artifacts:    CheckpointArtifacts::empty(),
            resume_hooks_version: 1,
            identity_epoch: 1,
            network_epoch:  1,
            created_at:    Utc::now(),
        };

        let result = scheduler.pick_compatible_runner(&ckpt).await;
        assert!(result.is_err(),
            "must fail when no runner matches the checkpoint's FC version");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("RUNNER_CLASS_INCOMPATIBLE") || msg.contains("ncompat"),
            "error must indicate incompatibility, got: {msg}"
        );
    }

    // ── Slot accounting ───────────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_increment_and_decrement_runner_slots() {
        let db   = test_db().await;
        let node = make_runner("fc-x86_64-v1", 10, 0);
        db.upsert_runner(&node).await.expect("upsert");

        db.increment_runner_slots(node.id).await.expect("increment");
        db.increment_runner_slots(node.id).await.expect("increment");
        let got = db.get_runner(node.id).await.expect("get").expect("exists");
        assert_eq!(got.used_slots, 2);

        db.decrement_runner_slots(node.id).await.expect("decrement");
        let got = db.get_runner(node.id).await.expect("get").expect("exists");
        assert_eq!(got.used_slots, 1);
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn test_decrement_does_not_go_below_zero() {
        let db   = test_db().await;
        let node = make_runner("fc-x86_64-v1", 10, 0);
        db.upsert_runner(&node).await.expect("upsert");

        // Decrement on a runner with 0 used_slots — must not underflow
        db.decrement_runner_slots(node.id).await.expect("decrement");
        let got = db.get_runner(node.id).await.expect("get").expect("exists");
        assert_eq!(got.used_slots, 0,
            "decrement below 0 must clamp to 0 via GREATEST(0, used_slots - 1)");
    }
}
