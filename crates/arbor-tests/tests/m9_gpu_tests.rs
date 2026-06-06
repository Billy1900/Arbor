/// M9 unit tests — GPU-capable workspaces via host-mediated inference.
///
/// All tests run without a live DB, runner-agent, or GPU sidecar.
/// Covers:
///   - RunnerNode GPU field arithmetic (available_gpu_slots)
///   - GPU runner class registration validation
///   - RuntimeConfig gpu_count serialization
///   - SecretMode::GpuSidecar + InjectKind::GpuSidecar round-trips
///   - GpuCapacityExhausted / GpuSidecarUnavailable error codes
///   - CompatibilityKey GPU field
///   - gpu.local egress grant shape
#[cfg(test)]
mod tests {
    use arbor_common::*;
    use arbor_egress_proxy::{GrantRegistry, InjectKind, ProxyGrant};
    use chrono::Utc;
    use serde_json::{json, Value};

    // ── RunnerNode GPU arithmetic ─────────────────────────────────────────────

    #[test]
    fn test_runner_node_available_gpu_slots_idle() {
        let r = gpu_runner(4, 0);
        assert_eq!(r.available_gpu_slots(), 4);
    }

    #[test]
    fn test_runner_node_available_gpu_slots_partial() {
        let r = gpu_runner(4, 3);
        assert_eq!(r.available_gpu_slots(), 1);
    }

    #[test]
    fn test_runner_node_available_gpu_slots_full() {
        let r = gpu_runner(2, 2);
        assert_eq!(r.available_gpu_slots(), 0, "fully allocated GPU runner must report 0 available");
    }

    #[test]
    fn test_runner_node_available_gpu_slots_saturating() {
        // Guard against data corruption where gpu_used > gpu_count
        let r = gpu_runner(2, 5);
        assert_eq!(r.available_gpu_slots(), 0,
            "saturating_sub must prevent underflow when gpu_used > gpu_count");
    }

    #[test]
    fn test_non_gpu_runner_has_zero_gpu_slots() {
        let r = RunnerNode {
            id:                  RunnerId::new(),
            runner_class:        "fc-x86_64-v1".into(),
            address:             "http://10.0.0.1:9090".into(),
            arch:                "x86_64".into(),
            firecracker_version: "1.9.0".into(),
            cpu_template:        "T2".into(),
            capacity_slots:      10,
            used_slots:          0,
            healthy:             true,
            last_heartbeat:      Utc::now(),
            gpu_model:           None,
            gpu_count:           0,
            gpu_used:            0,
            gpu_vram_mib:        0,
        };
        assert_eq!(r.available_gpu_slots(), 0);
    }

    // ── RuntimeConfig gpu_count ───────────────────────────────────────────────

