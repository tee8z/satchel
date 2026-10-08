# Changelog

Notable changes to Satchel. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
rename configuration keys; the entry says so when it does.

## [Unreleased]

## [0.3.1] - 2026-10-08

- Distinguish idle invoice subscriptions awaiting a response from disconnected streams.
- Report reconnection counts and reconciliation freshness, including partial failures.
- Document alerts that detect failed reconciliation without alarming on quiet invoice streams.

## [0.3.0] - 2026-10-07

- Open to a compact wallet overview with recent activity and clear Send and Receive actions.
- Move payment forms to dedicated pages, including the full flow without JavaScript.
- Collapse the test-sat faucet until needed.
- Give the wallet, app icons and browser chrome a charcoal-and-amber palette.

## [0.2.0] - 2026-10-07

- Refresh the wallet with a dark layout, a prominent balance, and keyboard-accessible Send/Receive tabs.
- Review the recipient, amount, maximum fee, and total before confirming a payment.
- Keep the payment review and edit flow usable without JavaScript.
- Explain unsupported payment formats and how to replace expired invoices.
- Add `server.network_name` for names such as Mutinynet without changing network validation.
- Add `server.recovery_url` for a separately hosted entry-recovery tool.


## [0.1.0] - not yet released

The first release of Satchel, forked from Koerier 1.2.2: a multi-account
Lightning wallet and Lightning Address server for test networks only.

### Wallet

- Accounts with a username and password (argon2id), or a Nostr key through a
  NIP-07 signer extension; one account can have both.
- A Lightning Address for every account (LUD-06, LUD-12 comments, LUD-16)
  and an LNURL QR code.
- Send to BOLT11 invoices, Lightning Addresses, and LNURLs, with a
  routing-fee budget held while a payment is in flight and the unused part
  returned.
- Receive with invoices and QR codes; invoices are credited once, from LND's
  invoice stream, with reconciliation at startup and every minute.
- Internal transfers between accounts on the same server, settled in the
  ledger without touching Lightning.
- An optional faucet with per-account, per-client-address, and global daily
  caps, never more than the node's channel balance covers.
- QR scanning on the Send form.
- Installable as a web app: a manifest, an SVG icon, PNG icons (192, 512,
  maskable), and an apple-touch-icon.
- A mobile-first stylesheet with dark mode and a visible test-network badge.

### For integrators

- Deep links: `/launch/lightning/{invoice}` and
  `/launch/lightning/{name@host}` open the Send form filled in; the user
  confirms the payment.
- "Open Satchel" handoff sign-in: an app posts a NIP-98 style signed Nostr
  event to `/auth/nostr/handoff`, and Satchel signs the user in, asks them
  to confirm, or offers to create a wallet. Trusted apps are listed in
  `server.handoff_origins`.
- An address lookup API, `GET /api/v1/address`, authenticated with a NIP-98
  signed request, with CORS for `server.handoff_origins`.

### Running in public

- A proof of work for every new account, solved in the browser in the
  background, with a global difficulty that rises with the sign-up rate
  (`[pow]`).
- Rate limits sized for crowds behind one NAT, IPv6 grouped by prefix, and
  global caps on account creation and open invoices.
- Operator IP blocks (IPv4 and IPv6 ranges, with a reason and optional
  expiry) and a view of the busiest client addresses.
- Frozen accounts cannot sign in, send, receive, or use the faucet.

### Operating

- Refuses to start unless LND reports a test network (`testnet`,
  `testnet4`, `signet`, `regtest`, `simnet`); `lnd.expected_network` pins
  one. `lnbc` invoices are always refused.
- An append-only SQLite ledger: triggers reject updates, deletes, and
  negative balances, and every entry has an idempotency key.
- An operator page with its own password and optionally its own origin
  (`server.operator_url`): accounts, balances, liabilities against the
  node's balances, freeze, credit, and IP blocks.
- Prometheus metrics (totals only) and `/healthz`, optionally on a private
  listener.
- Configuration from one TOML file, with `SATCHEL_<SECTION>__<KEY>`
  environment overrides.

### Packaging

- Release archives for x86_64 and aarch64 Linux with OpenSSL linked in, and
  SHA-256 checksums.
- A container image, `ghcr.io/tee8z/satchel`, for `linux/amd64` and
  `linux/arm64`, built from the release archives and published with each
  release.
- A Nix flake with a package and a NixOS module (`services.satchel`).
- A regtest Docker Compose example (`examples/regtest`) with bitcoind, two
  LND nodes, and Satchel.
- Documentation for operating, configuration, Docker, integrating, and
  abuse protection under `docs/`.

[Unreleased]: https://github.com/tee8z/satchel/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/tee8z/satchel/compare/v0.3.0...v0.3.1
[0.1.0]: https://github.com/tee8z/satchel/releases/tag/v0.1.0

[0.2.0]: https://github.com/tee8z/satchel/compare/v0.1.0...v0.2.0
