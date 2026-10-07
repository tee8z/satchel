# Configuration

Satchel reads one TOML file, given with `--config <path>` or the
`SATCHEL_CONFIG` environment variable. Unknown keys are refused, so a typo
stops the server at startup instead of being ignored. Amounts are in sats;
Satchel works in millisats internally, and every amount must stay below
2^53 msat.

[`example/config.toml.example`](../example/config.toml.example) is a complete,
commented file for Mutinynet.

## Environment overrides

Any key can be set or overridden from the environment as
`SATCHEL_<SECTION>__<KEY>` (two underscores between section and key; case
does not matter):

```sh
SATCHEL_SERVER__PUBLIC_URL=https://wallet.example.org
SATCHEL_FAUCET__ENABLED=false
SATCHEL_LIMITS__MAX_BALANCE_SAT=50_000
SATCHEL_SERVER__RESERVED_USERNAMES='["support", "team"]'
```

Values are read as TOML (numbers, booleans, arrays) and otherwise as plain
strings, and they are validated like the file. Other variables:

| Variable | Meaning |
| --- | --- |
| `SATCHEL_CONFIG` | Path to the configuration file. |
| `SATCHEL_ADMIN_PASSWORD_HASH` | The operator's argon2id hash, instead of `server.admin_password_hash_file`. |
| `SATCHEL_LOG_JSON` | `true` writes logs as JSON lines. |
| `RUST_LOG` | Log filter, for example `info` (the default) or `satchel=debug,info`. |
| `CREDENTIALS_DIRECTORY` | Set by systemd `LoadCredential`; relative credential paths resolve here. |

## Credential paths

`server.admin_password_hash_file`, `lnd.tls_cert_path`, and
`lnd.macaroon_path` may be relative. They resolve under
`CREDENTIALS_DIRECTORY` when systemd provides it, and otherwise in the
directory that holds the configuration file. `server.database_path` is used
as written.

