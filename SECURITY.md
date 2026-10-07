# Security policy

Satchel is a custodial Lightning wallet for **test networks only**
(Mutinynet, signet, testnet, regtest). It refuses to start on mainnet, holds
every balance on the operator's LND node, and has not been audited. Balances
are test sats with no value, but operators and the apps that rely on
Satchel still depend on it behaving correctly, so security reports are
welcome.

## Reporting a vulnerability

Report privately through GitHub: open the repository's **Security** tab and
choose **Report a vulnerability**
(<https://github.com/tee8z/satchel/security/advisories/new>). Please do not
open a public issue or pull request for a vulnerability.

Include what you found, how to reproduce it (a request sequence, a test, or
a regtest setup such as [`examples/regtest`](examples/regtest)), the release
or commit you tested, and the impact you expect. You should get a first
answer within a week. Fixes land in a new release; the advisory is published
after the release, with credit if you want it.

Only the latest release is supported.

## In scope

Bugs in Satchel itself that let someone:

- spend, receive, or claim more than the ledger allows, take another
  account's balance, or make the server pay twice;
- sign in as another account, link a Nostr key to someone else's account,
  bypass the CSRF and origin checks, or reach the operator pages without the
  operator password;
- read other accounts' data, sessions, or credentials;
- run Satchel against a mainnet node despite the network guard, or get it to
  pay a mainnet invoice;
- make the server send requests to private or internal addresses through
  LNURL or Lightning Address lookups (unless `allow_private_lnurl_hosts` is
  on);
- inject script or markup into pages (XSS) or get around the
  Content-Security-Policy;
- bypass the proof of work, rate limits, IP blocks, or account freezes in a
  way the documentation says is not possible;
- take the service down with little effort (a single cheap request that
  exhausts memory, CPU, or the database).

## Out of scope

- The custodial model itself: the operator can read the database and spend
  the node's funds. That is by design and documented.
- Loss of test sats from operator mistakes, a lost database, or a lost LND
  node; Satchel has no fund recovery.
- Attacks that need control of the operator's host, LND node, reverse
  proxy, or configuration.
- Limits that are generous because the operator configured them that way.
- Volumetric denial of service, which belongs at the network edge.
- Reports from automated scanners without a demonstrated impact, missing
  headers with no exploit, and social engineering.

Never test against a Satchel server you do not run without its operator's
permission. Use a local regtest setup instead.
