# Operating Satchel

This guide covers running Satchel for other people: the LND node behind it,
the macaroon, funding, the reverse proxy, the operator pages, backups,
upgrades, and monitoring. For every configuration key see
[configuration.md](configuration.md); for sign-up abuse, rate limits, and IP
blocks see [abuse-protection.md](abuse-protection.md).

Satchel is custodial test software. Everything in the database is an IOU
from you, the operator, backed by test sats on your node. Tell your users
that, and never connect it to a mainnet node (it refuses to start if you do).

## What you need

- A Linux host for Satchel (x86_64 or aarch64). It is small: one process
  and one SQLite file.
- An LND node on a test network: Mutinynet or another signet, testnet, or
  regtest. It can run on the same host or elsewhere on a private network.
- A domain name and an HTTPS reverse proxy. Lightning Addresses are
  `<username>@<host of server.public_url>`, and LNURL requires HTTPS.
- Optionally, a private name for the operator pages, reachable only over
  your VPN.

Run exactly one Satchel process per database. The single database writer
and the in-memory record of payments in flight assume it.

## LND

Satchel talks to LND's REST interface with one macaroon. Use a recent LND
release; the official binaries and the `lightninglabs/lnd` images include
the router and invoices sub-servers that Satchel needs (`/v2/router/send`,
`/v2/router/track`, `/v2/invoices/cancel`).

The parts of `lnd.conf` that matter:

```ini
[Application Options]
# REST on a private address Satchel can reach. 127.0.0.1 when on the same host.
restlisten=127.0.0.1:8080
# If Satchel connects over a private network, add that address to the
# certificate, and let LND regenerate the certificate when addresses change.
# tlsextraip=10.0.0.5
# tlsautorefresh=true

[Bitcoin]
bitcoin.signet=true
# For Mutinynet, also set bitcoin.signetchallenge and a chain backend as
# Mutinynet's documentation describes.
```

- `lnd.rest_host` must be an `IP:port`, not a host name, and LND's TLS
  certificate must list that IP. LND's own certificate covers `127.0.0.1`
  and the host's interface addresses; for another address use `tlsextraip`,
  then delete `tls.cert` and `tls.key` (or set `tlsautorefresh`) and restart
  LND so it writes a new certificate. Give Satchel the new `tls.cert`.
- Satchel trusts only that certificate for the connection, and checks the
  address, so a certificate from another node does not work.
- At startup Satchel calls `GetInfo` and stops unless the node reports one
  chain, `bitcoin`, on `testnet`, `testnet4`, `signet`, `regtest`, or
  `simnet`. Set `lnd.expected_network` to pin the one you mean (Mutinynet
  reports `signet`).
- If LND is not synced to the chain yet, Satchel logs a warning and starts
  anyway.

## The macaroon

Bake a macaroon with exactly the permissions Satchel uses, under its own
root key ID so you can revoke it without touching your other macaroons:

```sh
lncli bakemacaroon --root_key_id 3001 --save_to satchel.macaroon \
  info:read invoices:read invoices:write offchain:read offchain:write onchain:read
```

Any unused number works as the root key ID; `lncli listmacaroonids` shows
the ones in use. Never use ID `0`: that is the root key behind
`admin.macaroon` and the other default macaroons.

What the permissions are for:

| Permission | Used for |
| --- | --- |
| `info:read` | `GetInfo`: the network check and node alias at startup. |
| `invoices:read` | Looking up invoices and following the settled-invoice stream. |
| `invoices:write` | Creating invoices for receives and LNURL-pay, and canceling them. |
| `offchain:read` | Decoding invoices, tracking payments, and reading channel balances. |
| `offchain:write` | Sending payments. |
| `onchain:read` | Reading the on-chain balance for the operator page. |

`offchain:write` can spend everything in the node's channels, so treat the
macaroon as a hot-wallet key: keep it readable only by the Satchel service,
and keep no more on the node than the test network needs. It cannot move
on-chain funds or open and close channels.

To rotate or revoke it:

```sh
lncli deletemacaroonid 3001   # every macaroon baked with ID 3001 stops working
lncli bakemacaroon --root_key_id 3002 --save_to satchel.macaroon \
  info:read invoices:read invoices:write offchain:read offchain:write onchain:read
```

Then install the new file and restart Satchel. Satchel reads the macaroon
and certificate once at startup.

## Funding the node and the faucet

Every balance in Satchel is backed by your node's channels:

- **Outbound liquidity** (your local channel balance) pays users' outgoing
  payments. Keep it above the sum of all balances, which the operator page
  shows as liabilities and `/metrics` reports as `satchel_liabilities_msat`.
- **Inbound liquidity** (the remote side of your channels) lets users
  receive from other nodes. A node with only outbound channels cannot receive
  anything from outside. Ask a peer to open a channel to you, or open a
  channel and then pay out part of it to another wallet you control.
