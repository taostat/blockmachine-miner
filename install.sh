#!/usr/bin/env bash
set -euo pipefail

# Blockmachine Miner Setup
# Sets up the gateway + subtensor node on this server.
# No Python or CLI required — just Docker.

REPO_URL="https://github.com/taostat/blockmachine-miner.git"
INSTALL_DIR="${BM_MINER_DIR:-/root/blockmachine-miner}"
MIN_COMPOSE_MAJOR=2
MIN_COMPOSE_MINOR=21
MIN_RAM_MB=15000

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
  cd "$INSTALL_DIR"
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
    error "x86_64 architecture required (found ${arch}). Subtensor does not support ARM."
  fi

  if [ "$ram_mb" -gt 0 ] && [ "$ram_mb" -lt "$MIN_RAM_MB" ]; then
    error "At least ${MIN_RAM_MB}MB RAM required for warp sync (found ${ram_mb}MB). Use a larger server."
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
  # Try IPv4 first (simpler, no bracket issues)
  for url in https://ifconfig.me https://api.ipify.org https://icanhazip.com; do
    ip=$(curl -4 -sf --max-time 10 "$url" 2>/dev/null | tr -d '[:space:]') && [ -n "$ip" ] && echo "$ip" && return
  done
  # Fall back to IPv6
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

