#!/usr/bin/env bash
set -euo pipefail

# Blockmachine Miner Setup
# Sets up the gateway + a chain node (subtensor or ethereum) on this server.
# No Python or CLI required — just Docker.

REPO_URL="https://github.com/taostat/blockmachine-miner.git"
INSTALL_DIR="${BM_MINER_DIR:-/root/blockmachine-miner}"
MIN_COMPOSE_MAJOR=2
MIN_COMPOSE_MINOR=21
MIN_RAM_MB=15000
MIN_RAM_MB_ETH_LATEST=15000
MIN_RAM_MB_ETH_ARCHIVE=30000

# --- Helpers ---

info()  { echo "==> $*"; }
warn()  { echo "WARNING: $*" >&2; }
error() { echo "ERROR: $*" >&2; exit 1; }

check_command() {
  command -v "$1" >/dev/null || error "$1 is required but not found. $2"
}

install_docker() {
  info "Installing Docker..."
  curl -fsSL https://get.docker.com | sh
  systemctl enable --now docker >/dev/null 2>&1
  info "Docker installed"
}

install_git() {
  info "Installing git..."
  if command -v apt-get >/dev/null; then
    apt-get install -y -qq git >/dev/null 2>&1
  elif command -v yum >/dev/null; then
    yum install -y -q git >/dev/null 2>&1
  elif command -v dnf >/dev/null; then
    dnf install -y -q git >/dev/null 2>&1
  else
    error "Could not install git. Install it manually and re-run."
  fi
  info "Git installed"
}

clone_or_update_repo() {
  if [ -d "${INSTALL_DIR}/.git" ]; then
    info "Updating existing installation..."
    git -C "$INSTALL_DIR" pull --ff-only || warn "Could not update repo. Continuing with existing files."
  else
    info "Cloning blockmachine-miner..."
    git clone "$REPO_URL" "$INSTALL_DIR"
  fi
  cd "$INSTALL_DIR" || error "Could not enter ${INSTALL_DIR}"
}

check_port() {
  if ss -tlnp 2>/dev/null | grep -q ":$1 " ||
     netstat -tlnp 2>/dev/null | grep -q ":$1 "; then
    error "Port $1 is already in use. Stop the process and try again."
  fi
}

check_system() {
  local ram_mb
  ram_mb=$(awk '/MemTotal/ {printf "%d", $2/1024}' /proc/meminfo 2>/dev/null || sysctl -n hw.memsize 2>/dev/null | awk '{printf "%d", $1/1024/1024}' || echo "0")
  local arch
  arch=$(uname -m)
  if [ "$arch" != "x86_64" ]; then
    error "x86_64 architecture required (found ${arch})."
  fi

  if [ "$ram_mb" -gt 0 ] && [ "$ram_mb" -lt "$MIN_RAM_MB" ]; then
    error "At least ${MIN_RAM_MB}MB RAM required (found ${ram_mb}MB). Use a larger server."
  fi

  local cores
  cores=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo "0")
  if [ "$cores" -gt 0 ] && [ "$cores" -lt 4 ]; then
    error "At least 4 CPU cores required (found ${cores}). Use a larger server."
  fi

  if [ "$ram_mb" -gt 0 ] && [ "$cores" -gt 0 ]; then
    info "System: ${cores} cores, ${ram_mb}MB RAM"
  fi
}

# Warn (but don't error) when the chosen tier wants more RAM than the host has.
check_tier_resources() {
  local recommended_mb="$1" tier_label="$2"
  local ram_mb
  ram_mb=$(awk '/MemTotal/ {printf "%d", $2/1024}' /proc/meminfo 2>/dev/null || echo "0")
  if [ "$ram_mb" -gt 0 ] && [ "$ram_mb" -lt "$recommended_mb" ]; then
    warn "${tier_label} recommends ${recommended_mb}MB RAM; this host has ${ram_mb}MB. Sync may be slow or unstable."
  fi
}

