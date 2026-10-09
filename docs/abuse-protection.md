# Abuse protection

Satchel is a public, custodial wallet for **test networks only**. Anyone can
reach it, so it can be abused: scripted sign-ups, faucet draining, invoice
spam against the Lightning node. This page explains how Satchel limits that
and how an operator blocks networks, without locking out a crowd.

## The design: crowd-safe limits

At a conference, workshop or office, a few hundred people often share one
public IPv4 address (a NAT) or one IPv6 prefix. A limit of "5 sign-ups per
address per hour" would lock most of that room out. So Satchel sizes its
limits in three layers:

| Layer | Sized for | Examples |
| --- | --- | --- |
| Per client address | A few hundred people behind one NAT | 300 sign-ups per hour, 600 LNURL requests per minute, 250,000 faucet sats per day |
| Per account or identity | One person | 30 password attempts per hour, 10 sends per minute, 20,000 faucet sats per day |
| Global | The whole service | 1,000 new accounts per hour, 500,000 faucet sats per day |

Per-address limits stop one abusive machine from doing unbounded damage
without punishing everyone it shares an address with. Per-account limits stay
tight because one person never needs more. Global caps bound the total, no
matter how many addresses an attacker has.

Proof of work adds a cost to every new account. It is the same for everyone,
including a crowd behind one address: difficulty depends only on how many
accounts the whole service created recently.

