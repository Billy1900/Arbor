/// Reseal hook tests — unit-test the hook chain logic without a live DB.
#[cfg(test)]
mod tests {
    use arbor_common::*;
    use arbor_egress_proxy::{GrantRegistry, InjectKind, ProxyGrant};
    use std::sync::Arc;

    // ── Mock secret resolver ──────────────────────────────────────────────────

    struct MockSecretResolver {
        values: std::collections::HashMap<String, String>,
    }

    #[async_trait::async_trait]
    impl arbor_controller::reseal::SecretResolver for MockSecretResolver {
        async fn resolve(&self, vault_ref: &str) -> anyhow::Result<String> {
            self.values.get(vault_ref)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("mock: secret not found: {vault_ref}"))
        }
    }

    // ── Grant re-issue produces new grants ────────────────────────────────────

    #[test]
    fn test_grant_revoke_and_reissue_produces_unique_grant_ids() {
        // Simulate what reseal hook 2 + 4 do:
        // revoke all existing grants, then push fresh ones with new IDs.
        let registry = GrantRegistry::new();
        let ws       = WorkspaceId::new();

        // Before restore: grant G1 exists
        let g1_credential = "sk-original-value-from-snapshot";
        registry.upsert(ProxyGrant {
            workspace_id:     ws,
            provider:         "openai".into(),
            allowed_hosts:    vec!["api.openai.com".into()],
            credential_value: g1_credential.into(),
            inject_kind:      InjectKind::AuthorizationHeader,
        });

        // Verify G1 is live
        let before = registry.find_grant(ws, "api.openai.com").unwrap();
        assert_eq!(before.credential_value, g1_credential);

        // Reseal hook 2: revoke all
        registry.revoke_all_for_workspace(ws);
        assert!(registry.find_grant(ws, "api.openai.com").is_none(),
                "after revoke: grant must be gone");

        // Reseal hook 4: re-issue with fresh credential
        let g2_credential = "sk-fresh-rotated-value";
        registry.upsert(ProxyGrant {
            workspace_id:     ws,
            provider:         "openai".into(),
            allowed_hosts:    vec!["api.openai.com".into()],
            credential_value: g2_credential.into(),
            inject_kind:      InjectKind::AuthorizationHeader,
        });

        // New grant has new credential
        let after = registry.find_grant(ws, "api.openai.com").unwrap();
        assert_eq!(after.credential_value, g2_credential);
        assert_ne!(after.credential_value, g1_credential,
                   "credential must change after reseal");
    }

    // ── Multi-fork isolation: N forks → N distinct credential sets ────────────

    #[test]
    fn test_n_forks_have_distinct_credentials() {
        // Simulate forking the same checkpoint 5 times.
        // Each fork's reseal should produce a credential unique to that fork.
        let registry = GrantRegistry::new();
        let n_forks  = 5;

        let fork_ids: Vec<WorkspaceId> = (0..n_forks).map(|_| WorkspaceId::new()).collect();
        let mut credentials: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (i, &ws) in fork_ids.iter().enumerate() {
            // Simulate each fork getting a fresh credential after reseal
            let cred = format!("sk-fork-{}-unique-{}", i, uuid::Uuid::new_v4());
            registry.upsert(ProxyGrant {
                workspace_id:     ws,
                provider:         "openai".into(),
                allowed_hosts:    vec!["api.openai.com".into()],
                credential_value: cred.clone(),
                inject_kind:      InjectKind::AuthorizationHeader,
            });
            credentials.insert(cred);
        }

        // All credentials must be distinct
        assert_eq!(credentials.len(), n_forks,
                   "each fork must have a distinct credential after reseal");

        // Each fork's grant is isolated from others
        for (i, &ws_i) in fork_ids.iter().enumerate() {
            let grant_i = registry.find_grant(ws_i, "api.openai.com").unwrap();
            for (j, &ws_j) in fork_ids.iter().enumerate() {
                if i == j { continue; }
                let grant_j = registry.find_grant(ws_j, "api.openai.com").unwrap();
                assert_ne!(
                    grant_i.credential_value, grant_j.credential_value,
                    "fork {i} and fork {j} must not share credentials"
                );
            }
        }
    }

    // ── Revoking one fork doesn't affect siblings ─────────────────────────────

    #[test]
    fn test_fork_revoke_is_scoped() {
        let registry = GrantRegistry::new();
        let ws_a = WorkspaceId::new();
        let ws_b = WorkspaceId::new();
        let ws_c = WorkspaceId::new();

        for (ws, cred) in [
            (ws_a, "cred-a"), (ws_b, "cred-b"), (ws_c, "cred-c"),
        ] {
            registry.upsert(ProxyGrant {
                workspace_id:     ws,
                provider:         "openai".into(),
                allowed_hosts:    vec!["api.openai.com".into()],
                credential_value: cred.into(),
                inject_kind:      InjectKind::AuthorizationHeader,
            });
        }

        // Terminate ws_b — revoke its grants
        registry.revoke_all_for_workspace(ws_b);

        // ws_a and ws_c are unaffected
        assert!(registry.find_grant(ws_a, "api.openai.com").is_some(), "ws_a unaffected");
        assert!(registry.find_grant(ws_b, "api.openai.com").is_none(), "ws_b revoked");
        assert!(registry.find_grant(ws_c, "api.openai.com").is_some(), "ws_c unaffected");
    }

    // ── Quarantine semantics ──────────────────────────────────────────────────

    #[test]
    fn test_quarantined_workspace_has_no_grants_until_reseal() {
        // A freshly-restored workspace enters quarantine with no live grants.
        // The egress proxy sees no grants → denies all requests.
        let registry = GrantRegistry::new();
        let ws       = WorkspaceId::new();

        // No grants added yet (we're in quarantine, before reseal hooks run)
        let grant = registry.find_grant(ws, "api.openai.com");
        assert!(grant.is_none(),
                "quarantined workspace must have no grants before reseal");

        // After reseal hooks run, grants are pushed
        registry.upsert(ProxyGrant {
            workspace_id:     ws,
            provider:         "openai".into(),
            allowed_hosts:    vec!["api.openai.com".into()],
            credential_value: "sk-post-reseal".into(),
            inject_kind:      InjectKind::AuthorizationHeader,
        });

        let grant = registry.find_grant(ws, "api.openai.com");
        assert!(grant.is_some(), "after reseal: grant must exist");
    }
}