## `[server]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `bind_address` | socket address | required | Private HTTP listener, for example `127.0.0.1:8095`. Put an HTTPS reverse proxy in front of it. |
| `public_url` | URL | required | The public origin, for example `https://wallet.example.org`. Lightning Addresses are `<username>@<host>` (with the port, if the URL has one). Must be HTTPS without a path, query, fragment, or credentials; plain `http://` is accepted only for `localhost`, `127.0.0.1`, and `[::1]`, for local testing. |
| `database_path` | path | required | The SQLite database. Created on first start; WAL files sit beside it. |
| `operator_url` | URL | none | A separate origin for the operator pages, for example a name reachable only over your VPN. When set, `/admin` answers only on this host and returns 404 on the public one. Same rules as `public_url`. See [operating.md](operating.md#the-operator-origin). |
| `metrics_address` | socket address | none | A second private listener for `/metrics` and `/healthz`. Without it, `/healthz` is on the main listener and `/metrics` is not served. |
| `client_ip_header` | string | none | The header your reverse proxy sets to the client address, for example `x-real-ip`. If the header holds a list, the right-most entry is used. Without it, rate limits see the proxy's address. Set it only when a proxy you control always overwrites the header. |
| `admin_password_hash_file` | path | none | File with the operator's argon2id hash from `satchel hash-password`. `SATCHEL_ADMIN_PASSWORD_HASH` overrides it. With neither, the operator pages are off. |
| `reserved_usernames` | list of strings | `[]` | Usernames nobody may register, on top of the built-in list (`admin`, `abuse`, `api`, `faucet`, and other operator and mailbox names). |
| `session_days` | integer | `14` | How long a login lasts. |
| `allow_private_lnurl_hosts` | boolean | `false` | Allow paying Lightning Addresses and LNURLs on loopback and private addresses. Only for local regtest setups, where the other wallet is on your machine. |
| `handoff_origins` | list of URLs | `[]` | *Added in v0.1.0.* Origins of apps you trust to send users here with "Open Satchel". A handoff from one of these signs a known user in with one click; from anywhere else the user confirms first. The same origins may call `/api/v1/address` from the browser. They never skip the proof of work. See [integrating.md](integrating.md). |

Usernames are 3 to 32 characters of `a-z`, `0-9`, `.`, `_`, and `-`,
starting and ending with a letter or digit. Passwords need at least 10
characters.

## `[lnd]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `rest_host` | socket address | required | LND's REST listener as `IP:port`, for example `127.0.0.1:8080`. A host name is not accepted. LND's TLS certificate must cover this IP address (see `tlsextraip` in [operating.md](operating.md#lnd)). |
| `tls_cert_path` | path | required | LND's `tls.cert`. It is the only trusted certificate for the connection. |
| `macaroon_path` | path | required | A macaroon with `info:read invoices:read invoices:write offchain:read offchain:write onchain:read`. See [operating.md](operating.md#the-macaroon). |
| `request_timeout_secs` | integer | `10` | Deadline for one LND request. |
| `payment_timeout_secs` | integer | `60` | How long LND may try to route an outgoing payment. |
| `expected_network` | string | none | Refuse to start unless LND reports exactly this network: `testnet`, `testnet4`, `signet`, `regtest`, or `simnet`. Mutinynet reports `signet`. Mainnet is refused whatever this says. |

## `[limits]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `max_balance_sat` | integer | `1000000` | Largest balance an account can reach by receiving. Checked when an invoice is created (and in the LNURL `maxSendable`); a payment that arrives for an existing invoice is always credited. |
| `max_payment_sat` | integer | `250000` | Largest single outgoing payment, Lightning or internal. |
| `min_receive_sat` | integer | `1` | Smallest invoice amount. |
| `max_receive_sat` | integer | `250000` | Largest invoice amount. Must be at least `min_receive_sat`. |
| `invoice_expiry_secs` | integer | `3600` | How long an invoice can be paid. |
| `fee_limit_ppm` | integer | `10000` | Routing-fee budget for an outgoing payment, in parts per million of the amount (10000 is 1%). At most 1000000. |
| `min_fee_limit_sat` | integer | `10` | The routing-fee budget is never smaller than this. |

The amount plus the fee budget is held while a payment is in flight; the
unused part of the budget comes back when LND reports the result.

## `[faucet]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | boolean | `false` | Show the faucet on the wallet page. |
| `amount_sat` | integer | `10000` | Sats per grant. |
| `per_account_daily_sat` | integer | `20000` | Most one account can receive from the faucet in any 24 hours. |
| `global_daily_sat` | integer | `500000` | Most the faucet gives out in any 24 hours, across all accounts. |

When enabled, the amounts must satisfy
`0 < amount_sat <= per_account_daily_sat <= global_daily_sat`. Grants are
IOUs on the node's channel balance: a grant is refused when all balances
together would exceed the node's local channel balance. *Added in v0.1.0:*
a per-client-address daily cap, sized for many people behind one NAT; the
exact key and default are listed in [abuse-protection.md](abuse-protection.md).

## `[rate_limits]`

Attempts allowed in each window, counted in memory per client address (IPv4
address, or IPv6 prefix) or per account. A refused request gets an error
asking to retry later and increments `satchel_rate_limited_total`.

| Key | Type | Default | Applies to |
| --- | --- | --- | --- |
| `login_per_ip_per_minute` | integer | `10` | Password logins, Nostr sign-in, and operator logins, per client address. |
| `login_per_account_per_hour` | integer | `30` | Password attempts per username, including password changes. |
| `signup_per_ip_per_hour` | integer | `5` | New accounts per client address, by password or Nostr. Raise it before an event where many people share one network. |
| `lnurl_per_ip_per_minute` | integer | `60` | LNURL-pay requests (`/.well-known/lnurlp/…` and callbacks) per client address. |
| `lnurl_per_account_per_minute` | integer | `30` | LNURL invoices per receiving account. |
| `send_per_account_per_minute` | integer | `10` | Send attempts per account. |
| `receive_per_account_per_minute` | integer | `20` | Invoices created from the wallet page per account. |

*Added in v0.1.0:* IPv6 addresses are grouped by a configurable prefix
(default /56), the per-address defaults are sized for a conference crowd
behind one NAT, and global caps bound account creations per hour and open
invoices per account. The keys and defaults are listed in
[abuse-protection.md](abuse-protection.md).

## `[pow]`

*Added in v0.1.0.* Every new account, by any sign-up path, must include a
proof of work that the browser solves in the background while the sign-up
page is open. The difficulty is global, never per address, so a crowd
behind one address pays the same as anyone.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | boolean | `true` | Require the proof of work. |
| `base_bits` | integer | `18` | Leading zero bits required when sign-ups are quiet; about a second on a phone. |
| `max_bits` | integer | `22` | The most the difficulty can rise to. |
| `step_signups` | integer | `200` | One more bit for every this many accounts created in the last hour across the whole service. |

How it works and how to tune it: [abuse-protection.md](abuse-protection.md).

## NixOS module options

`services.satchel` maps onto the same keys: `listenAddress`
(`server.bind_address`), `publicUrl`, `operatorUrl`, `metricsAddress`,
`clientIpHeader`, `adminPasswordHashFile`, `reservedUsernames`,
`sessionDays`, `allowPrivateLnurlHosts`, and `lnd.{restHost, tlsCertPath,
macaroonPath, expectedNetwork, requestTimeoutSecs, paymentTimeoutSecs}`. The
`limits`, `faucet`, and `rateLimits` options take the TOML tables as
attribute sets with the key names above. The database is always
`/var/lib/satchel/wallet.db`, and credential files are passed with systemd
`LoadCredential`.