When a network is clearly abusive, the operator blocks it (see
[Blocking networks](#blocking-networks)).

## Client addresses

Limits and blocks need the real client address. Behind a reverse proxy, set
`server.client_ip_header` to the header the proxy fills (for example
`x-forwarded-for`; the right-most entry is used). Without it, every request
seems to come from the proxy.

Limits count per **client key**:

- IPv4: the full address. One IPv4 address is usually one NAT already.
- IPv6: the prefix of `rate_limits.ipv6_prefix_len` bits (default 56). One
  household or site usually holds a whole /56 or /64, so counting single IPv6
  addresses would let one client rotate through billions of them.
- IPv4-mapped IPv6 addresses (`::ffff:203.0.113.7`) count as IPv4.

The client key is what the operator page shows, for example `203.0.113.7` or
`2001:db8:1:200::/56`.

The rate limiter keeps its windows in memory (up to 100,000 keys). When the
table is full it first drops windows that have ended, then the oldest quarter.
It never refuses everyone because the table is full; at worst an attacker with
many keys makes other keys' windows shorter.

## Proof of work for new accounts

Every way of creating an account needs a solved proof of work: the password
sign-up form, the Nostr sign-up button, and any other page that posts to the
sign-up handler.

How it works:

1. The sign-up page loads and its script asks `POST /auth/pow` for a challenge:
   `{"challenge": "<base64url>", "difficulty": <bits>, "expires_at": <unix seconds>}`.
2. A Web Worker (`assets/pow-worker.js`, with the small pure-JavaScript
   SHA-256 in `assets/sha256.js`) searches for a nonce so that
   SHA-256(challenge bytes ‖ nonce as a 64-bit big-endian number) starts with
   `difficulty` zero bits. This usually finishes before the person has typed a
   username and password; if not, the button shows "Preparing…" until it does.
3. The form sends `pow_challenge` and `pow_nonce` (decimal) with the sign-up.
4. The server checks, with a single hash, that the challenge carries its own
   HMAC, has not expired, asks for at least the difficulty required now, is
   solved, and has not been used before.

Challenges are stateless: 16 random bytes, the expiry, the difficulty, and an
HMAC with a secret generated when the process starts. They live 10 minutes.
Used challenges are remembered until they expire. A restart invalidates
challenges being solved; the page simply fetches a new one.

Cheap checks run first (username rules, password rules, a taken name, the
global cap), so a typo never wastes a solved challenge, and the proof of work
is spent before the expensive password hash. A sign-up turned away because
every password worker is busy keeps its challenge too (see
[Password worker budget](configuration.md#password-worker-budget)).

### Difficulty

Difficulty is global and adapts to load:

```text
bits = min(base_bits + accounts_created_in_the_last_hour / step_signups, max_bits)
```

With the defaults (18 bits, one more per 200 accounts, at most 22), a quiet
service asks for about a second of work on a phone; after 800 sign-ups in an
hour it asks for about 16 times as much. A crowd behind one address pays
exactly what everyone else pays.

When difficulty rises between issuing a challenge and using it, the sign-up is
refused with "the sign-up check got harder" and the page solves a new one.
This happens only to sign-ups in flight at the moment a step is crossed.

With `pow.enabled = false`, challenges ask for zero bits and the server does
not check them, so sign-up also works without JavaScript. With proof of work
on, sign-up needs JavaScript.

Proof of work raises the cost of mass sign-ups; it does not stop a determined
attacker with fast native code. The per-address and global caps and the
per-address faucet cap are what bound the damage.

## Blocking networks

The operator page (`/admin`) has a **Blocked networks** section:

- **Add a block:** a network in CIDR notation (`203.0.113.0/24`,
  `2001:db8:1::/56`) or a single address, a reason, and an optional expiry in
  hours (empty means never). Blocks wider than /8 (IPv4) or /16 (IPv6) are
  refused so a typo cannot block a continent. Blocking the same network again
  replaces its reason and expiry.
- **Remove a block:** the Remove button next to it.

Blocks are stored in SQLite (`blocks` table) and cached in memory; the cache is
reloaded after every change, at startup, and when expired blocks are cleaned
up (every minute).

A blocked client gets:

- an HTML page with status 403 on every page and form;
- `{"status":"ERROR","reason":"Requests from your network are blocked"}` on
  the LNURL endpoints (`/.well-known/lnurlp/*` and `/lnurlp/*`), as LUD-06
  wallets expect.

The reason is for operators only and is never shown to the blocked client.
The operator pages, static files under `/assets/` and `/healthz` stay
reachable from blocked networks, so an operator can never lock themselves out.

### Busiest addresses (24 h)

Below the blocks, the operator page lists the busiest client keys of the last
24 hours for three kinds of activity:

- **Sign-ups:** accounts created.
- **Faucet claims:** grants and the sats they paid.
- **LNURL invoices:** invoices requested through Lightning Addresses (the
  client is the payer's wallet or wallet server).

Each row shows the number of events, the distinct accounts involved, and when
it last happened, plus a one-click **Block** button for the row's /24 (IPv4)
or /56 (IPv6). One-click blocks expire after 24 hours, because a busy address
is often a legitimate crowd that will be gone tomorrow. Use the form for
permanent blocks.

The account list also shows the client key each account signed up from, and
the search box matches it, so you can find every account created from one
network.

## Freezing accounts

The operator's **Freeze** button stops an account completely. A frozen
account:

- cannot sign in with a password, with Nostr, or with a handoff from another
  app, and its existing sessions stop working (freezing also deletes them);
- cannot receive: its Lightning Address answers with an LNURL error, it cannot
  create invoices, other accounts cannot pay it internally, and freezing
  cancels its unpaid invoices in LND;
- cannot send or claim the faucet.

An invoice LND settles anyway (a payment already in flight when the account was
frozen) is still credited, because those sats reached the node. **Unfreeze**
restores everything except the canceled invoices.

## Faucet caps

Faucet grants are limited three ways over a rolling 24 hours, all checked in
the same database transaction as the grant:

- `faucet.per_account_daily_sat`: per account;
- `faucet.per_address_daily_sat`: per client key, shared by everyone behind
  one address, so many accounts from one machine cannot drain the faucet;
- `faucet.global_daily_sat`: for the whole service.

For an event, raise `per_address_daily_sat` and `global_daily_sat` together.
Size the per-address cap as the most you are willing to hand to one venue's
network in a day.

## Open invoices

`limits.max_open_invoices` caps an account's unpaid, unexpired invoices. It is
counted separately for invoices the owner creates on the wallet page and for
invoices payers request through the owner's Lightning Address, so strangers
can never use up the owner's own allowance. A paid, canceled or expired
invoice frees its place. LNURL requests over the cap answer
`{"status":"ERROR","reason":"This address has too many unpaid invoices; retry later"}`.

## Configuration reference

Every key is optional; these are the defaults.

### `[rate_limits]`

| Key | Default | Scope | Meaning |
| --- | --- | --- | --- |
| `login_per_ip_per_minute` | `120` | address | Password and operator logins, and Nostr challenges |
| `login_per_account_per_hour` | `30` | account | Password attempts (login and password change) per username |
| `signup_per_ip_per_hour` | `300` | address | Sign-up attempts |
| `signups_global_per_hour` | `1000` | global | Accounts created in the last hour, by every path |
| `lnurl_per_ip_per_minute` | `600` | address | LNURL discovery and callback requests |
| `lnurl_per_account_per_minute` | `30` | account | LNURL invoices for one address |
| `send_per_account_per_minute` | `10` | account | Payments |
| `receive_per_account_per_minute` | `20` | account | Invoices from the wallet page |
| `ipv6_prefix_len` | `56` | | IPv6 clients count per prefix of this many bits (16 to 128) |

Why these per-address numbers: a room of 300 people signing up within an hour
needs 300 sign-ups; logging in over a few minutes needs about 100 logins a
minute; paying one speaker's Lightning Address at once needs two LNURL
requests per payer. Lower them for a private deployment.

### `[pow]`

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Require proof of work for new accounts |
| `base_bits` | `18` | Leading zero bits when sign-ups are quiet (about a second on a phone) |
| `max_bits` | `22` | Upper bound (at most 32) |
| `step_signups` | `200` | One more bit per this many accounts created in the last hour |

### `[faucet]` (abuse-related keys)

| Key | Default | Meaning |
| --- | --- | --- |
| `per_account_daily_sat` | `20000` | Per account per 24 hours |
| `per_address_daily_sat` | `250000` | Per client key per 24 hours; at least `amount_sat` |
| `global_daily_sat` | `500000` | Whole service per 24 hours |

### `[limits]` (abuse-related keys)

| Key | Default | Meaning |
| --- | --- | --- |
| `max_open_invoices` | `100` | Unpaid invoices per account, per source (wallet page, Lightning Address) |

Every key can also be set with an environment variable, for example
`SATCHEL_POW__BASE_BITS=20` or `SATCHEL_RATE_LIMITS__SIGNUP_PER_IP_PER_HOUR=50`.
With the NixOS module, use the `pow`, `rateLimits`, `faucet` and `limits`
options, which pass their attributes through to the TOML tables.

## Metrics

On the private metrics listener (`server.metrics_address`), next to the
existing totals:

| Metric | Type | Meaning |
| --- | --- | --- |
| `satchel_rate_limited_total{scope}` | counter | Requests refused by a limit, by scope: `login-ip`, `login-account`, `operator-login`, `nostr`, `signup`, `signup-global`, `lnurl-ip`, `lnurl-account`, `send`, `receive` |
| `satchel_blocked_requests_total` | counter | Requests refused by operator blocks |
| `satchel_pow_checks_total{outcome}` | counter | Proof-of-work solutions checked, `verified` or `rejected` |
| `satchel_pow_difficulty_bits` | gauge | Bits a new account needs now (refreshed every minute and on each challenge) |
| `satchel_faucet_paid_msat_total` | counter | Faucet sats paid since start, in msat |
| `satchel_signups_total` | counter | Accounts created since start |
| `satchel_faucet_last_day_msat` | gauge | Faucet sats paid in the last 24 hours |

Counters reset when the process restarts. A sudden rise in
`satchel_pow_checks_total{outcome="rejected"}` or in
`satchel_rate_limited_total{scope="signup"}` usually means scripted sign-ups;
check the busiest addresses on the operator page.
