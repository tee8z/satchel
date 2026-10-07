#!/usr/bin/env bash
# Brings up the regtest stack and makes it usable: mines coins, funds both
# LND nodes, opens a channel from Satchel's node to the peer, bakes Satchel's
# macaroon, writes its credentials, starts Satchel, and prints the URL.
# Safe to run again. Regtest only.
set -euo pipefail
cd "$(dirname "$0")"

url=http://127.0.0.1:8095
peer_address=172.29.42.12:9735

compose() { docker compose "$@"; }
btc() { compose exec -T bitcoind bitcoin-cli -regtest -rpcuser=satchel -rpcpassword=satchel "$@"; }
# lncli inside a node's container; the image includes jq, used for its JSON.
lncli_in() {
  local node=$1
  shift
  compose exec -T "$node" lncli --network=regtest "$@"
}
jq_in() {
  local node=$1
  shift
  compose exec -T "$node" jq -r "$@"
}
say() { printf '==> %s\n' "$*"; }

wait_for() {
  local what=$1
  shift
  for _ in $(seq 1 90); do
    if "$@" > /dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  echo "Timed out waiting for $what" >&2
  exit 1
}

synced() { lncli_in "$1" getinfo | jq_in "$1" -e '.synced_to_chain' > /dev/null; }
funded() { [ "$(lncli_in "$1" walletbalance | jq_in "$1" '.confirmed_balance')" != "0" ]; }
channel_active() { lncli_in lnd-wallet listchannels --active_only | jq_in lnd-wallet -e '.channels | length > 0' > /dev/null; }
mempool_has_tx() {
  local txids
  txids=$(btc getrawmempool)
  [[ $txids =~ [0-9a-f]{64} ]]
}
mine() { btc generatetoaddress "$1" "$miner_address" > /dev/null; }

say "Starting bitcoind and two LND nodes"
compose up -d bitcoind lnd-wallet lnd-peer
wait_for "bitcoind" btc getblockchaininfo

say "Creating the miner wallet and mining spendable coins"
btc createwallet miner > /dev/null 2>&1 || btc loadwallet miner > /dev/null 2>&1 || true
miner_address=$(btc -rpcwallet=miner getnewaddress)
height=$(btc getblockcount)
if [ "$height" -lt 101 ]; then
  mine $((101 - height))
fi

say "Waiting for both LND nodes to sync"
wait_for "lnd-wallet" lncli_in lnd-wallet getinfo
wait_for "lnd-peer" lncli_in lnd-peer getinfo
mine 1
wait_for "lnd-wallet to sync" synced lnd-wallet
wait_for "lnd-peer to sync" synced lnd-peer

say "Funding the LND wallets"
for node in lnd-wallet lnd-peer; do
  if ! funded "$node"; then
    address=$(lncli_in "$node" newaddress p2wkh | jq_in "$node" '.address')
    btc -rpcwallet=miner sendtoaddress "$address" 1 > /dev/null
  fi
done
mine 6
wait_for "lnd-wallet funds" funded lnd-wallet
wait_for "lnd-peer funds" funded lnd-peer

say "Opening a channel from Satchel's node to the peer"
peer_pubkey=$(lncli_in lnd-peer getinfo | jq_in lnd-peer '.identity_pubkey')
lncli_in lnd-wallet connect "$peer_pubkey@$peer_address" > /dev/null 2>&1 || true
if ! channel_active; then
  pending=$(lncli_in lnd-wallet pendingchannels | jq_in lnd-wallet '.pending_open_channels | length')
  if [ "$pending" = "0" ]; then
    # 5M sats, 2M of them pushed to the peer: Satchel can send 3M and receive 2M.
    lncli_in lnd-wallet openchannel --node_key "$peer_pubkey" --local_amt 5000000 --push_amt 2000000 > /dev/null
    wait_for "the funding transaction" mempool_has_tx
  fi
  mine 6
  wait_for "the channel" channel_active
fi

say "Writing Satchel's credentials"
lncli_in lnd-wallet bakemacaroon --root_key_id 3001 --save_to /root/.lnd/satchel.macaroon \
  info:read invoices:read invoices:write offchain:read offchain:write onchain:read > /dev/null
compose exec -T lnd-wallet cat /root/.lnd/satchel.macaroon > satchel/satchel.macaroon
compose exec -T lnd-wallet cat /root/.lnd/tls.cert > satchel/tls.cert
if [ ! -s satchel/admin-password.hash ]; then
  od -An -N12 -tx1 /dev/urandom | tr -d ' \n' > satchel/operator-password.txt
  compose run --rm --no-deps -T satchel hash-password < satchel/operator-password.txt > satchel/admin-password.hash
fi
# Satchel runs as uid 65532 in its container. Regtest credentials only.
chmod 0644 satchel/satchel.macaroon satchel/tls.cert satchel/admin-password.hash

say "Starting Satchel"
compose up -d --force-recreate satchel
if command -v curl > /dev/null; then
  wait_for "Satchel" curl -fsS "$url/healthz"
fi

cat << DONE

Satchel is running on regtest.

  Wallet:         $url   (open exactly this address)
  Operator page:  $url/admin
  Operator pass:  $(cat satchel/operator-password.txt)

Sign up, then try:

  Pay your Lightning Address from the peer node:
    ./pay-address.sh <username> 1000

  Pay an invoice created on the wallet page:
    docker compose exec lnd-peer lncli --network=regtest payinvoice --force <invoice>

  Get an invoice from the peer for Satchel's Send form:
    docker compose exec lnd-peer lncli --network=regtest addinvoice --amt 500

  Mine a block:
    docker compose exec bitcoind bitcoin-cli -regtest -rpcuser=satchel -rpcpassword=satchel -rpcwallet=miner -generate 1

Stop with 'docker compose down'; add '-v' to delete all data.
DONE
