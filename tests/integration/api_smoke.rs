/// API smoke tests — test request/response serialization, error codes,
/// and route handler logic without starting a live server or DB.
#[cfg(test)]
mod tests {
    use arbor_common::*;
    use serde_json::{json, Value};

    // ── Request deserialization ───────────────────────────────────────────────

    #[test]
    fn test_create_workspace_request_deserializes() {
        let raw = json!({
            "name": "my-repo",
            "repo": {
                "provider": "github",
                "url": "git@github.com:org/repo.git",
                "ref": "refs/heads/main"
            },
            "runtime": {
                "runner_class": "fc-x86_64-v1",
                "vcpu_count": 4,
                "memory_mib": 8192,
                "disk_gb": 60
            },
            "image": {
                "base_image_id": "ubuntu-24.04-dev-v1"
            },
            "network": {
                "egress_policy": "default-deny"
            }
        });

        let req: CreateWorkspaceRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "my-repo");
        assert_eq!(req.runtime.vcpu_count, 4);
        assert_eq!(req.runtime.memory_mib, 8192);
        assert_eq!(req.image.base_image_id, "ubuntu-24.04-dev-v1");
        assert_eq!(req.network.egress_policy, "default-deny");
    }

    #[test]
    fn test_create_workspace_default_egress_policy() {
        // egress_policy should default to "default-deny" when omitted
        let raw = json!({
            "name": "x",
            "repo": { "provider": "github", "url": "u", "ref": "r" },
            "runtime": { "runner_class": "fc-x86_64-v1", "vcpu_count": 2, "memory_mib": 2048, "disk_gb": 20 },
            "image": { "base_image_id": "ubuntu-24.04-dev-v1" },
            "network": {}
        });
        let req: CreateWorkspaceRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.network.egress_policy, "default-deny");
    }

    #[test]
    fn test_exec_request_defaults() {
        let raw = json!({
            "command": ["bash", "-l"]
        });
        let req: ExecRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.cwd, "/workspace");
        assert!(!req.pty);
        assert_eq!(req.cols, 220);
        assert_eq!(req.rows, 50);
        assert!(req.env.is_empty());
    }

    #[test]
    fn test_restore_request_quarantine_defaults_to_true() {
        let raw = json!({
            "post_restore": {}
        });
        let req: RestoreRequest = serde_json::from_value(raw).unwrap();
        assert!(req.post_restore.quarantine,       "quarantine must default to true");
        assert!(req.post_restore.identity_reseal,  "identity_reseal must default to true");
    }

    // ── Response serialization ────────────────────────────────────────────────

    #[test]
    fn test_api_error_serializes() {
        let err = ApiError::new("WORKSPACE_NOT_FOUND", "workspace not found", false);
        let v: Value = serde_json::to_value(&err).unwrap();
        assert_eq!(v["error"]["code"], "WORKSPACE_NOT_FOUND");
        assert_eq!(v["error"]["retryable"], false);
    }

    #[test]
    fn test_workspace_state_round_trips_json() {
        for state in [
            WorkspaceState::Creating, WorkspaceState::Ready, WorkspaceState::Running,
            WorkspaceState::Checkpointing, WorkspaceState::Restoring,
            WorkspaceState::Quarantined, WorkspaceState::Terminating,
            WorkspaceState::Terminated, WorkspaceState::Error,
        ] {
            let serialized   = serde_json::to_string(&state).unwrap();
            let deserialized: WorkspaceState = serde_json::from_str(&serialized).unwrap();
            assert_eq!(state, deserialized,
                       "state {serialized} must round-trip through JSON");
        }
    }

    #[test]
    fn test_checkpoint_state_round_trips_json() {
        for state in [
            CheckpointState::Pending, CheckpointState::Uploading,
            CheckpointState::Sealed,  CheckpointState::Failed,
        ] {
            let s = serde_json::to_string(&state).unwrap();
            let d: CheckpointState = serde_json::from_str(&s).unwrap();
            assert_eq!(state, d);
        }
    }

    #[test]
    fn test_secret_mode_round_trips_json() {
        for mode in [SecretMode::BrokeredProxy, SecretMode::EphemeralEnv] {
            let s = serde_json::to_value(&mode).unwrap();
            let d: SecretMode = serde_json::from_value(s).unwrap();
            assert_eq!(mode, d);
        }
    }

    // ── CompatibilityKey JSON structure ───────────────────────────────────────

    #[test]
    fn test_compatibility_key_json_fields() {
        let key = CompatibilityKey::new(
            "fc-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abcdef", "ubuntu-24.04-dev-v1", 1,
        );
        let v = &key.0;
        assert_eq!(v["runner_class"],          "fc-x86_64-v1");
        assert_eq!(v["arch"],                  "x86_64");
        assert_eq!(v["cpu_template"],          "T2");
        assert_eq!(v["firecracker_version"],   "1.9.0");
        assert_eq!(v["guest_kernel_hash"],     "sha256:abcdef");
        assert_eq!(v["base_image_id"],         "ubuntu-24.04-dev-v1");
        assert_eq!(v["block_layout_version"],  1);
    }

    #[test]
    fn test_compatibility_key_t2_not_t2a() {
        // Critical: x86_64 runners must use T2, not T2A (which is ARM Graviton)
        let key = CompatibilityKey::new(
            "fc-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        assert_eq!(key.0["cpu_template"], "T2",
                   "x86_64 runner class must use T2 cpu_template, not T2A");
        assert_ne!(key.0["cpu_template"], "T2A",
                   "T2A is for ARM/Graviton2 only");
    }

    // ── Error retryability ────────────────────────────────────────────────────

    #[test]
    fn test_retryable_errors() {
        use arbor_common::ArborError;
        // These should be retryable (transient)
        assert!(ArborError::RunnerCapacityExhausted { runner_class: "c".into() }.retryable());
        // These should not (permanent or client error)
        assert!(!ArborError::WorkspaceNotFound("x".into()).retryable());
        assert!(!ArborError::CheckpointNotSealed("x".into()).retryable());
        assert!(!ArborError::RunnerClassIncompatible {
            checkpoint_class: "a".into(), runner_class: "b".into()
        }.retryable());
    }
}
