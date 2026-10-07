# Satchel

> **Test networks only.** Satchel is a custodial, unaudited wallet for
> Mutinynet, signet, testnet, and regtest. It refuses to start when its LND
> node is on Bitcoin mainnet, and there is no flag to change that. Never use
> it with real bitcoin.

A small multi-account Lightning wallet with a Lightning Address for every
account, built for people testing Lightning apps: sign up in a browser, pay
test invoices, and receive payouts or refunds at `you@your-wallet-domain`.

One LND node holds the funds; each account's balance lives in an append-only
SQLite ledger. Pages are server-rendered with [maud](https://maud.lambda.xyz)
and [htmx](https://htmx.org); the only script is a small file for copy buttons
and Nostr login.

Satchel is forked from [Koerier](https://github.com/tee8z/koerier), a
Lightning Address server for LND (itself a fork of
[luisschwab/koerier](https://github.com/luisschwab/koerier)), and keeps its
history and its MIT OR Apache-2.0 licenses.

## What it does

- **Accounts**: username and password (argon2id), or Nostr login with a NIP-07
  signer extension; an account can have both. Each account gets
  `username@<domain>`.
- **Receive**: LNURL-pay (LUD-06, LUD-12 comments, LUD-16 Lightning
  Addresses) per account, and a "create invoice" form with a QR code.
  Invoices are credited once, from LND's invoice stream, with reconciliation
  at startup and every minute.
- **Send**: paste a BOLT11 invoice, a Lightning Address, or an LNURL. The
  amount plus a routing-fee budget is reserved first; unused budget or the
  whole amount comes back when LND reports the result. Payments between two
  accounts on this server settle inside the ledger without touching Lightning.
- **Faucet** (off by default): operator-funded test sats with per-account and
  global 24-hour limits, never more than the node's channel balance covers.
- **Operator page**: accounts and balances, total liabilities against the
  node's channel and on-chain balances, faucet usage, freeze and credit
  actions. It uses its own password, separate from user accounts.

## Safety model

- **Network guard**: at startup the server asks LND for its chain and network
  and stops unless it is one of `testnet`, `testnet4`, `signet`, `regtest`,
  or `simnet`. `lnd.expected_network` can pin one of them. Invoices are
  decoded by LND, which rejects invoices for other networks, and `lnbc`
  invoices are refused outright. Every page shows a "Test network only"
  banner.
- **Ledger**: balances are sums of ledger entries. SQLite triggers reject
  updates, deletes, and any entry that would make a balance negative; every
  entry has a unique idempotency key; all writes go through one connection
  in `BEGIN IMMEDIATE` transactions. An invoice can credit one account once,
  whether it is paid over Lightning or internally.
- **Payments**: a payment whose outcome is unknown (for example, LND
  restarted mid-flight) stays pending until reconciliation reads the result
  from LND; it is refunded only when LND reports a failure or says it never
  started the payment. Form submissions carry a request key, so a repeated
  submission returns the first result instead of paying twice.
- **Web**: session cookies are `__Host-`, `HttpOnly`, `SameSite=Lax`; every
  state-changing request must come from this origin and carry the session's
  CSRF token. A strict Content-Security-Policy allows only this site's own
  scripts and styles. Login, sign-up, LNURL callbacks, payments, and invoice
  creation are rate limited per client address and per account.
- **Paying other servers**: Lightning Address lookups resolve the host once,
  refuse loopback, private, and other non-public addresses, pin the
  connection to the checked address, and follow no redirects. The invoice
  returned must match the requested amount and the address metadata hash.

Not in scope: fund recovery, multi-node setups, mainnet hardening, or an
audit. Treat every balance as an IOU from the operator.

## Run

Install Rust 1.95 or later, `pkg-config`, and the OpenSSL development
headers. `nix develop` provides them.

```sh
cargo build --release --locked
cp example/config.toml.example config.toml
./target/release/satchel hash-password < operator-password.txt > admin-password.hash
./target/release/satchel --config config.toml
```

The LND macaroon needs exactly these permissions:

```sh
lncli bakemacaroon --save_to wallet.macaroon \
  info:read invoices:read invoices:write offchain:read offchain:write onchain:read
```

Relative credential paths resolve under `CREDENTIALS_DIRECTORY` when systemd
provides it, and otherwise beside the configuration file. The LND certificate
must cover the IP address in `rest_host`.

Put an HTTPS reverse proxy in front of the private listener (see
[the Caddy example](example/Caddyfile.example)) and set
`server.client_ip_header` to the header it fills, so rate limits see real
client addresses. Keep `/metrics` and `/healthz` private; with
`server.metrics_address` they get their own listener.

Environment variables: `SATCHEL_CONFIG` (config path),
`SATCHEL_ADMIN_PASSWORD_HASH` (operator hash, instead of a file),
`SATCHEL_LOG_JSON=true` (JSON logs), and `RUST_LOG` (log filter). Any
setting can also be overridden with `SATCHEL_<SECTION>__<KEY>`, for example
`SATCHEL_SERVER__PUBLIC_URL=https://wallet.example.org` or
`SATCHEL_FAUCET__ENABLED=false`; values are read as TOML (numbers, booleans,
arrays) and otherwise as strings, and are validated like the file.

## Configuration

See [the complete example](example/config.toml.example). Amounts are in sats.

| Section | Keys |
| --- | --- |
| `[server]` | `bind_address`, `public_url` (HTTPS origin; addresses use its host), `database_path`, `metrics_address`, `client_ip_header`, `admin_password_hash_file`, `reserved_usernames`, `session_days`, `allow_private_lnurl_hosts` (local regtest only) |
| `[lnd]` | `rest_host`, `tls_cert_path`, `macaroon_path`, `request_timeout_secs`, `payment_timeout_secs`, `expected_network` |
| `[limits]` | `max_balance_sat`, `max_payment_sat`, `min_receive_sat`, `max_receive_sat`, `invoice_expiry_secs`, `fee_limit_ppm`, `min_fee_limit_sat` |
| `[faucet]` | `enabled` (default `false`), `amount_sat`, `per_account_daily_sat`, `global_daily_sat` |
| `[rate_limits]` | `login_per_ip_per_minute`, `login_per_account_per_hour`, `signup_per_ip_per_hour`, `lnurl_per_ip_per_minute`, `lnurl_per_account_per_minute`, `send_per_account_per_minute`, `receive_per_account_per_minute` |

The balance cap is enforced when invoices are created; a payment that
arrives for an existing invoice is always credited.

## NixOS

The flake exports `packages.<system>.satchel` for `x86_64-linux` and
`aarch64-linux`, and `nixosModules.default` (`services.satchel`).

```nix
services.satchel = {
  enable = true;
  publicUrl = "https://wallet.example.org";
  clientIpHeader = "x-forwarded-for";
  metricsAddress = "127.0.0.1:9095";
  adminPasswordHashFile = "/run/secrets/satchel-admin.hash";
  lnd = {
    restHost = "127.0.0.1:8080";
    tlsCertPath = "/var/lib/lnd/tls.cert";
    macaroonPath = "/run/secrets/satchel.macaroon";
    expectedNetwork = "signet";
  };
  faucet = { enabled = true; amount_sat = 10000; };
};
```

Credentials stay host files loaded with systemd `LoadCredential`; the
database lives in `/var/lib/satchel`. The module opens no firewall
ports and configures no DNS or TLS.

```sh
nix build .#satchel
nix flake check
```

## Metrics

`/metrics` (Prometheus text) reports totals only, never per-account labels:
accounts, frozen accounts, liabilities, the node's local channel balance,
pending payments, open invoices, faucet use, payment and invoice counters,
sign-ups, failed logins, rate-limited requests, and whether the LND invoice
stream is connected.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The tests run against SQLite and an in-memory LND: exactly-once credits, no
negative balances, idempotent requests, concurrent sends, refunds and
reconciliation, internal transfers, the faucet limits, the LNURL endpoints,
the mainnet refusal, Nostr login, CSRF and origin checks, and the operator
pages.

Release archives come from the [release workflow](.github/workflows/release.yml),
which runs only by `workflow_dispatch`.

## License

MIT OR Apache-2.0, as in Koerier. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE). htmx 4.0.0 is vendored under its own
BSD Zero Clause license in `assets/vendor/htmx`.
