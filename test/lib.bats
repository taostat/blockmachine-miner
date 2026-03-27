#!/usr/bin/env bats

setup() {
  # Source lib.sh without triggering install
  source "${BATS_TEST_DIRNAME}/../lib.sh"
}

# --- is_ipv4 ---

@test "is_ipv4 accepts valid IPv4" {
  run is_ipv4 "192.168.1.1"
  [ "$status" -eq 0 ]
}

@test "is_ipv4 accepts loopback" {
  run is_ipv4 "127.0.0.1"
  [ "$status" -eq 0 ]
}

@test "is_ipv4 rejects IPv6" {
  run is_ipv4 "::1"
  [ "$status" -ne 0 ]
}

@test "is_ipv4 rejects hostname" {
  run is_ipv4 "example.com"
  [ "$status" -ne 0 ]
}

@test "is_ipv4 rejects empty string" {
  run is_ipv4 ""
  [ "$status" -ne 0 ]
}

# --- is_ip ---

@test "is_ip accepts IPv4" {
  run is_ip "10.0.0.1"
  [ "$status" -eq 0 ]
}

@test "is_ip accepts IPv6" {
  run is_ip "2001:db8::1"
  [ "$status" -eq 0 ]
}

@test "is_ip rejects hostname" {
  run is_ip "example.com"
  [ "$status" -ne 0 ]
}

# --- info / warn / error ---

@test "info outputs with prefix" {
  run info "hello"
  [ "$status" -eq 0 ]
  [ "$output" = "==> hello" ]
}

@test "warn outputs to stderr" {
  run warn "oops"
  [ "$status" -eq 0 ]
  [[ "$output" == *"WARNING: oops"* ]]
}

@test "error exits with status 1" {
  run error "fatal"
  [ "$status" -eq 1 ]
  [[ "$output" == *"ERROR: fatal"* ]]
}

# --- write_env ---

@test "write_env creates file with correct contents" {
  local tmpfile
  tmpfile=$(mktemp)
  write_env "$tmpfile" "mysecret" "example.com"

  run cat "$tmpfile"
  [[ "$output" == *"SECRET_V1=mysecret"* ]]
  [[ "$output" == *"DOMAIN=example.com"* ]]
  [[ "$output" == *"BACKEND_PORT=9944"* ]]

  rm -f "$tmpfile"
}

@test "write_env sets restrictive permissions" {
  local tmpfile
  tmpfile=$(mktemp)
  write_env "$tmpfile" "secret" "test.com"

  local perms
  perms=$(stat -f '%Lp' "$tmpfile" 2>/dev/null || stat -c '%a' "$tmpfile" 2>/dev/null)
  [ "$perms" = "600" ]

  rm -f "$tmpfile"
}

@test "write_env handles empty domain" {
  local tmpfile
  tmpfile=$(mktemp)
  write_env "$tmpfile" "secret"

  run cat "$tmpfile"
  [[ "$output" == *"DOMAIN="* ]]

  rm -f "$tmpfile"
}

# --- check_command ---

@test "check_command passes for existing command" {
  run check_command "bash" "should not fail"
  [ "$status" -eq 0 ]
}

@test "check_command fails for missing command" {
  run check_command "nonexistent_cmd_xyz" "Install it."
  [ "$status" -eq 1 ]
  [[ "$output" == *"nonexistent_cmd_xyz is required"* ]]
}

# --- check_snapshot_disk_space ---

@test "check_snapshot_disk_space returns 0 when content-length unavailable" {
  # Override curl to return no content-length
  curl() { echo "HTTP/1.1 200 OK"; }
  export -f curl

  run check_snapshot_disk_space "http://fake-url/snapshot.tar.zst"
  [ "$status" -eq 0 ]
}

@test "check_snapshot_disk_space returns 0 when enough space" {
  # 1 GB snapshot, disk check should pass on any dev machine
  curl() {
    echo "HTTP/1.1 200 OK"
    echo "Content-Length: 1073741824"
  }
  export -f curl

  run check_snapshot_disk_space "http://fake-url/snapshot.tar.zst"
  [ "$status" -eq 0 ]
}
