# shellcheck shell=bash
# Phala Cloud pre-launch script for the BlockMachine TEE proxy-miner CVM.
#
# This is upstream Phala Cloud's auto-injected pre-launch script v0.0.14
# with TWO deliberate functional changes from stock:
#
#   1. HARDENING (BM TEE threat model — the miner operator is untrusted):
#      the stock script's operator root/SSH provisioning is REMOVED. Stock
#      sets a root password from $DSTACK_ROOT_PASSWORD, installs SSH
#      authorized_keys from $DSTACK_ROOT_PUBLIC_KEY / $DSTACK_AUTHORIZED_KEYS,
#      and merges keys from the operator-controlled /dstack/user_config.
#      All of that is deleted here (see the "Operator root/SSH access"
#      note further down). This script is part of the measured app_compose
#      (it is hashed into compose_hash), so leaving a root shell or SSH
#      login in it would let an adversarial operator swap Envoy or the
#      upstream at runtime behind an otherwise-valid measurement —
#      defeating the entire point of measuring the image. With these paths
#      gone, a good measurement actually constrains runtime behaviour.
#      Operator-controlled /dstack/user_config is likewise ignored.
#
#   2. GHCR digest fix: upstream's GHCR verification mis-parses
#      digest-pinned image refs (`ghcr.io/owner/repo@sha256:...`) — it
#      treats `repo@sha256:...` as the repo and `latest` as the tag,
#      builds a malformed manifest URL, 404s, and aborts before the
#      container starts. BM deploys always pin images by digest, so they
#      always hit this. The block below splits `@`-pinned refs into the
#      bare repo and the `sha256:...` digest and verifies by digest.
#
# The release tooling bundles this file, writes it to the deploy staging
# dir, and passes it to `phala deploy --pre-launch-script`. Its exact
# bytes are hashed into compose_hash (see scripts/derive_compose_hash.py),
# so any change here is a re-measured image release.
echo "----------------------------------------------"
echo "Running Phala Cloud Pre-Launch Script v0.0.14"
echo "----------------------------------------------"
set -e

# Function: notify host
notify_host() {
  if command -v dstack-util >/dev/null 2>&1; then
    dstack-util notify-host -e "$1" -d "$2"
  else
    tdxctl notify-host -e "$1" -d "$2"
  fi
}

notify_host_hoot_info() {
  notify_host "boot.progress" "$1"
}

notify_host_hoot_error() {
  notify_host "boot.error" "$1"
}

# Function: Perform Docker cleanup
perform_cleanup() {
  echo "Pruning unused images"
  docker image prune -af
  echo "Pruning unused volumes"
  docker volume prune -f
  notify_host_hoot_info "docker cleanup completed"
}

# Function: Check Docker login status without exposing credentials
check_docker_login() {
  local registry="$1"

  # When registry is specified, check auth entry for that registry in Docker config
  if [[ -n "$registry" ]]; then
    local docker_config_path="${DOCKER_CONFIG:-$HOME/.docker}/config.json"
    if [[ -f "$docker_config_path" ]] && grep -q "$registry" "$docker_config_path"; then
      return 0
    else
      return 1
    fi
  fi

  # Fallback check when no explicit registry is provided
  if docker info 2>/dev/null | grep -q "Username"; then
    return 0
  else
    return 1
  fi
}

# Main logic starts here
echo "Starting login process..."

# Check if Docker credentials exist
if [[ -n "$DSTACK_DOCKER_USERNAME" && -n "$DSTACK_DOCKER_PASSWORD" ]]; then
  echo "Docker credentials found"
  DOCKER_REGISTRY_TARGET="${DSTACK_DOCKER_REGISTRY:-docker.io}"
  echo "Target Docker registry: $DOCKER_REGISTRY_TARGET"

  # Check if already logged in
  if check_docker_login "$DSTACK_DOCKER_REGISTRY"; then
    echo "Already logged in to Docker registry: $DOCKER_REGISTRY_TARGET"
  else
    echo "Logging in to Docker registry: $DOCKER_REGISTRY_TARGET"
    # Login without exposing password in process list
    if [[ -n "$DSTACK_DOCKER_REGISTRY" ]]; then
      if echo "$DSTACK_DOCKER_PASSWORD" | docker login -u "$DSTACK_DOCKER_USERNAME" --password-stdin "$DSTACK_DOCKER_REGISTRY"; then
        echo "Docker login successful: $DOCKER_REGISTRY_TARGET"
      else
        echo "Docker login failed: $DOCKER_REGISTRY_TARGET"
        notify_host_hoot_error "docker login failed"
        exit 1
      fi
    else
      if echo "$DSTACK_DOCKER_PASSWORD" | docker login -u "$DSTACK_DOCKER_USERNAME" --password-stdin; then
        echo "Docker login successful: $DOCKER_REGISTRY_TARGET"
      else
        echo "Docker login failed: $DOCKER_REGISTRY_TARGET"
        notify_host_hoot_error "docker login failed"
        exit 1
      fi
    fi
  fi
