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
| `network_name` | string | LND network | Name shown on wallet pages, such as `Mutinynet`. It does not change invoice or startup network checks. |
| `recovery_url` | URL or local path | none | Link to an independently hosted entry-recovery tool, such as `/recover/`. It does not recover Satchel balances. |
| `operator_url` | URL | none | A separate origin for the operator pages, for example a name reachable only over your VPN. When set, `/admin` answers only on this host and returns 404 on the public one. Same rules as `public_url`. See [operating.md](operating.md#the-operator-origin). |
| `metrics_address` | socket address | none | A second private listener for `/metrics` and `/healthz`. Without it, `/healthz` is on the main listener and `/metrics` is not served. |
| `client_ip_header` | string | none | The header your reverse proxy sets to the client address, for example `x-real-ip`. If the header holds a list, the right-most entry is used. Without it, rate limits see the proxy's address. Set it only when a proxy you control always overwrites the header. |
| `admin_password_hash_file` | path | none | File with the operator's argon2id hash from `satchel hash-password`. `SATCHEL_ADMIN_PASSWORD_HASH` overrides it. With neither, the operator pages are off. |
| `reserved_usernames` | list of strings | `[]` | Usernames nobody may register, on top of the built-in list (`admin`, `abuse`, `api`, `faucet`, and other operator and mailbox names). |
| `session_days` | integer | `14` | How long a login lasts. |
| `allow_private_lnurl_hosts` | boolean | `false` | Allow paying Lightning Addresses and LNURLs on loopback and private addresses. Only for local regtest setups, where the other wallet is on your machine. |
| `handoff_origins` | list of URLs | `[]` | *Added in v0.1.0.* HTTPS origins (no path) of apps you trust to send users here with "Open Satchel" (`POST /auth/nostr/handoff`). A handoff from one of these signs a known user in directly; from anywhere else the user confirms with one more click. The same origins may call `GET /api/v1/address` from the browser (CORS). They never skip the proof of work. See [integrating.md](integrating.md). |

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
| `max_open_invoices` | integer | `100` | *Added in v0.1.0.* Unpaid, unexpired invoices one account may hold, counted separately for invoices the owner creates and invoices payers request through the Lightning Address. Must be positive. |

The amount plus the fee budget is held while a payment is in flight; the
unused part of the budget comes back when LND reports the result.

## `[faucet]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | boolean | `false` | Show the faucet on the wallet page. |
| `amount_sat` | integer | `10000` | Sats per grant. |
| `per_account_daily_sat` | integer | `20000` | Most one account can receive from the faucet in any 24 hours. |
| `per_address_daily_sat` | integer | `250000` | *Added in v0.1.0.* Most the faucet gives to one client address (IPv4 address or IPv6 prefix) in any 24 hours. Everyone behind one NAT shares it, so size it for a crowd but below the global cap. |
| `global_daily_sat` | integer | `500000` | Most the faucet gives out in any 24 hours, across all accounts. |

When enabled, the amounts must satisfy
`0 < amount_sat <= per_account_daily_sat <= global_daily_sat` and
`amount_sat <= per_address_daily_sat`. Grants are IOUs on the node's channel
balance: a grant is refused when all balances together would exceed the
node's local channel balance.

## `[rate_limits]`

Attempts allowed in each window, counted in memory per client address (IPv4
address, or IPv6 prefix) or per account. A refused request gets an error
asking to retry later and increments `satchel_rate_limited_total`.

| Key | Type | Default | Applies to |
| --- | --- | --- | --- |
| `login_per_ip_per_minute` | integer | `120` | Password logins, Nostr sign-in challenges, and operator logins, per client address. |
| `login_per_account_per_hour` | integer | `30` | Password attempts per username, including password changes. |
| `signup_per_ip_per_hour` | integer | `300` | Sign-up attempts per client address, by any path. |
| `signups_global_per_hour` | integer | `1000` | *Added in v0.1.0.* Accounts created in the last hour across the whole service, by any path. |
| `lnurl_per_ip_per_minute` | integer | `600` | LNURL-pay requests (`/.well-known/lnurlp/…` and callbacks) per client address. |
| `lnurl_per_account_per_minute` | integer | `30` | LNURL invoices per receiving account. |
| `send_per_account_per_minute` | integer | `10` | Send attempts per account. |
| `receive_per_account_per_minute` | integer | `20` | Invoices created from the wallet page per account. |
| `ipv6_prefix_len` | integer | `56` | *Added in v0.1.0.* IPv6 clients count as one address per prefix of this many bits, 16 to 128. |

The per-address defaults are sized for a few hundred people behind one
conference NAT; per-account limits stay tight, and the global cap bounds the
total. For a private deployment, lower the per-address numbers. The
reasoning behind each number is in
[abuse-protection.md](abuse-protection.md#configuration-reference).

## `[pow]`

*Added in v0.1.0.* Every new account, by any sign-up path, must include a
proof of work that the browser solves in the background while the sign-up
page is open. The difficulty is global, never per address, so a crowd
behind one address pays the same as anyone.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | boolean | `true` | Require the proof of work. |
| `base_bits` | integer | `18` | Leading zero bits required when sign-ups are quiet; about a second on a phone. |
| `max_bits` | integer | `22` | The most the difficulty can rise to; at most 32 and at least `base_bits`. |
| `step_signups` | integer | `200` | One more bit for every this many accounts created in the last hour across the whole service. Must be positive. |

How it works and how to tune it: [abuse-protection.md](abuse-protection.md).

## NixOS module options

`services.satchel` maps onto the same keys: `listenAddress`
(`server.bind_address`), `publicUrl`, `networkName`, `recoveryUrl`, `operatorUrl`, `metricsAddress`,
`clientIpHeader`, `adminPasswordHashFile`, `reservedUsernames`,
`sessionDays`, `allowPrivateLnurlHosts`, `handoffOrigins`, and
`lnd.{restHost, tlsCertPath, macaroonPath, expectedNetwork,
requestTimeoutSecs, paymentTimeoutSecs}`. The `limits`, `faucet`,
`rateLimits`, and `pow` options take the TOML tables as attribute sets with
the key names above. The database is always
`/var/lib/satchel/wallet.db`, and credential files are passed with systemd
`LoadCredential`.

### Password worker budget

All HTTP password hashing and verification share two blocking workers, with no waiting queue.
This includes user login, operator login, signup, password changes, and dummy checks for unknown usernames.
When both workers are occupied, the form is shown again with a short busy message, HTTP 503 and `Retry-After: 1`.
A request turned away this way spends no login attempt, sign-up allowance or proof of work, so sending it again works.
A disconnected request keeps its worker occupied until the password operation finishes.
The fixed limit bounds ordinary Argon2 working memory to about 38 MiB; it does not cap total process memory.

Monitor `satchel_password_jobs` and `satchel_password_jobs_rejected_total` alongside service memory and request latency.
Per-IP and per-account rate limits still apply.