write_env() {
  local env_file="$1" secret="$2" domain="${3:-}"
  cat > "$env_file" <<EOF
SECRET_V1=${secret}
SECRET_V2=
DOMAIN=${domain}
BACKEND_PORT=9944
SSL_CERT_PATH=/etc/nginx/ssl/cert.pem
SSL_KEY_PATH=/etc/nginx/ssl/key.pem
EOF
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

wait_for_sync() {
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

    # Stage 1: Warp sync — downloading state, currentBlock stuck at 0
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
    # Stage 2: Block sync — warp done, catching up remaining blocks
    elif [ "$syncing" = "true" ]; then
      local pct=$(( current * 100 / highest ))
      printf "\r    Syncing blocks %s / %s (%d%%) — %s peers            " \
        "$current" "$highest" "$pct" "$peers"
    # Shouldn't get here (exit condition above), but show progress anyway
    else
      local pct=$(( current * 100 / highest ))
      printf "\r    Block %s / %s (%d%%) — %s peers                    " \
        "$current" "$highest" "$pct" "$peers"
    fi

    sleep 10
  done
}

print_registration() {
  echo ""
  echo "========================================"
  echo " Registration Details"
  echo "========================================"
  echo ""
  echo "  Endpoint: ${endpoint}"
  echo "  Chain:    tao ($([ "$archive" = true ] && echo "archive" || echo "lite"))"
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

echo ""
echo "Blockmachine Miner Setup"
echo "========================"
echo ""

# Quick sanity checks (no installs yet)
check_command curl "Install curl for network checks."
check_command openssl "Install openssl to generate certificates."
check_system

# ── Interactive: gather all user input ────────────────────────────

# Network
network="mainnet"
if prompt_yn "Use testnet?"; then
  network="testnet"
fi

bm_prefix="bm"
if [ "$network" = "testnet" ]; then
  bm_prefix="bm --testnet"
fi

# TLS / endpoint
use_certbot=false
domain=""

if prompt_yn "Do you have a domain name?"; then
  domain=$(prompt_value "Enter your domain name" "")
  domain=$(echo "$domain" | tr '[:upper:]' '[:lower:]' | xargs)
  [ -z "$domain" ] && error "Domain cannot be empty."

  if prompt_yn "Use auto-renewing Let's Encrypt certificate?" "y"; then
    use_certbot=true
  fi

  endpoint="wss://${domain}"
else
  public_ip=$(get_public_ip)
  info "Public IP: ${public_ip}"
  if is_ipv4 "$public_ip"; then
    endpoint="wss://${public_ip}"
  else
    endpoint="wss://[${public_ip}]"
  fi
  domain="$public_ip"
fi

# Node type
echo ""
node_type=$(prompt_value "Node type: lite or archive?" "lite")
archive=false
case "$node_type" in
  [Aa]*) archive=true ;;
esac

if [ "$archive" = true ]; then
  echo ""
  echo "  Archive node uses RocksDB. The chain data is currently ~3.2 TB and growing."
  echo "  You will need at least 2x that (~6.5 TB) for snapshot extraction."
  echo "  May take 6-12 hours to sync without a snapshot."
fi

# Snapshot (archive nodes only)
snapshot_url=""
if [ "$archive" = true ]; then
  echo ""
  echo "Speed up sync by restoring a snapshot."
  echo "Get a snapshot URL: ${bm_prefix} miner snapshot --type archive"
  echo ""
  snapshot_url=$(prompt_value "Snapshot URL (or press Enter to skip)" "")

  if [ -n "$snapshot_url" ]; then
    # Check disk space: archive RocksDB needs ~2.5x the compressed
    # snapshot size for download + extraction + compaction headroom
    snapshot_bytes=$(curl -sI -L "$snapshot_url" 2>/dev/null \
      | grep -i '^content-length:' | tail -1 \
      | tr -dc '0-9')
    if [ -n "$snapshot_bytes" ] && [ "$snapshot_bytes" -gt 0 ]; then
      required_bytes=$(( snapshot_bytes * 5 / 2 ))
      available_bytes=$(df --output=avail -B1 . 2>/dev/null \
        | tail -1 | tr -dc '0-9')
      if [ -n "$available_bytes" ] && \
         [ "$available_bytes" -lt "$required_bytes" ]; then
        required_gb=$(( required_bytes / 1073741824 ))
        available_gb=$(( available_bytes / 1073741824 ))
        warn "Disk space may be insufficient for snapshot restore."
        echo "    Available: ${available_gb} GB"
        echo "    Required:  ~${required_gb} GB (2.5x snapshot for RocksDB extraction)"
        if ! prompt_yn "Continue anyway?"; then
          error "Aborting. Free up disk space and re-run."
        fi
      fi
    fi
  fi
fi

# Alias
echo ""
default_alias="tao-$(echo "$domain" | tr '.' '-')"
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

# Stop existing services if re-running (so port checks pass)
if [ -f "${INSTALL_DIR}/docker-compose.yml" ]; then
  info "Stopping existing services for re-install..."
  docker compose -f "${INSTALL_DIR}/docker-compose.yml" down 2>/dev/null || true
fi
check_port 80
check_port 443

# Open firewall ports if ufw is active
if command -v ufw >/dev/null && ufw status | grep -q "active"; then
  info "Opening firewall ports (80, 443, 30333)..."
  ufw allow 80/tcp
  ufw allow 443/tcp
  ufw allow 30333/tcp
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

# Write .env
write_env ".env" "$secret" "$domain"
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

  if [ -f "$snapshot_file" ] && [ -s "$snapshot_file" ]; then
    info "Snapshot file found, resuming/skipping download"
  fi

  info "Downloading snapshot (this will take a while)..."
  curl -fL -C - "$snapshot_url" -o "$snapshot_file" ||
    error "Download failed. Re-run the installer to resume where you left off."

  info "Creating data volume and restoring snapshot..."
  docker volume create blockmachine-miner_node_data >/dev/null 2>&1 || true
  if $decompress_cmd "$snapshot_file" | \
    docker run --rm -i -v blockmachine-miner_node_data:/data alpine tar xf - -C /data; then
    info "Snapshot restored (keeping file until node is healthy)"
  else
    warn "Snapshot restore failed. The downloaded file has been kept."
    echo "    Re-run the installer to retry, or restore manually:"
    echo "    $decompress_cmd $snapshot_file | docker run --rm -i -v blockmachine-miner_node_data:/data alpine tar xf - -C /data"
    error "Snapshot restore failed"
  fi
fi

# Start services
echo ""
info "Starting services..."
compose_cmd="docker compose -f docker-compose.yml"
if [ "$archive" = true ]; then
  compose_cmd="$compose_cmd -f docker-compose.archive.yml"
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
  echo "    ufw allow 22/tcp && ufw allow 80/tcp && ufw allow 443/tcp && ufw allow 30333/tcp && ufw enable"
  echo ""
fi
