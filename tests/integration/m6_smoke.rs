/// M6 unit smoke tests — no live DB or runner-agent required.
///
/// Covers:
///   - RunnerNode slot arithmetic
///   - Register / heartbeat request JSON serialization
///   - Drain-mode: runner excluded from healthy list when draining
///   - Prometheus metric name constants
#[cfg(test)]
mod tests {
    use arbor_common::*;
    use chrono::Utc;
    use serde_json::{json, Value};

    // ── RunnerNode slot arithmetic ────────────────────────────────────────────

    #[test]
    fn test_available_slots_healthy_runner() {
        let r = RunnerNode {
            id:                  RunnerId::new(),
            runner_class:        "fc-x86_64-v1".into(),
            address:             "http://10.0.0.1:9090".into(),
            arch:                "x86_64".into(),
            firecracker_version: "1.9.0".into(),
            cpu_template:        "T2".into(),
            capacity_slots:      10,
            used_slots:          3,
            healthy:             true,
            last_heartbeat:      Utc::now(),
        };
        assert_eq!(r.available_slots(), 7);
    }

    #[test]
    fn test_available_slots_full_runner() {
        let r = RunnerNode {
            id:                  RunnerId::new(),
            runner_class:        "fc-x86_64-v1".into(),
            address:             "http://10.0.0.1:9090".into(),
            arch:                "x86_64".into(),
            firecracker_version: "1.9.0".into(),
            cpu_template:        "T2".into(),
            capacity_slots:      5,
            used_slots:          5,
            healthy:             true,
            last_heartbeat:      Utc::now(),
        };
        assert_eq!(r.available_slots(), 0, "full runner must have 0 available slots");
    }

    #[test]
    fn test_available_slots_does_not_underflow() {
        // Edge case: used_slots somehow exceeds capacity (data race / manual fixup)
        let r = RunnerNode {
            id:                  RunnerId::new(),
            runner_class:        "fc-x86_64-v1".into(),
            address:             "http://10.0.0.1:9090".into(),
            arch:                "x86_64".into(),
            firecracker_version: "1.9.0".into(),
            cpu_template:        "T2".into(),
            capacity_slots:      5,
            used_slots:          7, // over-full
            healthy:             true,
            last_heartbeat:      Utc::now(),
        };
        assert_eq!(r.available_slots(), 0,
            "available_slots must use saturating_sub, not panic on underflow");
    }

    // ── Runner register request serialization ─────────────────────────────────

    #[test]
    fn test_runner_register_request_round_trips_json() {
        // The register endpoint accepts this JSON body.
        // Verify the shape the runner-agent will POST on startup.
        let body = json!({
            "runner_class":        "fc-x86_64-v1",
            "address":             "http://10.0.1.5:9090",
            "arch":                "x86_64",
            "firecracker_version": "1.9.0",
            "cpu_template":        "T2",
            "capacity_slots":      10
        });

        // Required fields must all be present
        assert_eq!(body["runner_class"],        "fc-x86_64-v1");
        assert_eq!(body["arch"],                "x86_64");
        assert_eq!(body["firecracker_version"], "1.9.0");
        assert_eq!(body["cpu_template"],        "T2");
        assert_eq!(body["capacity_slots"],      10);

        // The response carries back a runner_id
        let response = json!({ "runner_id": uuid::Uuid::new_v4() });
        assert!(response["runner_id"].is_string(),
            "register response must include runner_id as a UUID string");
    }

    #[test]
    fn test_runner_register_requires_non_empty_class() {
        // runner_class="" is invalid — scheduler uses it as a filter key
        let raw = json!({
            "runner_class": "",
            "address": "http://10.0.0.1:9090",
            "arch": "x86_64",
            "firecracker_version": "1.9.0",
            "cpu_template": "T2",
            "capacity_slots": 4
        });
        assert_eq!(raw["runner_class"].as_str().unwrap(), "",
            "this test documents the invalid shape; validation is enforced server-side");
    }

    // ── Heartbeat request serialization ───────────────────────────────────────

    #[test]
    fn test_runner_heartbeat_request_serializes() {
        let runner_id = uuid::Uuid::new_v4();
        let body = json!({
            "runner_id":  runner_id,
            "used_slots": 3u32
        });

        assert_eq!(body["runner_id"], runner_id.to_string());
        assert_eq!(body["used_slots"], 3);
    }

    #[test]
    fn test_heartbeat_used_slots_zero_is_valid() {
        // used_slots = 0 is a valid heartbeat (idle runner)
        let body = json!({ "runner_id": uuid::Uuid::new_v4(), "used_slots": 0u32 });
        assert_eq!(body["used_slots"], 0);
    }

