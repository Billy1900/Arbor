/// M8 unit tests — ARM64 runner class (`fc-arm64-v1`).
///
/// All tests run without a live DB or runner-agent.
/// Covers:
///   - Runner class ↔ arch ↔ cpu_template consistency rules
///   - CompatibilityKey enforcement for ARM64
///   - Registration request validation helpers
///   - Error code and retryability for new RunnerArchMismatch error
#[cfg(test)]
mod tests {
    use arbor_common::*;
    use serde_json::{json, Value};

    // ── Runner class constants ─────────────────────────────────────────────────

    #[test]
    fn test_known_runner_classes() {
        // Document the two supported classes and their required parameters.
        let classes: &[(&str, &str, &str)] = &[
            ("fc-x86_64-v1", "x86_64",  "T2"),
            ("fc-arm64-v1",  "aarch64", "T2A"),
        ];
        for (class, arch, template) in classes {
            assert!(!class.is_empty());
            assert!(!arch.is_empty());
            assert!(!template.is_empty());
        }
    }

    // ── CompatibilityKey for ARM64 ────────────────────────────────────────────

    #[test]
    fn test_arm64_compatibility_key_uses_t2a() {
        let ck = CompatibilityKey::new(
            "fc-arm64-v1", "aarch64", "T2A", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        assert_eq!(ck.0["runner_class"],  "fc-arm64-v1");
        assert_eq!(ck.0["arch"],          "aarch64");
        assert_eq!(ck.0["cpu_template"],  "T2A",
            "ARM64 runner class must use T2A (Graviton2), not T2");
    }

    #[test]
    fn test_x86_compatibility_key_uses_t2() {
        let ck = CompatibilityKey::new(
            "fc-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        assert_eq!(ck.0["arch"],         "x86_64");
        assert_eq!(ck.0["cpu_template"], "T2",
            "x86_64 runner class must use T2, not T2A");
    }

    #[test]
    fn test_arm64_and_x86_keys_do_not_match() {
        let arm = CompatibilityKey::new(
            "fc-arm64-v1", "aarch64", "T2A", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        let x86 = CompatibilityKey::new(
            "fc-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        assert!(!arm.matches(&x86),
            "ARM64 and x86_64 compatibility keys must never match — \
             restoring an ARM snapshot on an x86 host would corrupt the guest");
    }

    #[test]
    fn test_same_arm64_keys_match() {
        let a = CompatibilityKey::new(
            "fc-arm64-v1", "aarch64", "T2A", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        let b = CompatibilityKey::new(
            "fc-arm64-v1", "aarch64", "T2A", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        assert!(a.matches(&b));
    }

    #[test]
    fn test_arm64_fc_version_mismatch_does_not_match() {
        let a = CompatibilityKey::new(
            "fc-arm64-v1", "aarch64", "T2A", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        let b = CompatibilityKey::new(
            "fc-arm64-v1", "aarch64", "T2A", "1.8.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        assert!(!a.matches(&b),
            "ARM64 keys with different FC versions must not match");
    }

    // ── Registration validation helpers ───────────────────────────────────────

    /// Mirrors the validation logic in `routes/runners.rs::validate_runner_class`.
    fn validate_runner_registration(
        runner_class: &str,
        arch: &str,
        cpu_template: &str,
    ) -> Result<(), String> {
        match runner_class {
            "fc-x86_64-v1" => {
                if arch != "x86_64" {
                    return Err(format!(
                        "runner_class fc-x86_64-v1 requires arch=x86_64, got {arch}"
                    ));
                }
                if cpu_template != "T2" {
                    return Err(format!(
                        "runner_class fc-x86_64-v1 requires cpu_template=T2, got {cpu_template}"
                    ));
                }
            }
            "fc-arm64-v1" => {
                if arch != "aarch64" {
                    return Err(format!(
                        "runner_class fc-arm64-v1 requires arch=aarch64, got {arch}"
                    ));
                }
                if cpu_template != "T2A" {
                    return Err(format!(
                        "runner_class fc-arm64-v1 requires cpu_template=T2A, got {cpu_template}"
                    ));
                }
            }
            other => {
                return Err(format!("unknown runner_class: {other}"));
            }
        }
        Ok(())
    }

    #[test]
    fn test_valid_x86_registration() {
        assert!(validate_runner_registration("fc-x86_64-v1", "x86_64", "T2").is_ok());
    }

    #[test]
    fn test_valid_arm64_registration() {
        assert!(validate_runner_registration("fc-arm64-v1", "aarch64", "T2A").is_ok());
    }

    #[test]
    fn test_arm64_class_with_wrong_template_rejected() {
        let result = validate_runner_registration("fc-arm64-v1", "aarch64", "T2");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("T2A"), "error must mention required template T2A, got: {msg}");
    }

    #[test]
    fn test_arm64_class_with_wrong_arch_rejected() {
        let result = validate_runner_registration("fc-arm64-v1", "x86_64", "T2A");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("aarch64"), "error must mention required arch aarch64, got: {msg}");
    }

    #[test]
    fn test_x86_class_with_arm_template_rejected() {
        let result = validate_runner_registration("fc-x86_64-v1", "x86_64", "T2A");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("T2"), "error must mention required template T2, got: {msg}");
    }

    #[test]
    fn test_x86_class_with_arm_arch_rejected() {
        let result = validate_runner_registration("fc-x86_64-v1", "aarch64", "T2");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("x86_64"), "error must mention required arch x86_64, got: {msg}");
    }

    #[test]
    fn test_unknown_runner_class_rejected() {
        let result = validate_runner_registration("fc-riscv-v1", "riscv64", "none");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown runner_class"));
    }

    // ── ArborError: RunnerArchMismatch ────────────────────────────────────────

    #[test]
    fn test_runner_arch_mismatch_error_code() {
        let err = ArborError::RunnerArchMismatch {
            expected: "aarch64".into(),
            got: "x86_64".into(),
        };
        assert_eq!(err.code(), "RUNNER_ARCH_MISMATCH");
        assert_eq!(err.http_status(), 422);
        assert!(!err.retryable(),
            "arch mismatch is a permanent config error, not retryable");
    }

    #[test]
    fn test_runner_arch_mismatch_error_message() {
        let err = ArborError::RunnerArchMismatch {
            expected: "aarch64".into(),
            got: "x86_64".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("aarch64"), "error message must include expected arch");
        assert!(msg.contains("x86_64"), "error message must include actual arch");
    }

    // ── Register request serialization for ARM64 ──────────────────────────────

    #[test]
    fn test_arm64_register_request_shape() {
        let body = json!({
            "runner_class":        "fc-arm64-v1",
            "address":             "http://10.0.2.1:9090",
            "arch":                "aarch64",
            "firecracker_version": "1.9.0",
            "cpu_template":        "T2A",
            "capacity_slots":      8
        });
        assert_eq!(body["runner_class"],  "fc-arm64-v1");
        assert_eq!(body["arch"],          "aarch64");
        assert_eq!(body["cpu_template"],  "T2A");
    }

    #[test]
    fn test_arm64_runner_node_round_trips_json() {
        let node = RunnerNode {
            id:                  RunnerId::new(),
            runner_class:        "fc-arm64-v1".into(),
            address:             "http://10.0.2.1:9090".into(),
            arch:                "aarch64".into(),
            firecracker_version: "1.9.0".into(),
            cpu_template:        "T2A".into(),
            capacity_slots:      8,
            used_slots:          0,
            healthy:             true,
            last_heartbeat:      chrono::Utc::now(),
        };
        let v: Value = serde_json::to_value(&node).unwrap();
        assert_eq!(v["runner_class"], "fc-arm64-v1");
        assert_eq!(v["arch"],         "aarch64");
        assert_eq!(v["cpu_template"], "T2A");

        let back: RunnerNode = serde_json::from_value(v).unwrap();
        assert_eq!(back.runner_class, "fc-arm64-v1");
        assert_eq!(back.cpu_template, "T2A");
    }

}
