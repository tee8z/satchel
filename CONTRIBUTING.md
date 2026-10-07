# Contributing to Satchel

Thanks for helping. Satchel is a small, focused project: a multi-account
Lightning wallet and Lightning Address server for **test networks only**.
Changes that make it easier to run on test networks, easier to integrate
with, or safer are welcome. Changes aimed at mainnet use are not.

For security problems, follow [SECURITY.md](SECURITY.md) instead of opening
an issue.

## Development setup

With Nix:

```sh
nix develop   # Rust, rustfmt, clippy, pkg-config, OpenSSL
```

Without Nix, install Rust 1.95 or later (rustup), `pkg-config`, and the
OpenSSL development headers (`libssl-dev` on Debian and Ubuntu,
`openssl-devel` on Fedora).

The [`justfile`](justfile) has shortcuts: `just check` (fmt and clippy),
`just test`, `just audit`, and `just pre-push` for all of them.

## Tests and checks

CI runs these on every pull request; run them before you push:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The tests need no LND node: they run against SQLite and an in-memory LND
(`src/tests/mock.rs`) and cover the ledger, payments, the LNURL endpoints,
sign-in, and the web flows. A change that touches money (the ledger,
payments, invoices, the faucet) needs a test that shows balances stay
correct, including on retries and failures.

CI also runs `nix build .#satchel`, `nix flake check` (which checks the
NixOS module), and `cargo audit`.

To click through a change by hand, run the
[regtest example](examples/regtest) and point a local build at its LND
node. On Linux the host can reach the containers' addresses directly:

```sh
cd examples/regtest && ./setup.sh && docker compose stop satchel && cd ../..
cat > /tmp/satchel-dev.toml <<'TOML'
[server]
bind_address = "127.0.0.1:8095"
public_url = "http://127.0.0.1:8095"
database_path = "/tmp/satchel-dev.db"
admin_password_hash_file = "admin-password.hash"

[lnd]
rest_host = "172.29.42.11:8080"
tls_cert_path = "tls.cert"
macaroon_path = "satchel.macaroon"
expected_network = "regtest"

[faucet]
enabled = true
TOML
cp examples/regtest/satchel/{tls.cert,satchel.macaroon,admin-password.hash} /tmp/
cargo run -- --config /tmp/satchel-dev.toml
```

## Code style

- `rustfmt` with the repository's [`rustfmt.toml`](rustfmt.toml), and no
  clippy warnings.
- Pages are server-rendered with maud and htmx. Every form must work
  without JavaScript; scripts only add to it. No inline scripts or styles:
  the Content-Security-Policy forbids them.
- Keep dependencies few, and explain any new one in the pull request.
- The ledger is append-only: no updates or deletes, an idempotency key on
  every entry, and every write in one `BEGIN IMMEDIATE` transaction.
- New configuration keys get a default, validation, an entry in
  [docs/configuration.md](docs/configuration.md), and, if they belong in the
  example, in [example/config.toml.example](example/config.toml.example).
- User-facing text is plain and short. Never suggest that balances have
  value or that the wallet works on mainnet.

## Commits and pull requests

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):

```text
feat(web): show the invoice expiry on the receive page
fix(ledger): refund the fee budget when LND reports no route
docs: explain the operator origin
```

Common types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `build`,
`ci`, `chore`. Use the imperative mood, keep the subject under about 72
characters, and explain the why in the body when it is not obvious.

Open pull requests against `master`. Describe what changed and how you
tested it, add a line to the `Unreleased` section of
[CHANGELOG.md](CHANGELOG.md) for anything users or operators notice, and
keep one topic per pull request. Open it as a draft until CI passes.

## License

By contributing, you agree that your contributions are licensed under
either of the [MIT license](LICENSE-MIT) or the
[Apache License, Version 2.0](LICENSE-APACHE), at the user's option, without
any additional terms or conditions.