check_compose_version() {
  local version
  version=$(docker compose version --short 2>/dev/null || docker compose version 2>/dev/null || echo "")
  version=$(echo "$version" | grep -oE '[0-9]+\.[0-9]+' | head -1)
  if [ -z "$version" ]; then
    error "docker compose not found. Install: https://docs.docker.com/compose/install/"
  fi
  local major minor
  major=$(echo "$version" | cut -d. -f1)
  minor=$(echo "$version" | cut -d. -f2)
  if [ "$major" -lt "$MIN_COMPOSE_MAJOR" ] ||
     { [ "$major" -eq "$MIN_COMPOSE_MAJOR" ] && [ "$minor" -lt "$MIN_COMPOSE_MINOR" ]; }; then
    error "docker compose >= ${MIN_COMPOSE_MAJOR}.${MIN_COMPOSE_MINOR} required (found $version)."
  fi
}

get_public_ip() {
  local url ip
  for url in https://ifconfig.me https://api.ipify.org https://icanhazip.com; do
    ip=$(curl -4 -sf --max-time 10 "$url" 2>/dev/null | tr -d '[:space:]') && [ -n "$ip" ] && echo "$ip" && return
  done
  for url in https://ifconfig.me https://api.ipify.org https://icanhazip.com; do
    ip=$(curl -6 -sf --max-time 10 "$url" 2>/dev/null | tr -d '[:space:]') && [ -n "$ip" ] && echo "$ip" && return
  done
  error "Could not determine public IP."
}

is_ipv4() {
  echo "$1" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$'
}

is_ip() {
  is_ipv4 "$1" || echo "$1" | grep -q ':'
}

generate_self_signed_cert() {
  local cn="$1" ssl_dir="$2"
  local san
  if is_ip "$cn"; then
    san="IP:${cn}"
  else
    san="DNS:${cn}"
  fi

  mkdir -p "$ssl_dir"

  if openssl req -x509 -nodes -days 3650 -newkey rsa:2048 \
       -keyout "${ssl_dir}/key.pem" -out "${ssl_dir}/cert.pem" \
       -subj "/CN=${cn}" -addext "subjectAltName=${san}" 2>/dev/null; then
    return
  fi

  warn "-addext not supported by this OpenSSL; generating CN-only cert."
  openssl req -x509 -nodes -days 3650 -newkey rsa:2048 \
    -keyout "${ssl_dir}/key.pem" -out "${ssl_dir}/cert.pem" \
    -subj "/CN=${cn}"
}

prompt_yn() {
  local prompt="$1" default="${2:-n}" answer
  if [ "$default" = "y" ]; then
    read -rp "$prompt [Y/n] " answer
    answer="${answer:-y}"
  else
    read -rp "$prompt [y/N] " answer
    answer="${answer:-n}"
  fi
  case "$answer" in
    [Yy]*) return 0 ;;
    *)     return 1 ;;
  esac
}

prompt_value() {
  local prompt="$1" default="$2" value
  read -rp "$prompt [$default] " value
  echo "${value:-$default}"
}

check_snapshot_disk_space() {
  local url="$1"
  local snapshot_bytes
  snapshot_bytes=$(curl -sI -L "$url" 2>/dev/null \
    | grep -i '^content-length:' | tail -1 \
    | tr -dc '0-9')
  [ -n "$snapshot_bytes" ] && [ "$snapshot_bytes" -gt 0 ] || return 0

  local required_bytes=$(( snapshot_bytes * 5 / 2 ))
  local check_dir="${INSTALL_DIR:-.}"
  if [ ! -d "$check_dir" ]; then
    check_dir="$(dirname "$check_dir")"
  fi
  local available_kb
  available_kb=$(df -k "$check_dir" 2>/dev/null | tail -1 | awk '{print $4}')
  [ -n "$available_kb" ] || return 0

  local available_bytes=$(( available_kb * 1024 ))
  [ "$available_bytes" -lt "$required_bytes" ] || return 0

  local required_gb=$(( required_bytes / 1073741824 ))
  local available_gb=$(( available_bytes / 1073741824 ))
  warn "Disk space may be insufficient for snapshot restore."
  echo "    Available: ${available_gb} GB"
  echo "    Required:  ~${required_gb} GB (2.5x snapshot for RocksDB extraction)"
  if ! prompt_yn "Continue anyway?"; then
    error "Aborting. Free up disk space and re-run."
  fi
}

