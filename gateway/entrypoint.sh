#!/bin/sh
set -e

# Default values if not set
: "${BACKEND_HOST:=127.0.0.1}"
: "${BACKEND_PORT:=9944}"
: "${SSL_CERT_PATH:=/etc/nginx/ssl/cert.pem}"
: "${SSL_KEY_PATH:=/etc/nginx/ssl/key.pem}"
: "${METRICS_HOST:=metrics}"
: "${METRICS_PORT:=9100}"
: "${SECRET_V1:=}"
: "${SECRET_V2:=}"

# Build the SECRET_V2 map line only if it differs from SECRET_V1
SECRET_V2_LINE=""
if [ -n "$SECRET_V2" ] && [ "$SECRET_V2" != "$SECRET_V1" ]; then
    SECRET_V2_LINE=$(printf '"Bearer %s" 1;' "$SECRET_V2")
fi
export SECRET_V2_LINE

# envsubst expects a shell-format string listing the variables to substitute.
# Single quotes are intentional — these are literal variable names, not expansions.
# shellcheck disable=SC2016
envsubst '${SECRET_V1} ${SECRET_V2_LINE} ${SSL_CERT_PATH} ${SSL_KEY_PATH} ${BACKEND_HOST} ${BACKEND_PORT} ${METRICS_HOST} ${METRICS_PORT}' \
    < /etc/nginx/conf.d/miner-gateway.conf.template \
    > /etc/nginx/conf.d/default.conf

echo "Nginx configuration rendered"
echo "Backend: ${BACKEND_HOST}:${BACKEND_PORT}"
echo "Metrics: ${METRICS_HOST}:${METRICS_PORT}"
echo "SSL Cert: ${SSL_CERT_PATH}"
