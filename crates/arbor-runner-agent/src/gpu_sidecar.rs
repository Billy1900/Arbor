/// GPU inference sidecar health monitor and metrics reporter.
///
/// On GPU runner hosts, a local inference sidecar (llama.cpp / vLLM / Ollama)
/// runs alongside the runner agent. This module:
///   1. Periodically health-checks the sidecar via GET /health (or equivalent)
///   2. Reports GPU utilization metrics to Prometheus
///   3. Warns when the sidecar is unreachable so operators know to investigate
///
/// The sidecar itself is not managed by Arbor — it must be started separately
/// and its URL passed via ARBOR_RUNNER__GPU_SIDECAR_URL.
use std::time::Duration;
use tracing::{info, warn};

const HEALTH_INTERVAL_SECS: u64 = 30;
const HEALTH_TIMEOUT_SECS:  u64 = 5;

/// Background task: health-check the GPU inference sidecar on a fixed interval
/// and update Prometheus gauges for GPU capacity reporting.
pub async fn health_loop(sidecar_url: String, gpu_count: u32, gpu_vram_mib: u32) {
    // Publish static capacity metrics immediately on startup
    metrics::gauge!("arbor.runner.gpu_available").set(gpu_count as f64);
    metrics::gauge!("arbor.runner.gpu_vram_total_mib").set(
        (gpu_count * gpu_vram_mib) as f64
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS))
        .build()
        .expect("build reqwest client");

    let health_url = format!(
        "{}/health",
        sidecar_url.trim_end_matches('/')
    );

    let mut interval = tokio::time::interval(Duration::from_secs(HEALTH_INTERVAL_SECS));
    let mut consecutive_failures: u32 = 0;

    loop {
        interval.tick().await;

        match client.get(&health_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if consecutive_failures > 0 {
                    info!(sidecar = %sidecar_url, "GPU sidecar recovered");
                }
                consecutive_failures = 0;
                metrics::gauge!("arbor.runner.gpu_sidecar_healthy").set(1.0);
            }
            Ok(resp) => {
                consecutive_failures += 1;
                warn!(
                    sidecar = %sidecar_url,
                    status  = %resp.status(),
                    consecutive_failures,
                    "GPU sidecar returned non-success"
                );
                metrics::gauge!("arbor.runner.gpu_sidecar_healthy").set(0.0);
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(
                    ?e,
                    sidecar = %sidecar_url,
                    consecutive_failures,
                    "GPU sidecar unreachable"
                );
                metrics::gauge!("arbor.runner.gpu_sidecar_healthy").set(0.0);

                if consecutive_failures == 3 {
                    tracing::error!(
                        sidecar = %sidecar_url,
                        "GPU sidecar has been unreachable for 3 consecutive checks — \
                         workspace GPU requests will fail until it recovers"
                    );
                }
            }
        }
    }
}
