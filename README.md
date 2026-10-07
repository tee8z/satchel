# Satchel

Give every tester a wallet and a Lightning Address on a test network, from
one binary and one LND node. Satchel is a multi-account Lightning wallet that
people open in a browser: they sign up, get `name@your-domain`, pay test
invoices, and receive payouts and refunds. It exists because mobile wallets
do not speak Mutinynet or signet, and the hosted test wallets that gave out
Lightning Addresses have shut down (Mutiny's ended on 2024-12-31). If you
build a Lightning app and want other people to try it on a test network,
Satchel is the wallet you hand them.

> **Test networks only: custodial, no recovery, never point it at mainnet.**
> Satchel runs on Mutinynet, signet, testnet, and regtest. The operator's LND
> node holds every balance; there is no seed, no export, and no way to recover
> funds if the server or its database is lost. It refuses to start when its
> LND node reports Bitcoin mainnet, and there is no flag to change that. It
> has not been audited. Never use it with real bitcoin.

## Features

- **Accounts**: username and password (argon2id), or a Nostr key through a
  NIP-07 signer extension; one account can have both.
- **Lightning Addresses**: every account gets `username@<domain>` over
  LNURL-pay (LUD-06, LUD-12 comments, LUD-16), plus an LNURL QR code.
- **Send and receive**: pay BOLT11 invoices, Lightning Addresses, and LNURLs;
  create invoices with a QR code. A payment reserves the amount plus a
  routing-fee budget and returns what is not spent.
- **Internal transfers**: payments between two accounts on the same server
  settle in the ledger without touching Lightning.
- **Faucet** (off by default): operator-funded test sats with per-account,
  per-address, and global daily caps.
- **Operator page**: accounts, balances, liabilities against the node's
  channel and on-chain balances, faucet use, freeze, credit, and IP blocks,
  behind its own password and optionally on its own private origin.
- **For integrators**: deep links (`/launch/lightning/{invoice}`) that open
  the Send form, an "Open Satchel" handoff that signs a user in from your app
  with their Nostr key, an address lookup API, and QR scanning on the Send
  form. See [Integrate with your app](#integrate-with-your-app).
- **Built to run in public**: a proof of work on every new account, per-address
  limits sized for a conference crowd behind one NAT, global caps, and
  operator IP blocks. See [Run it in public](#run-it-in-public).
- **Metrics and health**: Prometheus metrics (totals only, never per account)
  and `/healthz`, optionally on a private listener.
- **Installable**: a web app manifest and icons, so phones can add it to the
  home screen.

Pages are server-rendered with [maud](https://maud.lambda.xyz) and
[htmx](https://htmx.org), every form works without JavaScript, and balances
live in an append-only SQLite ledger.

## Screenshots

<!--
  Screenshots are added after the first public deployment: sign-up, wallet
  (balance, address, receive, send), a payment in flight, and the operator
  page. Store them under docs/images/ and keep each one under 200 KB.
-->

## Quick start

### Try it on regtest in a few minutes

[`examples/regtest`](examples/regtest) runs bitcoind, two LND nodes with a
channel between them, and Satchel with the faucet on, all on your machine:

```sh
cd examples/regtest
./setup.sh
```

`setup.sh` starts the containers, mines blocks, funds and connects the
nodes, bakes Satchel's macaroon, starts Satchel, and prints the URL to open.
`./pay-address.sh <username> 1000` then pays your new Lightning Address from
the second node. Docker with Compose v2 is all you need.

### Release binary

Each release has Linux archives for x86_64 and aarch64 with OpenSSL linked
in. They are built on Ubuntu 24.04, so use a distribution at least that new
(Debian 13, Ubuntu 24.04, Fedora 40, or later).

```sh
version=0.1.0
system=x86_64-linux  # or aarch64-linux
base=https://github.com/tee8z/satchel/releases/download/v$version
curl -fLO "$base/satchel-$version-$system.tar.gz"
curl -fLO "$base/satchel-$version-$system.tar.gz.sha256"
sha256sum --check "satchel-$version-$system.tar.gz.sha256"
tar -xzf "satchel-$version-$system.tar.gz"
sudo install -m 0755 "satchel-$version-$system/bin/satchel" /usr/local/bin/satchel
```

Then write a configuration, an operator password hash, and an LND macaroon,
and start it:

```sh
curl -fLo config.toml https://raw.githubusercontent.com/tee8z/satchel/master/example/config.toml.example
$EDITOR config.toml   # public_url, lnd.rest_host, lnd.expected_network
satchel hash-password < operator-password.txt > admin-password.hash
lncli bakemacaroon --root_key_id 3001 --save_to wallet.macaroon \
  info:read invoices:read invoices:write offchain:read offchain:write onchain:read
cp ~/.lnd/tls.cert tls.cert
satchel --config config.toml
```

Put an HTTPS reverse proxy in front of it. [docs/operating.md](docs/operating.md)
has Caddy and nginx examples, the macaroon details, and a systemd unit.

### Docker

Each release publishes `ghcr.io/tee8z/satchel` for `linux/amd64` and
`linux/arm64`. The image holds the release binary on a distroless base, runs
as uid 65532, and keeps the database in the `/data` volume:

```sh
cd satchel-config  # config.toml, tls.cert, wallet.macaroon, admin-password.hash
docker run -d --name satchel \
  -v "$PWD:/etc/satchel:ro" \
  -v satchel-data:/data \
  -p 127.0.0.1:8095:8095 \
  ghcr.io/tee8z/satchel:0.1.0
```

The image sets `server.bind_address` to `0.0.0.0:8095` and
`server.database_path` to `/data/wallet.db`; relative credential paths
resolve beside the configuration in `/etc/satchel`, and the files there must
be readable by uid 65532. To hash the operator password:
`docker run --rm -i ghcr.io/tee8z/satchel:0.1.0 hash-password < operator-password.txt`.
[docs/docker.md](docs/docker.md) covers Compose and building the image
yourself.

### NixOS

The flake exports `packages.<system>.satchel` and `nixosModules.default`
(`services.satchel`):

```nix
{
  inputs.satchel.url = "github:tee8z/satchel";

  outputs = { nixpkgs, satchel, ... }: {
    nixosConfigurations.wallet = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        satchel.nixosModules.default
        {
          services.satchel = {
            enable = true;
            publicUrl = "https://wallet.example.org";
            clientIpHeader = "x-real-ip";
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
        }
      ];
    };
  };
}
```

Credentials are loaded with systemd `LoadCredential` and never copied into
the Nix store; the database lives in `/var/lib/satchel`. The module opens no
firewall ports and configures no DNS or TLS.

## Configuration

One TOML file, passed with `--config` or `SATCHEL_CONFIG`. Any key can be
overridden from the environment as `SATCHEL_<SECTION>__<KEY>`, for example
`SATCHEL_FAUCET__ENABLED=false`. Amounts are in sats.

| Section | What it controls |
| --- | --- |
| `[server]` | listener, public origin (addresses use its host), operator origin, database, metrics listener, client address header, handoff origins |
| `[lnd]` | REST address, TLS certificate, macaroon, timeouts, expected network |
| `[limits]` | balance and payment caps, receive range, invoice expiry, routing-fee budget |
| `[faucet]` | on or off, grant size, per-account, per-address, and global daily caps |
| `[rate_limits]` | per-address, per-account, and global request limits |
| `[pow]` | proof-of-work difficulty for new accounts |

Every key with its type, default, and meaning is in
[docs/configuration.md](docs/configuration.md); a complete example is
[example/config.toml.example](example/config.toml.example).

## Integrate with your app

If your app takes Lightning payments on a test network, Satchel can be the
wallet your testers use:

- Link to `https://wallet.example.org/launch/lightning/<bolt11>` to open the
  Send form with the invoice filled in; the user taps **Pay**.
- "Open Satchel": your page signs a Nostr event with the user's key and posts
  it to Satchel, which signs them in (or offers to create a wallet) and opens
  the link you pass.
- Look up a user's Lightning Address with a NIP-98 signed request to
  `/api/v1/address`, for example to fill in a payout address.

Event formats, CORS, and examples: [docs/integrating.md](docs/integrating.md).

## Run it in public

A public test wallet attracts sign-up scripts and faucet drainers. Satchel
asks for a small proof of work on every new account, limits each client
address with limits sized for a few hundred people behind one NAT, caps
totals globally, and lets the operator block address ranges.

- [docs/abuse-protection.md](docs/abuse-protection.md): the proof of work,
  the limits, and IP blocks.
- [docs/operating.md](docs/operating.md): LND requirements, the macaroon,
  funding, reverse proxies, the operator origin, backups, upgrades, and what
  to alert on.

## How balances are kept

- **Network guard**: at startup Satchel asks LND for its chain and network
  and stops unless it is `testnet`, `testnet4`, `signet`, `regtest`, or
  `simnet`; `lnd.expected_network` pins one. `lnbc` invoices are refused
  outright, and LND rejects invoices for other networks.
- **Ledger**: a balance is the sum of its ledger entries. SQLite triggers
  reject updates, deletes, and any entry that would make a balance negative;
  every entry has a unique idempotency key; all writes go through one
  connection in `BEGIN IMMEDIATE` transactions. An invoice credits one
  account once, whether it is paid over Lightning or internally.
- **Payments**: a payment whose outcome is unknown stays pending until
  reconciliation reads the result from LND; it is refunded only when LND
  reports a failure or says the payment never started. Forms carry a request
  key, so a repeated submission returns the first result instead of paying
  twice.
- **Web**: `__Host-` session cookies, `SameSite=Lax`, an origin check and a
  CSRF token on every state change, and a strict Content-Security-Policy.
- **Paying other servers**: Lightning Address lookups refuse private
  addresses, pin the connection to the checked address, follow no
  redirects, and check the returned invoice's amount and description hash.

Run exactly one instance per database. Not in scope: fund recovery, several
LND nodes, mainnet hardening, or an audit. Treat every balance as an IOU from
the operator.

## Development

`nix develop` provides Rust, `pkg-config`, and OpenSSL. Then:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The tests use SQLite and an in-memory LND, so no node is needed. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the workflow and
[SECURITY.md](SECURITY.md) for reporting vulnerabilities. Changes are listed
in [CHANGELOG.md](CHANGELOG.md).

## Credits

Satchel is a fork of [Koerier](https://github.com/tee8z/koerier), a Lightning
Address server for LND, itself a fork of
[luisschwab/koerier](https://github.com/luisschwab/koerier) by Luis Schwab;
the history is kept. 5day4cast uses Satchel for its Mutinynet testing.

## License

MIT OR Apache-2.0, at your option, as in Koerier. See
[LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE). htmx is
vendored under its own BSD Zero Clause license in `assets/vendor/htmx`.
