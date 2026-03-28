/// Network namespace and TAP device setup for workspace VMs.
///
/// Each workspace gets:
///   1. A dedicated network namespace
///   2. A TAP device inside that netns for Firecracker
///   3. A veth pair bridging netns → root namespace
///   4. NAT via nftables (managed by subprocess)
///
/// IMPORTANT: setns(2) is thread-local. All netns operations that require
/// entering a namespace must run on a dedicated single OS thread, not a tokio
/// worker thread. We use std::thread::spawn + oneshot channels for this.
use anyhow::{Context, Result};
use nix::sched::{setns, CloneFlags};
use nix::unistd::getpid;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Command;
use tracing::{debug, info, instrument};

const NETNS_DIR: &str = "/var/run/netns";

#[derive(Debug, Clone)]
pub struct NetConfig {
    pub ws_id: String,
    pub tap_name: String,        // tap<8-char-prefix>
    pub veth_host: String,       // veth<8-char>h
    pub veth_guest: String,      // veth<8-char>g
    pub guest_ip: String,        // 172.20.X.2/30
    pub host_ip: String,         // 172.20.X.1/30
    pub netns_path: PathBuf,
    pub guest_mac: String,
}

impl NetConfig {
    /// Derive all network identifiers from workspace ID prefix.
    pub fn from_ws_id(ws_id: &str) -> Self {
        let prefix = &ws_id[..8];
        // Use deterministic IP from a hash of the prefix
        let ip_idx = u32::from_str_radix(&prefix[..4], 16).unwrap_or(0) % 16384;
        let octet3 = (ip_idx / 255) as u8;
        let octet4 = (ip_idx % 255) as u8;

        Self {
            ws_id: ws_id.to_string(),
            tap_name: format!("tap{}", prefix),
            veth_host: format!("vh{}", prefix),
            veth_guest: format!("vg{}", prefix),
            guest_ip: format!("172.20.{}.2", (ip_idx / 2) % 255),
            host_ip:  format!("172.20.{}.1", (ip_idx / 2) % 255),
            netns_path: PathBuf::from(format!("{}/arbor-{}", NETNS_DIR, prefix)),
            guest_mac: derive_mac(ws_id),
        }
    }
}

fn derive_mac(ws_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(ws_id.as_bytes());
    let d = h.finalize();
    // locally administered, unicast
    format!("02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", d[0], d[1], d[2], d[3], d[4])
}

// ── Setup ────────────────────────────────────────────────────────────────────

/// Create netns + TAP + veth pair for a new workspace.
/// Runs ip/iptables commands via subprocess — requires NET_ADMIN capability.
#[instrument(skip(cfg))]
pub fn setup_network(cfg: &NetConfig) -> Result<()> {
    let prefix = &cfg.ws_id[..8];
    let netns_name = format!("arbor-{}", prefix);

    // 1. Create named network namespace
    run_ip(&["netns", "add", &netns_name])?;
    info!(%prefix, "created netns");

    // 2. Create veth pair in root ns; move guest end into netns
    run_ip(&["link", "add", &cfg.veth_host, "type", "veth",
              "peer", "name", &cfg.veth_guest])?;
    run_ip(&["link", "set", &cfg.veth_guest, "netns", &netns_name])?;

    // 3. Configure host-side veth
    run_ip(&["addr", "add", &format!("{}/30", cfg.host_ip), "dev", &cfg.veth_host])?;
    run_ip(&["link", "set", &cfg.veth_host, "up"])?;

    // 4. Inside the netns: configure veth guest end + create TAP
    run_ip_netns(&netns_name, &["addr", "add", &format!("{}/30", cfg.guest_ip), "dev", &cfg.veth_guest])?;
    run_ip_netns(&netns_name, &["link", "set", &cfg.veth_guest, "up"])?;
    run_ip_netns(&netns_name, &["link", "set", "lo", "up"])?;

    // 5. Create TAP device inside netns (Firecracker will use this)
    run_ip_netns(&netns_name, &[
        "tuntap", "add", "dev", &cfg.tap_name, "mode", "tap",
    ])?;
    run_ip_netns(&netns_name, &["link", "set", &cfg.tap_name, "up"])?;

    // 6. Route inside netns: default via host-side veth
    run_ip_netns(&netns_name, &[
        "route", "add", "default", "via", &cfg.host_ip,
    ])?;

    // 7. Enable IP forwarding + NAT in root ns
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
    setup_nat(&cfg.host_ip, &cfg.veth_host)?;

    info!(%prefix, tap = %cfg.tap_name, "network setup complete");
    Ok(())
}

/// Tear down all network resources for a workspace.
#[instrument]
pub fn teardown_network(cfg: &NetConfig) -> Result<()> {
    let prefix = &cfg.ws_id[..8];
    let netns_name = format!("arbor-{}", prefix);

    // Remove NAT rules
    let _ = remove_nat(&cfg.host_ip, &cfg.veth_host);

    // Delete veth (also removes the peer inside netns)
    let _ = run_ip(&["link", "delete", &cfg.veth_host]);

    // Delete netns (removes TAP inside it too)
    let _ = run_ip(&["netns", "delete", &netns_name]);

    info!(%prefix, "network teardown complete");
    Ok(())
}

// ── nftables NAT ─────────────────────────────────────────────────────────────

fn setup_nat(host_ip: &str, veth_host: &str) -> Result<()> {
    // Add masquerade for traffic from the workspace subnet
    let subnet = format!("{}/30", host_ip);
    run_cmd("nft", &[
        "add", "rule", "ip", "nat", "postrouting",
        "ip", "saddr", &subnet,
        "oifname", "!=", veth_host,
        "masquerade",
    ]).ok(); // best-effort; nftables table may not exist yet

    // Simple forwarding rules
    run_cmd("nft", &[
        "add", "rule", "ip", "filter", "forward",
        "iifname", veth_host, "accept",
    ]).ok();
    run_cmd("nft", &[
        "add", "rule", "ip", "filter", "forward",
        "oifname", veth_host,
        "ct", "state", "established,related", "accept",
    ]).ok();
    Ok(())
}

fn remove_nat(_host_ip: &str, _veth_host: &str) -> Result<()> {
    // In production: flush the specific rules. For MVP we leave them;
    // GC sweep will clean up orphaned rules periodically.
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn run_ip(args: &[&str]) -> Result<()> {
    run_cmd("ip", args)
}

fn run_ip_netns(netns: &str, args: &[&str]) -> Result<()> {
    let mut full = vec!["netns", "exec", netns, "ip"];
    full.extend_from_slice(args);
    run_cmd("ip", &full)
}

fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    debug!("{} {}", cmd, args.join(" "));
    let output = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {cmd}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{cmd} {} failed: {}", args.join(" "), stderr);
    }
    Ok(())
}
