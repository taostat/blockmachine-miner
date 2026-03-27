#!/usr/bin/env bash
set -euo pipefail

# Blockmachine Miner Setup
# Sets up the gateway + subtensor node on this server.
# No Python or CLI required — just Docker.

# Source lib.sh — handle both local clone and bash <(curl ...) paths
LIB_URL="https://blockmachine.io/miner/lib.sh"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd 2>/dev/null)" || SCRIPT_DIR=""
if [ -n "$SCRIPT_DIR" ] && [ -f "${SCRIPT_DIR}/lib.sh" ]; then
  # shellcheck source=lib.sh
  source "${SCRIPT_DIR}/lib.sh"
elif [ -f "${INSTALL_DIR:-/root/blockmachine-miner}/lib.sh" ]; then
  # shellcheck source=lib.sh
  source "${INSTALL_DIR:-/root/blockmachine-miner}/lib.sh"
else
  # Piped via curl — fetch lib.sh from the same origin as install.sh
  LIB_TMP=$(mktemp)
  curl -fsSL "$LIB_URL" -o "$LIB_TMP" ||
    { echo "ERROR: Could not download lib.sh from ${LIB_URL}" >&2; exit 1; }
  # shellcheck source=lib.sh
  source "$LIB_TMP"
  rm -f "$LIB_TMP"
fi

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
  snapshot_stream=false
  if [ "$archive" = true ]; then
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
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main
fi
