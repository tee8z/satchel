#!/usr/bin/env bash
# Pays a Satchel Lightning Address from the peer LND node, the way another
# wallet would: LNURL-pay lookup, then the callback, then the invoice.
# Usage: ./pay-address.sh <username> [sats]
set -euo pipefail
cd "$(dirname "$0")"

user=${1:?usage: $0 <username> [sats]}
sats=${2:-1000}
base=http://127.0.0.1:8095

field() { docker compose exec -T lnd-peer jq -r "$1"; }

params=$(curl -fsS "$base/.well-known/lnurlp/$user")
callback=$(printf '%s' "$params" | field '.callback // empty')
if [ -z "$callback" ]; then
  echo "No Lightning Address $user@127.0.0.1:8095: $params" >&2
  exit 1
fi
reply=$(curl -fsS "$callback?amount=$((sats * 1000))")
invoice=$(printf '%s' "$reply" | field '.pr // empty')
if [ -z "$invoice" ]; then
  echo "The callback returned no invoice: $reply" >&2
  exit 1
fi
docker compose exec -T lnd-peer lncli --network=regtest payinvoice --force "$invoice" > /dev/null
echo "Paid $sats sats to $user@127.0.0.1:8095"