write_env() {
  local env_file="$1" secret="$2" domain="${3:-}" chain_id="${4:-tao}" tier="${5:-}"
  local git_sha git_branch
  git_sha=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
  git_branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")

  cat > "$env_file" <<EOF
SECRET_V1=${secret}
SECRET_V2=
DOMAIN=${domain}
CHAIN=${chain_id}
SSL_CERT_PATH=/etc/nginx/ssl/cert.pem
SSL_KEY_PATH=/etc/nginx/ssl/key.pem
BM_MINER_GIT_SHA=${git_sha}
BM_MINER_GIT_BRANCH=${git_branch}
EOF

  if [ "$chain_id" = "eth" ]; then
    local el_client="reth"
    [ "$tier" = "archive" ] && el_client="erigon"
    # Erigon serves HTTP and WS on the same port; reth uses 8545/8546 split.
    local ws_port=8546
    [ "$tier" = "archive" ] && ws_port=8545
    cat >> "$env_file" <<EOF
ETH_TIER=${tier}
ETH_NETWORK=mainnet
EL_CLIENT=${el_client}
BACKEND_HTTP_PORT=8545
BACKEND_WS_PORT=${ws_port}
EOF
  else
    echo "BACKEND_PORT=9944" >> "$env_file"
  fi

  chmod 600 "$env_file"
}

wait_for_health() {
  local retries=60 i
  info "Waiting for gateway health check..."
  for i in $(seq 1 "$retries"); do
    if curl -sf --max-time 5 http://localhost/health >/dev/null 2>&1; then
      return 0
    fi
    sleep 5
    if [ $((i % 6)) -eq 0 ]; then
      echo "    Still waiting ($i/$retries)..."
    fi
  done
  return 1
}

rpc_call() {
  curl -sk --max-time 5 \
    -X POST -H 'Content-Type: application/json' \
    -H "Authorization: Bearer ${secret}" \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"$1\",\"params\":[],\"id\":1}" \
    https://localhost:443 2>/dev/null
}

# Convert an "0x..." hex string from JSON-RPC to a decimal integer. Empty/0 on failure.
hex_to_dec() {
  local hex="$1"
  hex="${hex#0x}"
  [ -n "$hex" ] && [ "$hex" != "null" ] || { echo 0; return; }
  printf '%d' "0x${hex}" 2>/dev/null || echo 0
}