- Payments between two Satchel accounts never touch Lightning and need no
  liquidity.

To fund the node, get test coins from your network's faucet (for Mutinynet,
<https://faucet.mutinynet.com>, which can also open a channel to your node),
then open channels to well-connected peers on that network.

The faucet (`[faucet] enabled = true`) credits accounts from nothing: each
grant is an IOU on your node's channel balance, and Satchel refuses a grant
when all balances together would exceed the node's local channel balance.
Size `global_daily_sat` so that a full day of grants is a fraction of your
outbound liquidity, and watch the
[liabilities alert](#what-to-alert-on). The operator page can also credit an
account by hand; that is the same kind of IOU.

## Install and run

Install the release binary (see the [README](../README.md#release-binary)),
or use the [Docker image](docker.md) or the
[NixOS module](../README.md#nixos). Create the operator password hash:

```sh
satchel hash-password < operator-password.txt > /etc/satchel/admin-password.hash
```

A systemd unit with the credentials passed through `LoadCredential`, so the
service user never needs access to LND's directory
([`example/satchel.service.example`](../example/satchel.service.example)):

```ini
[Unit]
Description=Satchel (test networks only)
After=network-online.target
Wants=network-online.target

[Service]
DynamicUser=yes
StateDirectory=satchel
ExecStart=/usr/local/bin/satchel --config /etc/satchel/config.toml
LoadCredential=tls.cert:/srv/lnd/tls.cert
LoadCredential=wallet.macaroon:/srv/lnd/wallet.macaroon
LoadCredential=admin-password.hash:/etc/satchel/admin-password.hash
Restart=on-failure
RestartSec=10
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
UMask=0077

[Install]
WantedBy=multi-user.target
```

With this unit, set `database_path = "/var/lib/satchel/wallet.db"` and the
relative credential names (`tls.cert`, `wallet.macaroon`,
`admin-password.hash`) in the configuration. Satchel stops on `SIGTERM`
after finishing requests in progress.

## Reverse proxy

Satchel listens on plain HTTP on a private address. The reverse proxy in
front of it must:

1. Terminate HTTPS for `server.public_url` (and `server.operator_url`).
2. Pass the original `Host` header. Satchel uses it to tell the operator
   origin from the public one.
3. Set a client address header, overwriting anything the client sent, and
   name that header in `server.client_ip_header`. Without it, every request
   seems to come from the proxy, and the per-address rate limits apply to
   all your users at once.
4. Keep `/healthz`, `/metrics`, and `/admin` off the public name. Satchel
   already answers 404 for `/admin` on the public name when
   `server.operator_url` is set; blocking it in the proxy too costs nothing.

The examples below use `X-Real-IP`, so the configuration has:

```toml
[server]
bind_address = "127.0.0.1:8095"
public_url = "https://wallet.example.org"
operator_url = "https://wallet-admin.example.org:9443"
client_ip_header = "x-real-ip"
metrics_address = "127.0.0.1:9095"
```

If the proxy itself sits behind another proxy or a CDN, the address it sees
is that proxy's. Configure the outer proxy to pass the client address (for
example with the PROXY protocol) and the inner one to trust it, or every
user shares one rate limit.

### Caddy

Caddy gets and renews the public certificate on its own.

```caddyfile
wallet.example.org {
	@private path /healthz /metrics /admin /admin/*
	respond @private 404

	reverse_proxy 127.0.0.1:8095 {
		header_up X-Real-IP {remote_host}
	}
}

# Operator origin: reachable only from the VPN.
wallet-admin.example.org:9443 {
	tls internal
	@outside not remote_ip 10.0.0.0/8
	respond @outside 403

	reverse_proxy 127.0.0.1:8095 {
		header_up X-Real-IP {remote_host}
	}
}
```

`header_up` replaces any `X-Real-IP` the client sent. Caddy also sets
`X-Forwarded-For` to the client address and drops client-sent values unless
you configure `trusted_proxies`, so `client_ip_header = "x-forwarded-for"`
works with Caddy too.

### nginx

```nginx
server {
    listen 80;
    listen [::]:80;
    server_name wallet.example.org;
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name wallet.example.org;

    ssl_certificate     /etc/letsencrypt/live/wallet.example.org/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/wallet.example.org/privkey.pem;

    location = /healthz { return 404; }
    location = /metrics { return 404; }
    location = /admin   { return 404; }
    location ^~ /admin/ { return 404; }

    location / {
        proxy_pass http://127.0.0.1:8095;
        proxy_http_version 1.1;
        proxy_set_header Host $http_host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
        client_max_body_size 64k;
    }
}

# Operator origin: reachable only from the VPN.
server {
    listen 9443 ssl;
    http2 on;
    server_name wallet-admin.example.org;

    ssl_certificate     /etc/nginx/tls/wallet-admin.example.org.crt;
    ssl_certificate_key /etc/nginx/tls/wallet-admin.example.org.key;

    allow 10.0.0.0/8;
    deny all;

    location / {
        proxy_pass http://127.0.0.1:8095;
        proxy_http_version 1.1;
        proxy_set_header Host $http_host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

`proxy_set_header Host $http_host` passes the host and port the browser
used; nginx's default (`$proxy_host`) would send `127.0.0.1:8095`, and
Satchel would not recognise the operator origin. `X-Forwarded-For` is set
to the client address only, rather than appended to, so a client cannot
plant a value in it.

## The operator origin

The operator page (`/admin`) shows every account and balance and can
freeze accounts, credit them, and block client addresses. It has its own
password (`server.admin_password_hash_file`), its own session cookie, and
12-hour sessions, separate from user accounts.

Put it on its own origin that only you can reach:

1. Pick a private name, for example `wallet-admin.example.org`, that
   resolves only on your VPN or in private DNS, and set
   `server.operator_url = "https://wallet-admin.example.org:9443"` (any
   port; include it if it is not 443).
2. Serve that name from the same reverse proxy to the same Satchel listener,
   restricted to your VPN's addresses, as in the examples above.
3. Get a certificate for it. A public ACME HTTP challenge cannot reach a
   private name; use a DNS challenge, an internal CA (Caddy's `tls internal`,
   then trust its root on your devices), or your VPN's certificate feature.

With `operator_url` set, `/admin` answers only when the request's `Host` is
that origin's host and port, and returns 404 on the public name. Without it,
`/admin` is on the public name, protected only by the password and the
login rate limit.

## Backups

The database holds accounts, password hashes, linked Nostr keys, sessions,
and the ledger: the only record of who owns what. LND holds the funds. You
need both: back up LND's channel state as LND documents
(`channel.backup`), and back up Satchel's database as below.

Take consistent copies while Satchel runs with the SQLite online backup:

```sh
install -d -m 0700 /var/backups/satchel
sqlite3 /var/lib/satchel/wallet.db \
  ".timeout 5000" \
  ".backup '/var/backups/satchel/wallet-$(date -u +%Y%m%dT%H%M%SZ).db'"
```

Do not copy the `.db` file with `cp` while Satchel runs: without the `-wal`
file it can be incomplete. Check a backup with
`sqlite3 <file> "PRAGMA integrity_check"` (it prints `ok`). Backups contain
password hashes and session token hashes: keep them private. Run the backup
from a timer every few minutes; the database is small. For continuous
replication, a tool such as Litestream also works.

To restore:

1. Stop Satchel.
2. Move the current files aside, for example `wallet.db` to `wallet.db.old`,
   and the same for `wallet.db-wal` and `wallet.db-shm` if they exist. Keep
   them until the restore is checked.
3. Copy the backup to `server.database_path` and give it the same owner
   and mode as the old file, for example
   `chown --reference=wallet.db.old wallet.db` and `chmod 0600 wallet.db`.
   With systemd `DynamicUser`, the real directory is
   `/var/lib/private/satchel`.
4. Start Satchel. It reconciles pending payments and open invoices with LND
   at startup.
5. Compare liabilities with the node's channel balance on the operator page.

A restore loses everything written after the backup: new accounts,
invoices, and ledger entries. Invoices created after the backup and paid
since are unknown to the restored database, and payments sent after the
backup reappear as balance. On a test network the simplest fix is a
correcting credit from the operator page, or telling users.

## Upgrading

1. Read [CHANGELOG.md](../CHANGELOG.md) for the release: renamed or new
   configuration keys, and anything marked as needing action. Unknown keys
   stop the server, so fix the configuration first.
2. Download and verify the new release archive (or pull the new image).
3. Back up the database as above.
4. Stop Satchel, replace the binary, and start it. Database migrations run
   at startup.
5. Check `/healthz` and the log line `satchel is listening`.

Migrations only go forward. To go back to an older release, restore the
backup you took before upgrading; do not run an older binary against a
migrated database.

## Health and metrics

`/healthz` answers `200` with
database status, `invoice_stream`, `invoice_stream_state`, and `reconciliation_last_success`.
It answers `503` when the database does not respond. Stream status does not change the HTTP status.

`invoice_stream_state` is `connected`, `awaiting_response`, or `disconnected`.
LND's REST proxy can wait for the first invoice before sending response headers.
An idle `awaiting_response` subscription alone does not prove an outage.

Reconciliation checks invoices, pending payments, and node balances every minute.
`reconciliation_last_success` is its last complete successful pass, in Unix seconds, or zero before success.
A failed lookup or ledger write leaves the previous success time unchanged.

Allow long idle connections through any LND proxy, while retaining connection and TLS handshake limits.
For example, a 24-hour idle timeout avoids reconnecting every ten minutes during quiet periods.

`/metrics` is Prometheus text. It is served only on `server.metrics_address`
(together with a second `/healthz`); keep that listener private.

```yaml
scrape_configs:
  - job_name: satchel
    static_configs:
      - targets: ["127.0.0.1:9095"]
```

Every metric is a total; none has a per-account label.

| Metric | Type | Meaning |
| --- | --- | --- |
| `satchel_accounts` | gauge | Accounts. |
| `satchel_frozen_accounts` | gauge | Frozen accounts. |
| `satchel_liabilities_msat` | gauge | Sum of all balances. |
| `satchel_node_channel_local_msat` | gauge | The node's local channel balance, read every minute (absent until first read). |
| `satchel_pending_payments` | gauge | Outgoing payments without a final status. |
| `satchel_open_invoices` | gauge | Unpaid invoices. |
| `satchel_faucet_last_day_msat` | gauge | Faucet credits in the last 24 hours. |
| `satchel_payments_total{kind,outcome}` | counter | Lightning payments succeeded or failed, and internal payments. |
| `satchel_invoices_settled_total{source}` | counter | Invoices paid over Lightning, from LNURL or the wallet page. |
| `satchel_faucet_grants_total` | counter | Faucet grants. |
| `satchel_signups_total` | counter | Accounts created. |
| `satchel_login_failures_total` | counter | Failed logins. |
| `satchel_rate_limited_total{scope}` | counter | Requests refused by rate limits and global caps, by scope (`signup`, `signup-global`, `login-ip`, `lnurl-ip`, `send`, and so on). |
| `satchel_blocked_requests_total` | counter | Requests refused by operator IP blocks. |
| `satchel_pow_checks_total{outcome}` | counter | Proof-of-work solutions checked, `verified` or `rejected`. |
| `satchel_pow_difficulty_bits` | gauge | Leading zero bits a new account needs now. |
| `satchel_faucet_paid_msat_total` | counter | Faucet sats paid since start, in msat. |
| `satchel_invoice_stream_up` | gauge | `1` after LND accepts the invoice subscription. |
| `satchel_invoice_stream_connecting` | gauge | `1` while waiting for LND's subscription response. |
| `satchel_invoice_stream_reconnects_total` | counter | Subscription failures or endings followed by a reconnect. |
| `satchel_reconciliation_last_attempt_timestamp_seconds` | gauge | Last reconciliation start, in Unix seconds. |
| `satchel_reconciliation_last_success_timestamp_seconds` | gauge | Last complete successful reconciliation, in Unix seconds. |
| `satchel_reconciliation_failures_total` | counter | Reconciliation passes with at least one failure. |

Counters start at zero when the process starts. What the abuse-related
metrics mean in practice: [abuse-protection.md](abuse-protection.md#metrics).

### What to alert on

```yaml
groups:
  - name: satchel
    rules:
      - alert: SatchelDown
        expr: up{job="satchel"} == 0
        for: 5m
      - alert: SatchelLiabilitiesExceedChannels
        # Users hold more than the node can pay out.
        expr: satchel_liabilities_msat > satchel_node_channel_local_msat
        for: 15m
      - alert: SatchelReconciliationStale
        expr: up{job="satchel"} == 1 unless on(instance,job) (time() - satchel_reconciliation_last_success_timestamp_seconds < 180)
        for: 5m
      - alert: SatchelInvoiceStreamReconnecting
        expr: increase(satchel_invoice_stream_reconnects_total[30m]) > 2
        for: 5m
      - alert: SatchelPaymentsStuck
        expr: satchel_pending_payments > 0
        for: 30m
      - alert: SatchelFaucetNearCap
        # Replace 500000000 with global_daily_sat * 1000.
        expr: satchel_faucet_last_day_msat > 0.8 * 500000000
        for: 5m
      - alert: SatchelSignupSurge
        expr: increase(satchel_signups_total[1h]) > 500
      - alert: SatchelRateLimiting
        expr: sum(rate(satchel_rate_limited_total[5m])) > 1
        for: 10m
      - alert: SatchelPowRejections
        # Many bad proof-of-work solutions: usually a sign-up script.
        expr: rate(satchel_pow_checks_total{outcome="rejected"}[10m]) > 0.5
        for: 10m
```

Also probe `/healthz` from outside the host. The thresholds above are
starting points; set them from what normal looks like on your server.

## Logs

Satchel logs to standard output, human-readable by default and as JSON
lines with `SATCHEL_LOG_JSON=true`. `RUST_LOG` sets the filter (default
`info`). Startup logs the network, node alias, and node public key, which
is a quick check that you reached the node you meant.