# Check if AWS ECR credentials exist
elif [[ -n "$DSTACK_AWS_ACCESS_KEY_ID" && -n "$DSTACK_AWS_SECRET_ACCESS_KEY" && -n "$DSTACK_AWS_REGION" && -n "$DSTACK_AWS_ECR_REGISTRY" ]]; then
  echo "AWS ECR credentials found"

  # Check if AWS CLI is installed
  if [ ! -f "./aws/dist/aws" ]; then
    notify_host_hoot_info "awscli not installed, installing..."
    echo "AWS CLI not installed, installing..."
    curl "https://awscli.amazonaws.com/awscli-exe-linux-x86_64-2.24.14.zip" -o "awscliv2.zip"
    if ! echo "6ff031a26df7daebbfa3ccddc9af1450 awscliv2.zip" | md5sum -c; then
      echo "MD5 checksum failed"
      notify_host_hoot_error "awscli install failed"
      exit 1
    fi
    unzip awscliv2.zip &>/dev/null
  else
    echo "AWS CLI is already installed: ./aws/dist/aws"
  fi

  # Set AWS credentials as environment variables
  export AWS_ACCESS_KEY_ID="$DSTACK_AWS_ACCESS_KEY_ID"
  export AWS_SECRET_ACCESS_KEY="$DSTACK_AWS_SECRET_ACCESS_KEY"
  export AWS_DEFAULT_REGION="$DSTACK_AWS_REGION"

  # Set session token if provided (for temporary credentials)
  if [[ -n "$DSTACK_AWS_SESSION_TOKEN" ]]; then
    echo "AWS session token found, using temporary credentials"
    export AWS_SESSION_TOKEN="$DSTACK_AWS_SESSION_TOKEN"
  fi

  # Test AWS credentials before attempting ECR login
  echo "Testing AWS credentials..."
  if ! ./aws/dist/aws sts get-caller-identity &>/dev/null; then
    echo "AWS credentials test failed"
    # For session token credentials, this might be expected if they're expired
    # Log warning but don't fail startup
    if [[ -n "$DSTACK_AWS_SESSION_TOKEN" ]]; then
      echo "Warning: AWS temporary credentials may have expired, continuing startup"
      notify_host_hoot_info "AWS temporary credentials may have expired"
    else
      echo "AWS credentials test failed"
      notify_host_hoot_error "Invalid AWS credentials"
      exit 1
    fi
  else
    echo "Logging in to AWS ECR..."
    if ./aws/dist/aws ecr get-login-password --region "$DSTACK_AWS_REGION" | docker login --username AWS --password-stdin "$DSTACK_AWS_ECR_REGISTRY"; then
      echo "AWS ECR login successful"
      notify_host_hoot_info "AWS ECR login successful"
    else
      echo "AWS ECR login failed"
      # For session token credentials, don't fail startup if login fails
      if [[ -n "$DSTACK_AWS_SESSION_TOKEN" ]]; then
        echo "Warning: AWS ECR login failed with temporary credentials, continuing startup"
        notify_host_hoot_info "AWS ECR login failed with temporary credentials"
      else
        notify_host_hoot_error "AWS ECR login failed"
        exit 1
      fi
    fi
  fi
fi

perform_cleanup

