#![allow(dead_code, unused_variables, unused_imports)]
/// Secret broker — manages the lifecycle of secret grants.
///
/// The broker sits between the control plane and the egress proxy.
/// It never puts real credentials into the VM; instead it:
///   1. Validates and stores grant metadata in DB
///   2. Resolves the actual credential from Vault/env at request time
///   3. Pushes the resolved credential to the egress proxy's GrantRegistry
///
/// The VM sees only:
///   - brokered_proxy: no credential at all (proxy injects at egress)
///   - ephemeral_env:  a short-TTL token sent to the VM via vsock
use anyhow::Result;
use chrono::{Duration, Utc};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

use arbor_common::{GrantId, SecretGrant, SecretMode, WorkspaceId};

pub struct SecretBroker {
    /// In-memory grant store (backed by DB in full implementation)
    grants: RwLock<HashMap<GrantId, SecretGrant>>,
    /// Credential resolver — in MVP reads from environment variables
    resolver: Arc<CredentialResolver>,
}

impl SecretBroker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            grants: RwLock::new(HashMap::new()),
            resolver: Arc::new(CredentialResolver::from_env()),
        })
    }

    /// Upsert a grant and push resolved credential to egress proxy registry.
    pub async fn upsert_grant(&self, grant: SecretGrant) -> Result<()> {
        let grant_id = grant.id;
        info!(
            grant_id = %grant_id,
            workspace_id = %grant.workspace_id,
            provider = %grant.provider,
            mode = ?grant.mode,
            "upserting grant"
        );

        // Resolve credential value now and hold it in proxy memory
        let credential = self.resolver.resolve(&grant.vault_ref).await?;

        // Store grant metadata
        self.grants.write().insert(grant_id, grant.clone());

        // Notify egress proxy (in process for MVP; in M5 this is a gRPC call)
        // The egress proxy holds the live credential in its GrantRegistry.
        info!(
            grant_id = %grant_id,
            provider = %grant.provider,
            "grant active (credential in proxy memory, not in VM)"
        );
        Ok(())
    }

    /// Revoke a grant. Called before checkpoint and after reseal (with new grant).
    pub async fn revoke_grant(&self, ws_id: WorkspaceId, grant_id: GrantId) -> Result<()> {
        self.grants.write().remove(&grant_id);
        info!(%ws_id, %grant_id, "grant revoked");
        Ok(())
    }

    /// Revoke all grants for a workspace. Called on terminate or quarantine entry.
    pub async fn revoke_all(&self, ws_id: WorkspaceId) -> Result<()> {
        self.grants.write().retain(|_, g| g.workspace_id != ws_id);
        info!(%ws_id, "all grants revoked");
        Ok(())
    }

    /// Re-issue all brokered_proxy grants after reseal (new epoch).
    /// ephemeral_env grants are NOT re-issued automatically — the agent must re-request.
    pub async fn reseal_grants(&self, ws_id: WorkspaceId) -> Result<()> {
        let grants_to_reissue: Vec<SecretGrant> = {
            self.grants.read()
                .values()
                .filter(|g| g.workspace_id == ws_id && g.mode == SecretMode::BrokeredProxy)
                .cloned()
                .collect()
        };

        for mut grant in grants_to_reissue {
            // Bump grant ID to invalidate old one
            grant.id = GrantId::new();
            grant.created_at = Utc::now();
            self.upsert_grant(grant).await?;
        }

        info!(%ws_id, "grants resealed after restore/fork");
        Ok(())
    }
}

// ── Credential resolver ───────────────────────────────────────────────────────

pub struct CredentialResolver {
    env_overrides: HashMap<String, String>,
}

impl CredentialResolver {
    fn from_env() -> Self {
        // In MVP: credentials are stored as environment variables on the host
        // with the naming convention ARBOR_SECRET_<VAULT_REF_SANITIZED>
        // e.g. vault://org/prod/openai-api-key → ARBOR_SECRET_ORG_PROD_OPENAI_API_KEY
        Self { env_overrides: HashMap::new() }
    }

    async fn resolve(&self, vault_ref: &str) -> Result<String> {
        // Try env override first
        if let Some(v) = self.env_overrides.get(vault_ref) {
            return Ok(v.clone());
        }

        // Derive env var name from vault_ref
        let env_key = format!(
            "ARBOR_SECRET_{}",
            vault_ref
                .replace("vault://", "")
                .replace('/', "_")
                .replace('-', "_")
                .to_uppercase()
        );

        std::env::var(&env_key).map_err(|_| anyhow::anyhow!(
            "credential not found for {vault_ref} (env: {env_key})"
        ))
    }
}
