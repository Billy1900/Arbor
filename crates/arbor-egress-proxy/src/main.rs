use anyhow::Result;
use arbor_egress_proxy::{GrantRegistry, ProxyState};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "arbor_egress_proxy=info".into()),
        )
        .json()
        .init();

    let bind = std::env::var("ARBOR_PROXY_BIND").unwrap_or_else(|_| "0.0.0.0:3128".into());
    let registry = GrantRegistry::new();
    let state = ProxyState::new(registry);

    info!(bind, "starting arbor-egress-proxy");
    arbor_egress_proxy::run_proxy(&bind, state).await
}