wait_for_sync_tao() {
  info "Waiting for node to sync (lite nodes warp-sync in ~15 minutes)..."
  echo "    (Ctrl+C to skip — the node will continue syncing in the background)"
  echo ""

  local dots=0
  while true; do
    local health_result sync_result
    health_result=$(rpc_call "system_health") || { sleep 10; continue; }
    sync_result=$(rpc_call "system_syncState") || { sleep 10; continue; }

    local peers syncing current highest
    peers=$(echo "$health_result" | grep -o '"peers":[0-9]*' | grep -o '[0-9]*')
    syncing=$(echo "$health_result" | grep -o '"isSyncing":[a-z]*' | cut -d: -f2)
    current=$(echo "$sync_result" | grep -o '"currentBlock":[0-9]*' | grep -o '[0-9]*')
    highest=$(echo "$sync_result" | grep -o '"highestBlock":[0-9]*' | grep -o '[0-9]*')
    peers="${peers:-0}"

    if [ -z "$current" ] || [ -z "$highest" ] || [ "$highest" -eq 0 ]; then
      printf "\r    Connecting to peers...                                          "
      sleep 10
      continue
    fi

    local remaining=$(( highest - current ))

    if [ "$remaining" -le 5 ] && [ "$syncing" = "false" ]; then
      printf "\r    Block %s — synced! (%s peers)                                  \n" \
        "$current" "$peers"
      return 0
    fi

    if [ "$current" -le 1 ]; then
      dots=$(( (dots + 1) % 4 ))
      local indicator=""
      case $dots in
        0) indicator="   " ;;
        1) indicator=".  " ;;
        2) indicator=".. " ;;
        3) indicator="..." ;;
      esac
      printf "\r    Warp syncing%s (%s peers, target block %s)          " \
        "$indicator" "$peers" "$highest"
    elif [ "$syncing" = "true" ]; then
      local pct=$(( current * 100 / highest ))
      printf "\r    Syncing blocks %s / %s (%d%%) — %s peers            " \
        "$current" "$highest" "$pct" "$peers"
    else
      local pct=$(( current * 100 / highest ))
      printf "\r    Block %s / %s (%d%%) — %s peers                    " \
        "$current" "$highest" "$pct" "$peers"
    fi

    sleep 10
  done
}

# Ethereum sync waiter. Polls eth_blockNumber + eth_syncing + net_peerCount.
# eth_syncing returns `false` when fully synced, else an object with progress.
wait_for_sync_eth() {
  info "Waiting for node to sync..."
  echo "    Latest tier (reth --full) typically catches up to tip within hours via P2P;"
  echo "    archive tier (erigon) syncs from torrents/peers and can take a day or more."
  echo "    (Ctrl+C to skip — sync continues in the background)"
  echo ""

  while true; do
    local block_result sync_result peer_result
    block_result=$(rpc_call "eth_blockNumber") || { sleep 10; continue; }
    sync_result=$(rpc_call "eth_syncing") || { sleep 10; continue; }
    peer_result=$(rpc_call "net_peerCount") || { sleep 10; continue; }

    local block_hex peer_hex
    block_hex=$(echo "$block_result" | grep -o '"result":"0x[0-9a-fA-F]*"' | grep -o '0x[0-9a-fA-F]*' | head -1)
    peer_hex=$(echo "$peer_result" | grep -o '"result":"0x[0-9a-fA-F]*"' | grep -o '0x[0-9a-fA-F]*' | head -1)

    local current_block peers
    current_block=$(hex_to_dec "${block_hex:-0x0}")
    peers=$(hex_to_dec "${peer_hex:-0x0}")

    # eth_syncing is `false` when fully synced, else { currentBlock, highestBlock, ... }
    if echo "$sync_result" | grep -q '"result":false'; then
      if [ "$current_block" -gt 0 ]; then
        printf "\r    Block %s — synced! (%s peers)                                  \n" \
          "$current_block" "$peers"
        return 0
      fi
      printf "\r    Connecting to peers (%s connected)...                            " "$peers"
      sleep 10
      continue
    fi

    local highest_hex highest
    highest_hex=$(echo "$sync_result" | grep -o '"highestBlock":"0x[0-9a-fA-F]*"' | grep -o '0x[0-9a-fA-F]*' | head -1)
    highest=$(hex_to_dec "${highest_hex:-0x0}")

    if [ "$highest" -gt 0 ] && [ "$current_block" -gt 0 ]; then
      local pct=$(( current_block * 100 / highest ))
      printf "\r    Syncing blocks %s / %s (%d%%) — %s peers            " \
        "$current_block" "$highest" "$pct" "$peers"
    else
      printf "\r    Discovering peers (%s connected, target unknown)...               " "$peers"
    fi

    sleep 10
  done
}

