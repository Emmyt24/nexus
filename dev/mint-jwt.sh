#!/usr/bin/env bash
# Mint a JWT the API will accept, without going through the OTP flow.
# For local development only — it needs your JWT_SECRET.
#
#   ./dev/mint-jwt.sh <user_uuid> <email> <role> [hospital_uuid]
#
# role is one of: health_worker | hospital_admin | super_admin |
#                 operations_admin | verification_admin | finance_admin
#
# Example:
#   JWT_SECRET=... ./dev/mint-jwt.sh 9d4e... dr@example.test health_worker
set -euo pipefail

if [ $# -lt 3 ]; then
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
fi

: "${JWT_SECRET:?set JWT_SECRET (same value the server runs with)}"

b64() { openssl base64 -e -A | tr '+/' '-_' | tr -d '='; }

now=$(date +%s)
exp=$((now + 86400))
hospital="null"
[ "${4:-}" != "" ] && hospital="\"$4\""

header='{"alg":"HS256","typ":"JWT"}'
payload="{\"sub\":\"$1\",\"email\":\"$2\",\"role\":\"$3\",\"hospital_id\":$hospital,\"exp\":$exp,\"iat\":$now}"

h=$(printf '%s' "$header"  | b64)
p=$(printf '%s' "$payload" | b64)
sig=$(printf '%s' "$h.$p" | openssl dgst -binary -sha256 -hmac "$JWT_SECRET" | b64)

printf '%s.%s.%s\n' "$h" "$p" "$sig"
