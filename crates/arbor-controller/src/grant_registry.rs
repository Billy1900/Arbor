//! Shared in-process grant registry — the bridge between SecretBroker and EgressProxy.
//!
//! Both the secret broker (which manages grant lifecycle in DB) and the egress proxy
//! (which enforces allowlists and injects credentials) need to read the same set of
//! live grants. This registry is the shared source of truth held in process memory.
//!
//! Wiring: Arc<GrantRegistry> is created once in main() and passed to both services.
use arbor_egress_proxy::ProxyGrant;
use arbor_common::{WorkspaceId, GrantId};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Re-export so callers don't need to depend on arbor-egress-proxy directly.
pub use arbor_egress_proxy::{GrantRegistry, InjectKind};

/// Extend GrantRegistry with revoke_all_for_workspace (already on the type,
/// but we need it accessible from reseal hooks without knowing proxy internals).
pub trait GrantRegistryExt {
    fn revoke_all_for_workspace(&self, ws_id: WorkspaceId);
}

impl GrantRegistryExt for GrantRegistry {
    fn revoke_all_for_workspace(&self, ws_id: WorkspaceId) {
        self.revoke_all_for_workspace(ws_id);
    }
}
