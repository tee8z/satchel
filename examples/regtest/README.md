# Satchel on regtest

Everything needed to try Satchel on your own machine in a few minutes: a
regtest bitcoind, two LND nodes with a channel between them, and Satchel with
the faucet on. Nothing here is fit for a public server; the passwords and
credentials are for local use only.

You need Docker with Compose v2 and `curl`. The images are
`bitcoin/bitcoin:29.4`, `lightninglabs/lnd:v0.21.4-beta`, and
`ghcr.io/tee8z/satchel:0.1.0`.

```sh
./setup.sh
```

`setup.sh` starts the containers, creates a miner wallet and mines 101
blocks, funds both LND wallets, opens a 5,000,000 sat channel from Satchel's
node (`lnd-wallet`) to the other node (`lnd-peer`) with 2,000,000 sats pushed
to the peer, bakes Satchel's macaroon, writes the credentials into
`satchel/`, starts Satchel, and prints:

- the wallet URL, `http://127.0.0.1:8095` (open exactly this; the origin
  check refuses `localhost`),
- the operator page and its generated password.

Running it again is safe; it skips what is already done.

## Things to try

1. Open the wallet URL and create an account. Take test sats from the
   faucet.
2. Pay your Lightning Address from the peer, the way another wallet would:

   ```sh
   ./pay-address.sh alice 1500
   ```

3. Create an invoice on the wallet page and pay it from the peer:

   ```sh
   docker compose exec lnd-peer lncli --network=regtest payinvoice --force <invoice>
   ```

4. Pay the peer from Satchel: get an invoice and paste it into Send.

   ```sh
   docker compose exec lnd-peer lncli --network=regtest addinvoice --amt 500
   ```

5. Create a second account and pay its Lightning Address from the first:
   the payment settles inside Satchel without touching Lightning.
6. Open `http://127.0.0.1:8095/admin` with the printed password to see
   balances, liabilities, and the node's channel balance.

## Files

| File | What it is |
| --- | --- |
| `docker-compose.yml` | The four services on a fixed private network (`172.29.42.0/24`). Satchel's `lnd.rest_host` must be an IP address that LND's certificate covers, hence the fixed addresses and `--tlsextraip`. |
| `satchel/satchel.toml` | Satchel's configuration: regtest, the faucet on, private LNURL hosts allowed. |
| `satchel/` (generated) | `tls.cert`, `satchel.macaroon`, `admin-password.hash`, and `operator-password.txt`, written by `setup.sh`. Ignored by git. |
| `setup.sh` | The setup described above. |
| `pay-address.sh` | Pays a Lightning Address from the peer through LNURL-pay. |

To use a locally built image, set `SATCHEL_IMAGE`, for example
`SATCHEL_IMAGE=satchel:dev ./setup.sh`.

If `172.29.42.0/24` collides with a network you already use, change the
subnet and the addresses in `docker-compose.yml` and `rest_host` in
`satchel/satchel.toml`, then delete the LND volumes so the certificate is
made again.

## Stop and clean up

```sh
docker compose down      # stop, keep the chain and wallets
docker compose down -v   # stop and delete all data
```

After `down -v`, delete the generated files in `satchel/` too, and run
`./setup.sh` again for a fresh start.
