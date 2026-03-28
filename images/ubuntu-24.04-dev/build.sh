#!/usr/bin/env bash
# Build the Ubuntu 24.04 base rootfs for Arbor workspace VMs.
#
# Prerequisites on the build host:
#   apt install -y debootstrap qemu-utils e2fsprogs
#   cargo build --release --target x86_64-unknown-linux-musl -p arbor-guest-agent
#
# Output:
#   ./output/ubuntu-24.04-dev-v1/rootfs.base.raw   (4G sparse ext4 image)
#   ./output/ubuntu-24.04-dev-v1/metadata.json
set -euo pipefail

IMAGE_ID="ubuntu-24.04-dev-v1"
OUTPUT_DIR="output/${IMAGE_ID}"
ROOTFS_RAW="${OUTPUT_DIR}/rootfs.base.raw"
MOUNT_DIR="/tmp/arbor-rootfs-build"
IMAGE_SIZE="4G"
ARCH="amd64"
GUEST_AGENT_BIN="../../target/x86_64-unknown-linux-musl/release/arbor-guest-agent"

echo "==> Building Arbor guest image: ${IMAGE_ID}"
mkdir -p "${OUTPUT_DIR}" "${MOUNT_DIR}"

# ── 1. Create sparse raw disk ─────────────────────────────────────────────────
echo "==> Creating ${IMAGE_SIZE} sparse image"
truncate -s "${IMAGE_SIZE}" "${ROOTFS_RAW}"
mkfs.ext4 -F -L arbor-root "${ROOTFS_RAW}"

# ── 2. Mount image ────────────────────────────────────────────────────────────
echo "==> Mounting image"
mount -o loop "${ROOTFS_RAW}" "${MOUNT_DIR}"
trap "umount -l ${MOUNT_DIR} 2>/dev/null || true" EXIT

# ── 3. Bootstrap Ubuntu 24.04 ─────────────────────────────────────────────────
echo "==> Running debootstrap (Ubuntu 24.04 noble)"
debootstrap \
  --arch="${ARCH}" \
  --include="systemd,systemd-sysv,dbus,curl,wget,ca-certificates,git,\
sudo,openssh-client,procps,iproute2,iputils-ping,\
build-essential,pkg-config,\
python3,python3-pip,python3-venv,\
nodejs,npm,\
golang-go,\
rustup" \
  noble \
  "${MOUNT_DIR}" \
  http://archive.ubuntu.com/ubuntu/

# ── 4. Configure system ───────────────────────────────────────────────────────
echo "==> Configuring system"

# Hostname
echo "arbor-vm" > "${MOUNT_DIR}/etc/hostname"

# fstab
cat > "${MOUNT_DIR}/etc/fstab" <<'EOF'
/dev/vda / ext4 defaults,noatime 0 1
tmpfs /tmp tmpfs defaults,size=512m 0 0
EOF

# /etc/resolv.conf
cat > "${MOUNT_DIR}/etc/resolv.conf" <<'EOF'
nameserver 8.8.8.8
nameserver 8.8.4.4
EOF

# Network — eth0 gets IP via cloud-init/dhcp in real deployments.
# Firecracker sets up TAP; guest uses static IP passed via kernel cmdline or mmds.
cat > "${MOUNT_DIR}/etc/systemd/network/20-eth0.network" <<'EOF'
[Match]
Name=eth0

[Network]
DHCP=yes
EOF

# ── 5. Install Docker ─────────────────────────────────────────────────────────
echo "==> Installing Docker inside rootfs"
chroot "${MOUNT_DIR}" bash -c "
  curl -fsSL https://download.docker.com/linux/ubuntu/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
  echo 'deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu noble stable' \
    > /etc/apt/sources.list.d/docker.list
  apt-get update -qq
  apt-get install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin
  systemctl enable docker
"

# ── 6. Install guest-agent ────────────────────────────────────────────────────
echo "==> Installing arbor-guest-agent"
if [[ ! -f "${GUEST_AGENT_BIN}" ]]; then
  echo "WARN: ${GUEST_AGENT_BIN} not found — skipping (build with cargo build --release --target x86_64-unknown-linux-musl -p arbor-guest-agent)"
else
  install -m 755 "${GUEST_AGENT_BIN}" "${MOUNT_DIR}/usr/local/bin/arbor-guest-agent"
fi

# Guest-agent systemd service
cat > "${MOUNT_DIR}/etc/systemd/system/arbor-guest-agent.service" <<'EOF'
[Unit]
Description=Arbor Guest Agent
After=network.target
Wants=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/arbor-guest-agent
Restart=always
RestartSec=1
StandardOutput=journal
StandardError=journal
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

chroot "${MOUNT_DIR}" systemctl enable arbor-guest-agent

# ── 7. arbor-reseal service (runs after snapshot restore) ─────────────────────
cat > "${MOUNT_DIR}/etc/systemd/system/arbor-reseal.service" <<'EOF'
[Unit]
Description=Arbor Post-Restore Reseal
DefaultDependencies=no
Before=network.target
After=systemd-journald.socket

[Service]
Type=oneshot
ExecStart=/usr/local/bin/arbor-reseal
RemainAfterExit=no

[Install]
WantedBy=basic.target
EOF

# Reseal script — signals guest-agent that system came up after restore
cat > "${MOUNT_DIR}/usr/local/bin/arbor-reseal" <<'EOF'
#!/bin/bash
# Notify host that guest has completed post-restore boot.
# The guest-agent will handle the actual token/entropy refresh.
logger -t arbor-reseal "post-restore boot complete"
EOF
chmod +x "${MOUNT_DIR}/usr/local/bin/arbor-reseal"

# ── 8. Create /workspace directory ───────────────────────────────────────────
mkdir -p "${MOUNT_DIR}/workspace"
chmod 777 "${MOUNT_DIR}/workspace"

# ── 9. Finalize ───────────────────────────────────────────────────────────────
echo "==> Cleaning up apt cache"
chroot "${MOUNT_DIR}" apt-get clean
rm -rf "${MOUNT_DIR}/var/lib/apt/lists/"*

echo "==> Unmounting"
umount -l "${MOUNT_DIR}"
trap - EXIT

# Compute hash of rootfs
HASH=$(sha256sum "${ROOTFS_RAW}" | awk '{print $1}')

# Write metadata
cat > "${OUTPUT_DIR}/metadata.json" <<EOF
{
  "image_id": "${IMAGE_ID}",
  "arch": "x86_64",
  "base_distro": "ubuntu-24.04-noble",
  "rootfs_hash": "sha256:${HASH}",
  "cpu_template": "T2",
  "block_layout_version": 1,
  "created_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

echo ""
echo "✓ Image built: ${ROOTFS_RAW}"
echo "  SHA256: ${HASH}"
echo "  Size: $(du -h ${ROOTFS_RAW} | cut -f1)"
