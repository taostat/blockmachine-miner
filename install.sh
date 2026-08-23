#!/usr/bin/env bash
set -euo pipefail

# Blockmachine Miner Setup
# Sets up the gateway that fronts YOUR chain node on this server.
# You bring the node: provisioning, syncing and disk are your responsibility.
# No Python or CLI required here — just Docker.

REPO_URL="https://github.com/taostat/blockmachine-miner.git"
INSTALL_DIR="${BM_MINER_DIR:-/root/blockmachine-miner}"
MIN_COMPOSE_MAJOR=2
MIN_COMPOSE_MINOR=21
MIN_RAM_MB=2000

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
  local port="$1" proto="${2:-tcp}"
  local flags="-tlnp"
  [ "$proto" = "udp" ] && flags="-ulnp"
  if ss "$flags" 2>/dev/null | grep -q ":${port} " ||
     netstat "$flags" 2>/dev/null | grep -q ":${port} "; then
    error "${proto^^} port ${port} is already in use. Stop the process and try again."
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

write_env() {
  local env_file="$1" secret="$2" domain="${3:-}" chain_id="${4:-tao}"
  : "${gateway_template:=tao}"
  : "${backend_host:=host.docker.internal}"
  : "${backend_port:=9944}"
  : "${backend_http_port:=8545}"
  : "${backend_ws_port:=8546}"
  local git_sha git_branch
  git_sha=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
  git_branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")

  cat > "$env_file" <<EOF
SECRET_V1=${secret}
SECRET_V2=
DOMAIN=${domain}
CHAIN=${chain_id}
GATEWAY_TEMPLATE=${gateway_template}
BACKEND_HOST=${backend_host}
SSL_CERT_PATH=/etc/nginx/ssl/cert.pem
SSL_KEY_PATH=/etc/nginx/ssl/key.pem
BM_MINER_GIT_SHA=${git_sha}
BM_MINER_GIT_BRANCH=${git_branch}
EOF

  if [ "$gateway_template" = "eth" ]; then
    cat >> "$env_file" <<EOF
BACKEND_HTTP_PORT=${backend_http_port}
BACKEND_WS_PORT=${backend_ws_port}
EOF
  else
    echo "BACKEND_PORT=${backend_port}" >> "$env_file"
  fi

  chmod 600 "$env_file"
}

print_registration() {
  echo ""
  echo "========================================"
  echo " Registration Details"
  echo "========================================"
  echo ""
  echo "  Endpoint: ${endpoint}"
  echo "  Chain:    ${chain}"
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
  echo "Blockmachine Gateway Setup"
  echo "=========================="
  echo ""
  echo "This installs the gateway that fronts your node. You bring the node:"
  echo "provisioning, syncing and disk sizing are your responsibility, and your"
  echo "node must meet the eligibility requirements for its chain and type."
  echo ""

  # Quick sanity checks (no installs yet)
  check_command curl "Install curl for network checks."
  check_command openssl "Install openssl to generate certificates."
  check_system

  # ── Interactive: gather all user input ────────────────────────────

  # Chain — the canonical lower-case chain code.
  echo ""
  chain=$(prompt_value "Chain code (e.g. tao, eth, bsc, base, ...)" "tao")
  chain="${chain,,}"

  # Gateway template: substrate speaks WS on one port; EVM chains split
  # HTTP and WS across two.
  gateway_template="eth"
  [ "$chain" = "tao" ] && gateway_template="tao"

  # Network — testnet only applies to tao for now.
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

  # Where is your node? The gateway proxies to it.
  echo ""
  echo "Where does your node listen? (host.docker.internal reaches this host;"
  echo "use an IP or hostname for a node on another machine)"
  backend_host=$(prompt_value "Node host" "host.docker.internal")
  backend_port=""
  backend_http_port=""
  backend_ws_port=""
  if [ "$gateway_template" = "tao" ]; then
    backend_port=$(prompt_value "Node RPC/WS port" "9944")
  else
    backend_http_port=$(prompt_value "Node HTTP JSON-RPC port" "8545")
    backend_ws_port=$(prompt_value "Node WebSocket port" "8546")
  fi

  # TLS / endpoint. Substrate uses WS for JSON-RPC (wss://), EVM chains use
  # HTTP with optional Upgrade (https://).
  use_certbot=false
  domain=""
  local default_scheme="wss"
  [ "$gateway_template" = "eth" ] && default_scheme="https"

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

  # Alias — default prefix reflects the chain so multi-chain operators can
  # tell nodes apart at a glance.
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
  info "Configuration complete. Setting up the gateway..."
  echo ""

  # ── Non-interactive: install, configure, start ────────────────────

  if ! command -v git >/dev/null; then
    install_git
  fi

  if ! command -v docker >/dev/null; then
    install_docker
  fi
  check_compose_version

  # Stop existing services if re-running (so port checks pass).
  if [ -d "${INSTALL_DIR}" ] && [ -f "${INSTALL_DIR}/docker-compose.yml" ]; then
    info "Stopping any existing services for re-install..."
    docker compose -f "${INSTALL_DIR}/docker-compose.yml" down --remove-orphans 2>/dev/null || true
  fi
  check_port 80
  check_port 443

  # Open firewall ports if ufw is active. Your node's own ports (p2p etc.)
  # are yours to manage.
  if command -v ufw >/dev/null && ufw status | grep -q "active"; then
    info "Opening firewall ports (80, 443)..."
    ufw allow 80/tcp
    ufw allow 443/tcp
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
  write_env ".env" "$secret" "$domain" "$chain"
  info ".env written"

  # Show registration details
  print_registration

  # Start the gateway stack
  echo ""
  info "Starting the gateway..."
  compose_cmd="docker compose -f docker-compose.yml"
  if [ "$use_certbot" = true ]; then
    compose_cmd="$compose_cmd -f docker-compose.tls.yml"
  fi
  $compose_cmd up -d

  if wait_for_health; then
    info "Gateway is healthy"
  else
    warn "Gateway not yet healthy. Check that your node is reachable at the host/port you gave."
    echo "    Check status: ${compose_cmd} logs -f"
  fi

  if [ "$use_certbot" = true ]; then
    echo ""
    info "Certbot is obtaining your Let's Encrypt certificate..."
    echo "    Check progress: ${compose_cmd} logs certbot"
    echo "    Certificates auto-renew every 12 hours."
  fi

  # Done
  echo ""
  echo "========================================"
  echo " Gateway is running!"
  echo "========================================"
  echo ""
  echo "Manage it:"
  echo "  Logs:    ${compose_cmd} logs -f"
  echo "  Update:  cd ${INSTALL_DIR} && git pull && ${compose_cmd} pull && ${compose_cmd} up -d"
  echo "  Health:  curl -sSf http://localhost/health"
  echo ""
  if ! command -v ufw >/dev/null || ! ufw status | grep -q "active"; then
    echo "Firewall:"
    echo "  Consider enabling a firewall if you haven't already:"
    echo "    ufw allow 22/tcp && ufw allow 80/tcp && ufw allow 443/tcp && ufw enable"
    echo "  Your node's own ports (p2p etc.) are yours to manage."
    echo ""
  fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main
fi
