#!/usr/bin/env bash
# Shared functions for install.sh — sourceable without side effects.

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
  local available_kb
  local check_dir="${INSTALL_DIR:-.}"
  # Fall back to parent if INSTALL_DIR doesn't exist yet (fresh install)
  if [ ! -d "$check_dir" ]; then
    check_dir="$(dirname "$check_dir")"
  fi
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

# Variables set by main() in install.sh
# shellcheck disable=SC2154
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
