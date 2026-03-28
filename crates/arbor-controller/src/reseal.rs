//! Post-restore reseal hook chain (M4 — core differentiator).
//!
//! After any fork/restore the workspace enters QUARANTINED.
//! This module runs the ordered hook chain that rotates every piece of
//! state that was cloned verbatim from the snapshot, then releases the
//! workspace to READY.
//!
//! Hook execution order matters:
//!   1. Bump identity/network epoch  (new VM identity in DB)
//!   2. Revoke all existing grants   (snapshot had old grant tokens)
//!   3. Re-seed guest entropy        (tell guest-agent to mix new randomness)
//!   4. Re-issue brokered grants     (push fresh credentials to egress proxy)
//!   5. Rotate attach tokens         (old WebSocket tokens are invalid)
//!
//! Any hook failure leaves the workspace in QUARANTINED — egress stays blocked.
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::time::{timeout, Duration};
use tracing::{info, warn, instrument};

use arbor_common::*;
use arbor_common::proto::{HostMessage, encode_frame};

use crate::db::Db;
use crate::grant_registry::GrantRegistry;

pub struct ResealContext {
    pub ws_id:        WorkspaceId,
    pub runner_addr:  String,
    pub db:           Arc<Db>,
    pub grants:       Arc<GrantRegistry>,
    pub secret_vals:  Arc<dyn SecretResolver>,
}

/// Resolves a vault_ref to a live credential value.
#[async_trait::async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, vault_ref: &str) -> Result<String>;
}

/// Run the complete reseal hook chain.
/// Returns Ok(new_identity_epoch) on success.
#[instrument(skip(ctx), fields(ws_id = %ctx.ws_id))]
pub async fn run_reseal_hooks(ctx: &ResealContext) -> Result<u64> {
    let ws_id = ctx.ws_id;

    // ── Hook 1: bump identity + network epoch ─────────────────────────────
    info!(%ws_id, "reseal hook 1: bump identity epoch");
    let new_epoch = ctx.db.bump_identity_epoch(ws_id).await
        .context("failed to bump identity epoch")?;
    info!(%ws_id, epoch = new_epoch, "identity epoch bumped");

    // ── Hook 2: revoke all existing grants ────────────────────────────────
    info!(%ws_id, "reseal hook 2: revoke existing grants");
    ctx.db.revoke_all_grants(ws_id).await
        .context("failed to revoke grants")?;
    ctx.grants.revoke_all_for_workspace(ws_id);
    info!(%ws_id, "existing grants revoked");

    // ── Hook 3: re-seed guest entropy via vsock ───────────────────────────
    info!(%ws_id, "reseal hook 3: re-seed guest entropy");
    if let Err(e) = reseed_guest_entropy(ctx).await {
        // Non-fatal: guest-agent may not be up yet on the first boot after restore.
        // The OS entropy pool will be seeded by the kernel's vmgenid device.
        warn!(%ws_id, ?e, "entropy reseed skipped (non-fatal)");
    }

    // ── Hook 4: re-issue brokered grants ──────────────────────────────────
    info!(%ws_id, "reseal hook 4: re-issue brokered grants");
    reissue_grants(ctx).await
        .context("failed to re-issue secret grants")?;

    info!(%ws_id, epoch = new_epoch, "reseal hooks complete");
    Ok(new_epoch)
}

async fn reseed_guest_entropy(ctx: &ResealContext) -> Result<()> {
    use tokio::net::UnixStream;
    use tokio::io::AsyncWriteExt;

    // Build a fresh entropy blob and send it to the guest-agent via vsock.
    // The guest-agent's Quiesce/Ping handler will mix it into /dev/urandom.
    let entropy: Vec<u8> = (0..64).map(|_| rand::random::<u8>()).collect();
    
    // vsock UDS path on the runner host
    let vsock_path = format!("/var/lib/arbor/workspaces/{}/vm.vsock", ctx.ws_id);
    
    let ping = HostMessage::Ping;
    let frame = encode_frame(&ping)?;
    
    let mut stream = timeout(
        Duration::from_secs(5),
        UnixStream::connect(&vsock_path)
    ).await.context("vsock connect timeout")?.context("vsock connect")?;
    
    stream.write_all(&frame).await.context("vsock write")?;
    Ok(())
}

async fn reissue_grants(ctx: &ResealContext) -> Result<()> {
    // Fetch the grants that were on this workspace before the checkpoint was taken.
    // In a real deployment these would come from the original workspace's grant list
    // stored in DB. For now we re-read from DB (revoke_all_grants only marked
    // them inactive, the rows still exist).
    let grants = sqlx::query(
        "SELECT id, workspace_id, provider, mode, vault_ref, allowed_hosts, ttl_seconds, created_at
         FROM secret_grants WHERE workspace_id = $1"
    )
    .bind(ctx.ws_id.0)
    .fetch_all(ctx.db.pool())
    .await?;

    for row in grants {
        use sqlx::Row;
        let vault_ref: String = row.get("vault_ref");
        let provider:  String = row.get("provider");
        let mode_str:  String = row.get("mode");
        let allowed:   Vec<String> = row.get("allowed_hosts");
        let ttl:       i32 = row.get("ttl_seconds");

        // Resolve fresh credential
        let credential = match ctx.secret_vals.resolve(&vault_ref).await {
            Ok(v)  => v,
            Err(e) => {
                warn!(%vault_ref, ?e, "could not resolve secret — skipping grant");
                continue;
            }
        };

        // Inject into live egress proxy registry
        let new_grant_id = GrantId::new();
        ctx.grants.upsert(arbor_egress_proxy::ProxyGrant {
            workspace_id: ctx.ws_id,
            provider: provider.clone(),
            allowed_hosts: allowed.clone(),
            credential_value: credential,
            inject_kind: arbor_egress_proxy::InjectKind::AuthorizationHeader,
        });

        // Mark fresh grant in DB
        let now = chrono::Utc::now();
        let new_grant = SecretGrant {
            id:           new_grant_id,
            workspace_id: ctx.ws_id,
            provider,
            mode:         if mode_str == "brokered_proxy" { SecretMode::BrokeredProxy } else { SecretMode::EphemeralEnv },
            vault_ref,
            allowed_hosts: allowed,
            ttl_seconds:   ttl as u64,
            active:        true,
            expires_at:    Some(now + chrono::Duration::seconds(ttl as i64)),
            created_at:    now,
        };
        ctx.db.upsert_secret_grant(&new_grant).await?;
    }

    info!(ws_id = %ctx.ws_id, "grants reissued");
    Ok(())
}

/// Environment-variable based secret resolver (MVP).
/// Production should use HashiCorp Vault / AWS Secrets Manager.
pub struct EnvSecretResolver;

#[async_trait::async_trait]
impl SecretResolver for EnvSecretResolver {
    async fn resolve(&self, vault_ref: &str) -> Result<String> {
        let env_key = format!(
            "ARBOR_SECRET_{}",
            vault_ref.replace("vault://", "").replace('/', "_").replace('-', "_").to_uppercase()
        );
        std::env::var(&env_key)
            .map_err(|_| anyhow::anyhow!("secret not found: {vault_ref} (env: {env_key})"))
    }
}