wait_for_sync() {
  if [ "${chain:-tao}" = "eth" ]; then
    wait_for_sync_eth
  else
    wait_for_sync_tao
  fi
}

# shellcheck disable=SC2154
print_registration() {
  local tier_label
  if [ "$chain" = "eth" ]; then
    tier_label="$eth_tier"
  else
    tier_label="$([ "$archive" = true ] && echo "archive" || echo "lite")"
  fi

  echo ""
  echo "========================================"
  echo " Registration Details"
  echo "========================================"
  echo ""
  echo "  Endpoint: ${endpoint}"
  echo "  Chain:    ${chain} (${tier_label})"
  echo "  Alias:    ${alias}"
  echo "  Secret:   ${secret}"
  if [ "$use_certbot" = true ]; then
    echo "  TLS:      Let's Encrypt (auto-renewing)"
  fi
  echo ""
  echo "Register this node from your local machine:"
  echo ""
  echo "  ${bm_prefix} miner login"
  echo "  ${bm_prefix} miner add --endpoint '${endpoint}' --alias ${alias} --secret '${secret}' --price <usd-per-cu>"
  echo ""
  echo "Install the CLI (requires Python 3.10+):"
  echo "  pip install blockmachine"
  echo ""
}

# --- Main ---

main() {
  echo ""
  echo "Blockmachine Miner Setup"
  echo "========================"
  echo ""

  # Quick sanity checks (no installs yet)
  check_command curl "Install curl for network checks."
  check_command openssl "Install openssl to generate certificates."
  check_system

  # ── Interactive: gather all user input ────────────────────────────

  # Chain
  echo ""
  echo "Which blockchain will this miner serve?"
  echo "  tao - Bittensor subtensor (default)"
  echo "  eth - Ethereum mainnet (reth Latest tier or erigon Archive tier)"
  chain=$(prompt_value "Chain" "tao")
  chain=$(echo "$chain" | tr '[:upper:]' '[:lower:]')
  case "$chain" in
    tao|eth) ;;
    *) error "Unknown chain '${chain}'. Choose 'tao' or 'eth'." ;;
  esac

  # Network — testnet only applies to tao for now; eth is mainnet-only.
  network="mainnet"
  if [ "$chain" = "tao" ]; then
    if prompt_yn "Use testnet?"; then
      network="testnet"
    fi
  fi

  bm_prefix="bm"
  if [ "$network" = "testnet" ]; then
    bm_prefix="bm --testnet"
  fi

  # TLS / endpoint. Default URL scheme depends on chain — substrate uses WS for
  # JSON-RPC (wss://), Ethereum uses HTTP with optional Upgrade (https://).
  use_certbot=false
  domain=""
  local default_scheme="wss"
  [ "$chain" = "eth" ] && default_scheme="https"

  if prompt_yn "Do you have a domain name?"; then
    domain=$(prompt_value "Enter your domain name" "")
    domain=$(echo "$domain" | tr '[:upper:]' '[:lower:]' | xargs)
    [ -z "$domain" ] && error "Domain cannot be empty."

    if prompt_yn "Use auto-renewing Let's Encrypt certificate?" "y"; then
      use_certbot=true
    fi

    endpoint="${default_scheme}://${domain}"
  else
    public_ip=$(get_public_ip)
    info "Public IP: ${public_ip}"
    if is_ipv4 "$public_ip"; then
      endpoint="${default_scheme}://${public_ip}"
    else
      endpoint="${default_scheme}://[${public_ip}]"
    fi
    domain="$public_ip"
  fi

  # Node tier (chain-specific terminology).
  archive=false
  eth_tier="latest"
  if [ "$chain" = "eth" ]; then
    echo ""
    echo "Ethereum tier:"
    echo "  latest  - reth --full preset (~240 GB disk, ~34h eth_getProof window)"
    echo "  archive - erigon archive (~3.5 TB disk, full history back to genesis)"
    eth_tier=$(prompt_value "Tier" "latest")
    eth_tier=$(echo "$eth_tier" | tr '[:upper:]' '[:lower:]')
    case "$eth_tier" in
      latest)
        check_tier_resources "$MIN_RAM_MB_ETH_LATEST" "ETH Latest tier"
        ;;
      archive)
        check_tier_resources "$MIN_RAM_MB_ETH_ARCHIVE" "ETH Archive tier"
        echo ""
        echo "  Archive node uses erigon and requires ~3.5 TB of fast SSD/NVMe."
        echo "  Initial sync from torrents/peers takes a day or more."
        ;;
      *)
        error "Unknown tier '${eth_tier}'. Choose 'latest' or 'archive'."
        ;;
    esac
  else
    echo ""
    node_type=$(prompt_value "Node type: lite or archive?" "lite")
    case "$node_type" in
      [Aa]*) archive=true ;;
    esac

    if [ "$archive" = true ]; then
      echo ""
      echo "  Archive node uses RocksDB. The chain data is currently ~3.2 TB and growing."
      echo "  You will need at least 2x that (~6.5 TB) for snapshot extraction."
      echo "  May take 6-12 hours to sync without a snapshot."
    fi
  fi

  # Snapshot (tao archive nodes only — eth archive uses erigon's built-in
  # torrent-based snapshot fetch, so no URL prompt is needed).
  snapshot_url=""
  snapshot_stream=false
  if [ "$chain" = "tao" ] && [ "$archive" = true ]; then
    echo ""
    echo "Speed up sync by restoring a snapshot."
    echo "Get a snapshot URL: ${bm_prefix} miner snapshot --type archive"
    echo ""
    snapshot_url=$(prompt_value "Snapshot URL (or press Enter to skip)" "")
  fi

  if [ -n "$snapshot_url" ]; then
    echo ""
    echo "  Restore method:"
    echo "    1) Download first — requires ~2x disk, supports resume if connection drops"
    echo "    2) Stream directly — requires ~1x disk, must restart from scratch if interrupted"
    restore_method=$(prompt_value "Choose restore method" "1")
    case "$restore_method" in
      2) snapshot_stream=true ;;
    esac

    if [ "$snapshot_stream" = false ]; then
      check_snapshot_disk_space "$snapshot_url"
    fi
  fi

  # Alias — default prefix reflects the chain so multi-chain operators can tell
  # nodes apart at a glance in the dashboard.
  echo ""
  default_alias="${chain}-$(echo "$domain" | tr '.' '-')"
  alias=$(prompt_value "Node alias (friendly name)" "$default_alias")

  # Secret
  echo ""
  default_secret=$(openssl rand -base64 32 | tr -d '=/+' | head -c 43)
  # Re-use existing secret if re-running
  if [ -f "${INSTALL_DIR}/.env" ]; then
    existing_secret=$(grep -oP '(?<=SECRET_V1=).+' "${INSTALL_DIR}/.env" 2>/dev/null || echo "")
    if [ -n "$existing_secret" ]; then
      default_secret="$existing_secret"
    fi
  fi
  secret=$(prompt_value "Bearer token secret" "$default_secret")

  echo ""
  info "Configuration complete. Setting up infrastructure..."
  echo ""

  # ── Non-interactive: install, configure, start ────────────────────

  if ! command -v git >/dev/null; then
    install_git
  fi

  if ! command -v docker >/dev/null; then
    install_docker
  fi
  check_compose_version

  # Stop existing services if re-running (so port checks pass). Try every
  # known compose file so switching chains during a re-install also works.
  if [ -d "${INSTALL_DIR}" ]; then
    info "Stopping any existing services for re-install..."
    for f in docker-compose.yml docker-compose.eth.yml docker-compose.eth-archive.yml; do
      if [ -f "${INSTALL_DIR}/${f}" ]; then
        docker compose -f "${INSTALL_DIR}/${f}" down 2>/dev/null || true
      fi
    done
  fi
  check_port 80
  check_port 443

  # P2P port differs per chain — substrate uses 30333, ethereum uses 30303.
  p2p_port=30333
  [ "$chain" = "eth" ] && p2p_port=30303

  # Open firewall ports if ufw is active
  if command -v ufw >/dev/null && ufw status | grep -q "active"; then
    info "Opening firewall ports (80, 443, ${p2p_port})..."
    ufw allow 80/tcp
    ufw allow 443/tcp
    ufw allow "${p2p_port}/tcp"
    ufw allow "${p2p_port}/udp"
    if [ "$chain" = "eth" ] && [ "$eth_tier" = "latest" ]; then
      # Lighthouse beacon P2P (TCP+UDP on 9000, QUIC discovery UDP on 9001).
      ufw allow 9000/tcp
      ufw allow 9000/udp
      ufw allow 9001/udp
    fi
  fi

  # Clone or update the repo
  clone_or_update_repo

  # TLS certificates
  ssl_dir="./ssl"

  if [ "$use_certbot" = true ]; then
    info "Generating temporary certificate (replaced by Let's Encrypt on startup)..."
    generate_self_signed_cert "$domain" "$ssl_dir"
  elif is_ip "$domain"; then
    info "Generating self-signed certificate..."
    generate_self_signed_cert "$domain" "$ssl_dir"
    info "Self-signed certificate created"
  elif [ ! -f "${ssl_dir}/cert.pem" ] || [ ! -f "${ssl_dir}/key.pem" ]; then
    echo ""
    echo "No certificates found. Place your certificates at:"
    echo "  ${INSTALL_DIR}/ssl/cert.pem"
    echo "  ${INSTALL_DIR}/ssl/key.pem"
    echo ""
    error "Certificates required for domain without Let's Encrypt. Re-run and choose Let's Encrypt, or provide certs."
  fi

  # Write .env (chain-aware: emits ETH_TIER, EL_CLIENT, BACKEND_HTTP/WS_PORT for eth)
  if [ "$chain" = "eth" ]; then
    write_env ".env" "$secret" "$domain" "eth" "$eth_tier"
  else
    write_env ".env" "$secret" "$domain" "tao"
  fi
  info ".env written"

  # Show registration details now (safe to Ctrl+C during sync)
  print_registration

  # Restore snapshot if provided
  if [ -n "$snapshot_url" ]; then
    # Detect compression from URL
    case "$snapshot_url" in
      *.tar.lz4*)
        decompress_cmd="lz4 -dc"
        decompress_pkg="lz4"
        snapshot_file="snapshot.tar.lz4"
        ;;
      *)
        decompress_cmd="zstd -d --stdout"
        decompress_pkg="zstd"
        snapshot_file="snapshot.tar.zst"
        ;;
    esac

    if ! command -v "${decompress_cmd%% *}" >/dev/null; then
      info "Installing ${decompress_pkg}..."
      apt-get install -y -qq "$decompress_pkg" >/dev/null 2>&1 ||
        error "Failed to install ${decompress_pkg}. Install manually: apt install ${decompress_pkg}"
    fi

    volume_name="blockmachine-miner_node_data"
    restore_cmd="docker run --rm -i -v ${volume_name}:/data alpine tar xf - -C /data"

    info "Creating data volume..."
    docker volume create "$volume_name" >/dev/null 2>&1 || true

    if [ "$snapshot_stream" = true ]; then
      info "Streaming snapshot directly..."
      if curl -fL "$snapshot_url" | $decompress_cmd | $restore_cmd; then
        info "Snapshot restored"
      else
        error "Stream restore failed. Re-run the installer to try again."
      fi
    else
      if [ -f "$snapshot_file" ] && [ -s "$snapshot_file" ]; then
        info "Snapshot file found, resuming/skipping download"
      fi

      info "Downloading snapshot..."
      curl -fL -C - "$snapshot_url" -o "$snapshot_file" ||
        error "Download failed. Re-run the installer to resume where you left off."

      info "Restoring snapshot..."
      if $decompress_cmd "$snapshot_file" | $restore_cmd; then
        info "Snapshot restored (keeping file until node is healthy)"
      else
        warn "Snapshot restore failed. The downloaded file has been kept."
        echo "    Re-run the installer to retry, or restore manually:"
        echo "    $decompress_cmd $snapshot_file | $restore_cmd"
        error "Snapshot restore failed"
      fi
    fi
  fi

  # Start services. Compose file selection:
  #   tao + lite     → docker-compose.yml
  #   tao + archive  → docker-compose.yml + docker-compose.archive.yml
  #   eth + latest   → docker-compose.eth.yml
  #   eth + archive  → docker-compose.eth-archive.yml
  #   + tls (any)    → adds docker-compose.tls.yml
  echo ""
  info "Starting services..."
  if [ "$chain" = "eth" ]; then
    if [ "$eth_tier" = "archive" ]; then
      compose_cmd="docker compose -f docker-compose.eth-archive.yml"
    else
      compose_cmd="docker compose -f docker-compose.eth.yml"
    fi
  else
    compose_cmd="docker compose -f docker-compose.yml"
    if [ "$archive" = true ]; then
      compose_cmd="$compose_cmd -f docker-compose.archive.yml"
    fi
  fi
  if [ "$use_certbot" = true ]; then
    compose_cmd="$compose_cmd -f docker-compose.tls.yml"
  fi
  $compose_cmd up -d

  if wait_for_health; then
    info "Gateway is healthy"
    if [ -n "${snapshot_file:-}" ] && [ -f "$snapshot_file" ]; then
      rm -f "$snapshot_file"
      info "Snapshot file removed"
    fi
  else
    warn "Gateway not yet healthy. The subtensor node may still be syncing."
    echo "    Check status: docker compose logs -f"
    if [ -n "${snapshot_file:-}" ] && [ -f "$snapshot_file" ]; then
      warn "Keeping snapshot file until node is confirmed healthy."
      echo "    Remove manually once healthy: rm $snapshot_file"
    fi
  fi

  if [ "$use_certbot" = true ]; then
    echo ""
    info "Certbot is obtaining your Let's Encrypt certificate..."
    echo "    Check progress: docker compose -f docker-compose.yml -f docker-compose.tls.yml logs certbot"
    echo "    Certificates auto-renew every 12 hours."
  fi

  echo ""
  wait_for_sync || true

  # Done
  echo ""
  echo "========================================"
  echo " Miner is running!"
  echo "========================================"
  echo ""
  echo "Manage this node:"
  echo "  Logs:    docker compose logs -f"
  echo "  Update:  cd ${INSTALL_DIR} && git pull && docker compose pull && docker compose up -d"
  echo "  Health:  curl -sSf http://localhost/health"
  echo ""
  if ! command -v ufw >/dev/null || ! ufw status | grep -q "active"; then
    echo "Firewall:"
    echo "  Consider enabling a firewall if you haven't already:"
    if [ "$chain" = "eth" ] && [ "$eth_tier" = "latest" ]; then
      echo "    ufw allow 22/tcp && ufw allow 80/tcp && ufw allow 443/tcp \\"
      echo "      && ufw allow 30303 && ufw allow 9000 && ufw allow 9001/udp && ufw enable"
    elif [ "$chain" = "eth" ]; then
      echo "    ufw allow 22/tcp && ufw allow 80/tcp && ufw allow 443/tcp && ufw allow 30303 && ufw enable"
    else
      echo "    ufw allow 22/tcp && ufw allow 80/tcp && ufw allow 443/tcp && ufw allow 30333/tcp && ufw enable"
    fi
    echo ""
  fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main
fi