#
# GHCR image pull access verification (pure HTTP, no docker daemon).
#
# Digest-aware fix: upstream v0.0.14 splits the ref on `:` only, so a
# digest-pinned ref `ghcr.io/owner/repo@sha256:...` is mis-parsed
# (`repo@sha256` taken as the repo, `...` as the tag) and the manifest
# request 404s. The `@`-vs-`:` split below handles both digest-pinned and
# tag-pinned refs.
#
if [[ "$DOCKER_REGISTRY_TARGET" == "ghcr.io" && -n "$DSTACK_DOCKER_USERNAME" && -n "$DSTACK_DOCKER_PASSWORD" ]]; then
  COMPOSE_IMAGES=$(grep 'image:' /dstack/docker-compose.yaml 2>/dev/null | awk '{print $2}' | tr -d '"'\' || true)
  for img in $COMPOSE_IMAGES; do
    [[ "$img" != ghcr.io/* ]] && continue
    ref="${img#ghcr.io/}"
    if [[ "$ref" == *@* ]]; then
      repo="${ref%%@*}"
      tag="${ref#*@}"
    else
      repo="${ref%%:*}"
      tag="${ref##*:}"
      [[ "$tag" == "$ref" || "$tag" == "$repo" ]] && tag="latest"
    fi
    echo "Verifying GHCR pull access: $img"
    token=$(curl -sf -u "$DSTACK_DOCKER_USERNAME:$DSTACK_DOCKER_PASSWORD" \
      "https://ghcr.io/token?service=ghcr.io&scope=repository:${repo}:pull" | jq -r '.token // empty' || true)
    if [[ -z "$token" ]]; then
      echo "ERROR: GHCR token exchange failed for $img"
      notify_host_hoot_error "GHCR token exchange failed: $img"
      exit 1
    fi
    http_code=$(curl -s -o /dev/null -w "%{http_code}" -H "Authorization: Bearer $token" \
      -H "Accept: application/vnd.oci.image.index.v1+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.docker.distribution.manifest.v2+json" \
      "https://ghcr.io/v2/${repo}/manifests/${tag}")
    if [[ "$http_code" != "200" ]]; then
      echo "ERROR: GHCR pull access denied for $img (HTTP $http_code)"
      notify_host_hoot_error "GHCR pull access denied: $img (HTTP $http_code)"
      exit 1
    fi
    echo "GHCR pull access OK: $img"
  done
fi

#
# Operator root/SSH access is intentionally NOT provisioned (BM TEE
# hardening — see the header note).
#
# Upstream stock v0.0.14 sets a root password from $DSTACK_ROOT_PASSWORD
# (or a random one), installs SSH authorized_keys from
# $DSTACK_ROOT_PUBLIC_KEY / $DSTACK_AUTHORIZED_KEYS, and appends keys from
# the operator-controlled /dstack/user_config. Every one of those inputs
# is operator-controlled and would grant a root shell / SSH login inside
# the measured guest, letting an adversarial miner swap Envoy or the
# upstream at runtime behind a valid measurement. All of it is removed so
# a good measurement constrains runtime behaviour. The env vars are left
# untouched (never consumed); /dstack/user_config is ignored entirely.
echo "Skipping operator root-password / SSH-key provisioning (BM TEE hardening)"

if [[ -S /var/run/dstack.sock ]]; then
  DSTACK_APP_ID=$(curl -s --unix-socket /var/run/dstack.sock http://dstack/Info | jq -j .app_id)
  export DSTACK_APP_ID
elif [[ -S /var/run/tappd.sock ]]; then
  DSTACK_APP_ID=$(curl -s --unix-socket /var/run/tappd.sock http://dstack/prpc/Tappd.Info | jq -j .app_id)
  export DSTACK_APP_ID
fi
# Resolve DSTACK_GATEWAY_DOMAIN from the measured app-compose.json ONLY.
# Stock consulted the operator-controlled /dstack/user_config first and
# also honoured any pre-existing (operator/platform-provided)
# DSTACK_GATEWAY_DOMAIN. Both are operator-controllable, so both are
# rejected here (BM TEE hardening — user_config is ignored): any inbound
# value is unset first, and the domain is then sourced solely from
# app-compose.json, which is part of the measured compose and therefore
# tamper-evident. This makes the app domain something the operator cannot
# influence.
unset DSTACK_GATEWAY_DOMAIN
if [[ $(jq 'has("default_gateway_domain")' app-compose.json) == "true" ]]; then
  DSTACK_GATEWAY_DOMAIN=$(jq -j '.default_gateway_domain' app-compose.json)
  export DSTACK_GATEWAY_DOMAIN
fi
if [[ -n "$DSTACK_GATEWAY_DOMAIN" ]]; then
  export DSTACK_APP_DOMAIN="$DSTACK_APP_ID.$DSTACK_GATEWAY_DOMAIN"
fi

echo "----------------------------------------------"
echo "Script execution completed"
echo "----------------------------------------------"