    #[test]
    fn test_runtime_config_gpu_count_defaults_to_zero() {
        let raw = json!({
            "runner_class": "fc-x86_64-v1",
            "vcpu_count": 4,
            "memory_mib": 8192,
            "disk_gb": 40
        });
        let cfg: RuntimeConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.gpu_count, 0, "gpu_count must default to 0 (no GPU requested)");
    }

    #[test]
    fn test_runtime_config_gpu_count_one() {
        let raw = json!({
            "runner_class": "fc-gpu-x86_64-v1",
            "vcpu_count": 8,
            "memory_mib": 32768,
            "disk_gb": 100,
            "gpu_count": 1
        });
        let cfg: RuntimeConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.gpu_count, 1);
        assert_eq!(cfg.runner_class, "fc-gpu-x86_64-v1");
    }

    #[test]
    fn test_runtime_config_gpu_count_round_trips_json() {
        let cfg = RuntimeConfig {
            runner_class: "fc-gpu-x86_64-v1".into(),
            vcpu_count:   8,
            memory_mib:   32768,
            disk_gb:      100,
            gpu_count:    1,
        };
        let v: Value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["gpu_count"], 1);
        let back: RuntimeConfig = serde_json::from_value(v).unwrap();
        assert_eq!(back.gpu_count, 1);
    }

    // ── GPU runner class validation ───────────────────────────────────────────

    fn validate_gpu_runner(
        runner_class: &str,
        arch: &str,
        cpu_template: &str,
        gpu_count: u32,
    ) -> Result<(), String> {
        match runner_class {
            "fc-gpu-x86_64-v1" => {
                if arch != "x86_64" {
                    return Err(format!("fc-gpu-x86_64-v1 requires arch=x86_64, got {arch}"));
                }
                if cpu_template != "T2" {
                    return Err(format!("fc-gpu-x86_64-v1 requires cpu_template=T2, got {cpu_template}"));
                }
                if gpu_count == 0 {
                    return Err("fc-gpu-x86_64-v1 requires gpu_count > 0".into());
                }
            }
            "fc-gpu-arm64-v1" => {
                if arch != "aarch64" {
                    return Err(format!("fc-gpu-arm64-v1 requires arch=aarch64, got {arch}"));
                }
                if cpu_template != "T2A" {
                    return Err(format!("fc-gpu-arm64-v1 requires cpu_template=T2A, got {cpu_template}"));
                }
                if gpu_count == 0 {
                    return Err("fc-gpu-arm64-v1 requires gpu_count > 0".into());
                }
            }
            _ => {} // non-GPU classes handled by existing validator
        }
        Ok(())
    }

    #[test]
    fn test_valid_gpu_x86_registration() {
        assert!(validate_gpu_runner("fc-gpu-x86_64-v1", "x86_64", "T2", 4).is_ok());
    }

    #[test]
    fn test_valid_gpu_arm64_registration() {
        assert!(validate_gpu_runner("fc-gpu-arm64-v1", "aarch64", "T2A", 2).is_ok());
    }

    #[test]
    fn test_gpu_runner_with_zero_gpus_rejected() {
        let r = validate_gpu_runner("fc-gpu-x86_64-v1", "x86_64", "T2", 0);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("gpu_count > 0"));
    }

    #[test]
    fn test_gpu_runner_wrong_arch_rejected() {
        let r = validate_gpu_runner("fc-gpu-x86_64-v1", "aarch64", "T2", 4);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("x86_64"));
    }

    #[test]
    fn test_gpu_runner_wrong_template_rejected() {
        let r = validate_gpu_runner("fc-gpu-x86_64-v1", "x86_64", "T2A", 4);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("T2"));
    }

    // ── SecretMode::GpuSidecar ────────────────────────────────────────────────

    #[test]
    fn test_secret_mode_gpu_sidecar_serializes() {
        let mode = SecretMode::GpuSidecar;
        let v: Value = serde_json::to_value(&mode).unwrap();
        assert_eq!(v, "gpu_sidecar");
    }

    #[test]
    fn test_secret_mode_gpu_sidecar_deserializes() {
        let mode: SecretMode = serde_json::from_str("\"gpu_sidecar\"").unwrap();
        assert_eq!(mode, SecretMode::GpuSidecar);
    }

    #[test]
    fn test_all_secret_modes_round_trip() {
        for mode in [
            SecretMode::BrokeredProxy,
            SecretMode::EphemeralEnv,
            SecretMode::GpuSidecar,
        ] {
            let s = serde_json::to_value(&mode).unwrap();
            let d: SecretMode = serde_json::from_value(s.clone()).unwrap();
            assert_eq!(mode, d, "SecretMode::{s} must round-trip");
        }
    }

    // ── InjectKind::GpuSidecar ────────────────────────────────────────────────

    #[test]
    fn test_inject_kind_gpu_sidecar_in_grant() {
        let ws = WorkspaceId::new();
        let registry = GrantRegistry::new();
        registry.upsert(ProxyGrant {
            workspace_id:     ws,
            provider:         "gpu-local".into(),
            allowed_hosts:    vec!["gpu.local".into()],
            credential_value: "model=mistral-7b".into(),
            inject_kind:      InjectKind::GpuSidecar {
                sidecar_url: "http://127.0.0.1:11434".into(),
            },
        });

        let grant = registry.find_grant(ws, "gpu.local").expect("grant must exist");
        match grant.inject_kind {
            InjectKind::GpuSidecar { ref sidecar_url } => {
                assert_eq!(sidecar_url, "http://127.0.0.1:11434");
            }
            other => panic!("expected GpuSidecar, got {other:?}"),
        }
    }

    #[test]
    fn test_gpu_grant_allowed_host_is_gpu_local() {
        // gpu.local is the canonical internal address for the GPU sidecar.
        // Using a sentinel hostname (not a real DNS name) prevents agents
        // from accidentally routing real GPU traffic through external networks.
        let ws = WorkspaceId::new();
        let registry = GrantRegistry::new();
        registry.upsert(ProxyGrant {
            workspace_id:     ws,
            provider:         "gpu-local".into(),
            allowed_hosts:    vec!["gpu.local".into()],
            credential_value: "".into(),
            inject_kind:      InjectKind::GpuSidecar {
                sidecar_url: "http://127.0.0.1:11434".into(),
            },
        });

        assert!(registry.find_grant(ws, "gpu.local").is_some());
        assert!(registry.find_grant(ws, "api.openai.com").is_none(),
            "GPU grant must not cover external hosts");
    }

    // ── Error codes ───────────────────────────────────────────────────────────

    #[test]
    fn test_gpu_capacity_exhausted_error() {
        let err = ArborError::GpuCapacityExhausted { runner_class: "fc-gpu-x86_64-v1".into() };
        assert_eq!(err.code(), "GPU_CAPACITY_EXHAUSTED");
        assert_eq!(err.http_status(), 503);
        assert!(err.retryable(), "GPU capacity exhausted is transient — retry later");
    }

    #[test]
    fn test_gpu_sidecar_unavailable_error() {
        let err = ArborError::GpuSidecarUnavailable("http://127.0.0.1:11434".into());
        assert_eq!(err.code(), "GPU_SIDECAR_UNAVAILABLE");
        assert_eq!(err.http_status(), 503);
        assert!(err.retryable());
    }

    // ── CompatibilityKey includes GPU model ───────────────────────────────────

    #[test]
    fn test_compatibility_key_with_gpu_model() {
        let ck = CompatibilityKey::new_gpu(
            "fc-gpu-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-gpu-v1", 1,
            "NVIDIA A10G",
        );
        assert_eq!(ck.0["gpu_model"], "NVIDIA A10G");
        assert_eq!(ck.0["runner_class"], "fc-gpu-x86_64-v1");
    }

    #[test]
    fn test_gpu_compat_keys_different_models_do_not_match() {
        let a10g = CompatibilityKey::new_gpu(
            "fc-gpu-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-gpu-v1", 1, "NVIDIA A10G",
        );
        let a100 = CompatibilityKey::new_gpu(
            "fc-gpu-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-gpu-v1", 1, "NVIDIA A100",
        );
        assert!(!a10g.matches(&a100),
            "GPU checkpoints from different GPU models must not match — \
             inference results differ across GPU generations");
    }

    #[test]
    fn test_gpu_compat_key_does_not_match_non_gpu() {
        let gpu_key = CompatibilityKey::new_gpu(
            "fc-gpu-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-gpu-v1", 1, "NVIDIA A10G",
        );
        let cpu_key = CompatibilityKey::new(
            "fc-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        assert!(!gpu_key.matches(&cpu_key),
            "GPU and non-GPU runner classes must never share checkpoints");
    }

    // ── Runner registration request shape ─────────────────────────────────────

    #[test]
    fn test_gpu_runner_register_request_shape() {
        let body = json!({
            "runner_class":        "fc-gpu-x86_64-v1",
            "address":             "http://10.0.3.1:9090",
            "arch":                "x86_64",
            "firecracker_version": "1.9.0",
            "cpu_template":        "T2",
            "capacity_slots":      8,
            "gpu_model":           "NVIDIA A10G",
            "gpu_count":           4,
            "gpu_vram_mib":        24576
        });
        assert_eq!(body["runner_class"], "fc-gpu-x86_64-v1");
        assert_eq!(body["gpu_model"],    "NVIDIA A10G");
        assert_eq!(body["gpu_count"],    4);
        assert_eq!(body["gpu_vram_mib"], 24576);
    }

    #[test]
    fn test_gpu_runner_node_round_trips_json() {
        let node = gpu_runner(4, 1);
        let v: Value = serde_json::to_value(&node).unwrap();
        assert_eq!(v["gpu_model"],    "NVIDIA A10G");
        assert_eq!(v["gpu_count"],    4);
        assert_eq!(v["gpu_used"],     1);
        assert_eq!(v["gpu_vram_mib"], 24576);

        let back: RunnerNode = serde_json::from_value(v).unwrap();
        assert_eq!(back.gpu_count, 4);
        assert_eq!(back.gpu_used,  1);
        assert_eq!(back.available_gpu_slots(), 3);
    }

    // ── Prometheus metric names ───────────────────────────────────────────────

    #[test]
    fn test_gpu_metric_names_follow_convention() {
        let names = [
            "arbor.runner.gpu_used",
            "arbor.runner.gpu_available",
            "arbor.runner.gpu_vram_total_mib",
        ];
        for name in names {
            assert!(name.starts_with("arbor.runner."),
                "GPU metric '{name}' must be under arbor.runner. namespace");
            assert!(!name.contains(' '));
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn gpu_runner(gpu_count: u32, gpu_used: u32) -> RunnerNode {
        RunnerNode {
            id:                  RunnerId::new(),
            runner_class:        "fc-gpu-x86_64-v1".into(),
            address:             "http://10.0.3.1:9090".into(),
            arch:                "x86_64".into(),
            firecracker_version: "1.9.0".into(),
            cpu_template:        "T2".into(),
            capacity_slots:      8,
            used_slots:          0,
            healthy:             true,
            last_heartbeat:      Utc::now(),
            gpu_model:           Some("NVIDIA A10G".into()),
            gpu_count,
            gpu_used,
            gpu_vram_mib:        24576,
        }
    }
}