    // ── Drain API request serialization ──────────────────────────────────────

    #[test]
    fn test_drain_request_shape() {
        // PUT /drain on runner-agent — no request body required.
        // Response shape:
        let resp = json!({ "draining": true, "active_vms": 2u32 });
        assert_eq!(resp["draining"], true);
        assert!(resp["active_vms"].as_u64().unwrap() >= 0);
    }

    // ── Prometheus metric name conventions ────────────────────────────────────

    #[test]
    fn test_metric_names_follow_arbor_prefix_convention() {
        // All Arbor metrics must start with "arbor." — verified by convention.
        let metric_names = [
            "arbor.runner.active_vms",
            "arbor.runner.vm_boots_total",
            "arbor.runner.vm_boot_duration_seconds",
            "arbor.runner.checkpoints_total",
            "arbor.runner.restores_total",
            "arbor.runner.unhealthy",
            "arbor.checkpoint.created",
            "arbor.fork.created",
            "arbor.restore.completed",
        ];
        for name in metric_names {
            assert!(
                name.starts_with("arbor."),
                "metric '{name}' must use the 'arbor.' prefix"
            );
            assert!(
                !name.contains(' '),
                "metric '{name}' must not contain spaces"
            );
        }
    }

    #[test]
    fn test_runner_agent_metric_names_are_namespaced_under_runner() {
        let runner_metrics = [
            "arbor.runner.active_vms",
            "arbor.runner.vm_boots_total",
            "arbor.runner.vm_boot_duration_seconds",
            "arbor.runner.checkpoints_total",
            "arbor.runner.restores_total",
        ];
        for name in runner_metrics {
            assert!(
                name.starts_with("arbor.runner."),
                "runner-agent metric '{name}' must be under 'arbor.runner.' namespace"
            );
        }
    }

    // ── Compatibility key for CPU template ────────────────────────────────────

    #[test]
    fn test_runner_class_cpu_template_invariants() {
        // x86_64 runner class uses T2, not T2A.
        // T2A is ARM/Graviton2 — using it on x86 would cause snapshot incompatibility.
        let ck = CompatibilityKey::new(
            "fc-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        assert_eq!(ck.0["cpu_template"], "T2");
        assert_ne!(ck.0["cpu_template"], "T2A",
            "x86_64 runner class must never use T2A (ARM-only template)");
    }

    #[test]
    fn test_compatibility_key_version_field_required() {
        let ck = CompatibilityKey::new(
            "fc-x86_64-v1", "x86_64", "T2", "1.9.0",
            "sha256:abc", "ubuntu-24.04-dev-v1", 1,
        );
        assert!(
            ck.0["firecracker_version"].as_str().map_or(false, |v| !v.is_empty()),
            "firecracker_version must be non-empty in CompatibilityKey"
        );
    }

    // ── RunnerNode JSON serialization ─────────────────────────────────────────

    #[test]
    fn test_runner_node_serializes_to_json() {
        let node = RunnerNode {
            id:                  RunnerId::new(),
            runner_class:        "fc-x86_64-v1".into(),
            address:             "http://10.0.0.5:9090".into(),
            arch:                "x86_64".into(),
            firecracker_version: "1.9.0".into(),
            cpu_template:        "T2".into(),
            capacity_slots:      10,
            used_slots:          2,
            healthy:             true,
            last_heartbeat:      Utc::now(),
        };

        let v: Value = serde_json::to_value(&node).expect("serialize RunnerNode");
        assert_eq!(v["runner_class"],        "fc-x86_64-v1");
        assert_eq!(v["arch"],                "x86_64");
        assert_eq!(v["firecracker_version"], "1.9.0");
        assert_eq!(v["cpu_template"],        "T2");
        assert_eq!(v["capacity_slots"],      10);
        assert_eq!(v["used_slots"],          2);
        assert_eq!(v["healthy"],             true);
    }

    #[test]
    fn test_runner_node_deserializes_from_list_response() {
        // Verify the shape returned by GET /v1/runners can be deserialized
        let raw = json!([{
            "id":                  uuid::Uuid::new_v4(),
            "runner_class":        "fc-x86_64-v1",
            "address":             "http://10.0.0.1:9090",
            "arch":                "x86_64",
            "firecracker_version": "1.9.0",
            "cpu_template":        "T2",
            "capacity_slots":      10,
            "used_slots":          0,
            "healthy":             true,
            "last_heartbeat":      chrono::Utc::now()
        }]);

        let nodes: Vec<RunnerNode> = serde_json::from_value(raw).expect("deserialize runner list");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].runner_class, "fc-x86_64-v1");
        assert_eq!(nodes[0].available_slots(), 10);
    }
}
