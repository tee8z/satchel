# Integrating an app with Satchel

> **Test networks only.** Satchel is a custodial, unaudited wallet for
> Mutinynet, signet, testnet, and regtest. Everything below moves test sats.
> Never point a mainnet app at it.

Apps can work with Satchel in three ways:

1. **Deep links** open Satchel's Send screen with an invoice or a Lightning
   Address filled in.
2. **Handoff sign-in** ("Open Satchel") signs the user in to Satchel with the
   Nostr key your app already has, in one click, and can create their wallet.
3. **Address lookup** returns the Lightning Address of the wallet that uses a
   Nostr key, so you can show where payouts or refunds will go.

In the examples, `SATCHEL` is the wallet's public origin
(`https://wallet.example.org`) and your app runs at `https://app.example.org`.

## Operator setup

Deep links need no setup. For handoffs and address lookups from the browser,
the Satchel operator lists your app's origin:

```toml
[server]
handoff_origins = ["https://app.example.org"]
```

With the NixOS module:

```nix
services.satchel.handoffOrigins = [ "https://app.example.org" ];
```

Entries must be HTTPS origins, with no path. Plain HTTP is accepted only on
loopback, for local development. A listed origin gets two things:

- Its handoffs skip the confirmation page.
- Its pages may call the address API, which needs CORS.

Handoffs from other origins still work, but take one more click. The list
never skips a sign-up check.

## 1. Deep links

```
SATCHEL/launch/lightning/{bolt11}
SATCHEL/launch/lightning/{name@host}
```

The path is the same shape as Cash App's deep links, so one "Pay with…" link
format serves both. A `lightning:` prefix and any letter case are accepted.
Escape the target as one path segment:

```js
const link = `${SATCHEL}/launch/lightning/${encodeURIComponent(invoice)}`;
```

What the user sees:

- **Invoice:** the Send screen shows the invoice's amount, description, and
  expiry, and a **Pay** button. Nothing is paid until the user taps Pay.
  Invoices without an amount ask for one.
- **Lightning Address:** the Send screen shows the address. The user enters an
  amount (and an optional comment), then sends.
- **Signed out:** the user signs in with a password, a Nostr extension, or a
  handoff, then lands back on the same link.
- **Problems:** the Send form shows its usual error for an invalid or expired
  invoice. It does the same for an invoice from another network, judged by
  its prefix:

  | Satchel's node is on | Accepted invoice prefix |
  | -------------------- | ----------------------- |
  | signet (Mutinynet)   | `lntbs`                 |
  | testnet, testnet4    | `lntb`                  |
  | regtest              | `lnbcrt`                |
  | simnet               | `lnsb`                  |

  `lnbc` (mainnet) is always refused.

### `next`

Sign-in pages and handoffs take a `next` parameter: the page to open after
signing in. It must be a local path:

- one leading `/`, and no `//` anywhere;
- no `\`, spaces, or control characters, only printable ASCII;
- at most 2048 bytes.

Anything else is replaced by `/wallet`. A deep link such as
`/launch/lightning/lntbs1…` is a valid `next`.

## 2. Handoff sign-in ("Open Satchel")

Your page holds the user's Nostr key, either through a NIP-07 signer
(`window.nostr`) or as a key your app keeps in the browser. It signs a
[NIP-98](https://github.com/nostr-protocol/nips/blob/master/98.md) HTTP auth
event and posts it to Satchel in a top-level form. Satchel signs the user in
to the wallet that uses that key, or offers to create one.

### The event

```json
{
  "kind": 27235,
  "created_at": 1791400000,
  "content": "",
  "tags": [
    ["u", "https://wallet.example.org/auth/nostr/handoff"],
    ["method", "POST"],
    ["name", "alice"]
  ]
}
```

- `u` must be exactly `SATCHEL/auth/nostr/handoff`.
- `method` must be `POST`.
- `created_at` must be within 120 seconds of Satchel's clock.
- `name` is optional. It suggests a username for a new wallet. Satchel
  lowercases it and uses it only if it is a valid username that nobody has.
- Each event works once. Sign a fresh one for every click.

### The form post

Send a top-level `POST` (`application/x-www-form-urlencoded`) to
`SATCHEL/auth/nostr/handoff` with these fields:

| Field   | Value                                                                 |
| ------- | --------------------------------------------------------------------- |
| `event` | the signed event as JSON                                              |
| `next`  | optional local path to open afterwards, such as a deep link (default `/wallet`) |

Use a real form submission, not `fetch`. The browser has to navigate to
Satchel so that Satchel can set its own cookies.

To open Satchel in a new tab without the popup blocker stepping in, open a
named window synchronously inside the click handler, before any `await`.
Then sign, and submit the form into that window:

```js
const SATCHEL = "https://wallet.example.org";

async function openSatchel({ next = "/wallet", name } = {}) {
  // Must run synchronously in the click handler, before any await.
  const tab = window.open("", "satchel");
  try {
    const tags = [
      ["u", `${SATCHEL}/auth/nostr/handoff`],
      ["method", "POST"],
    ];
    if (name) tags.push(["name", name]);
    const event = await window.nostr.signEvent({
      kind: 27235,
      created_at: Math.floor(Date.now() / 1000),
      content: "",
      tags,
    });
    const form = document.createElement("form");
    form.method = "POST";
    form.action = `${SATCHEL}/auth/nostr/handoff`;
    form.target = "satchel";
    for (const [field, value] of Object.entries({ event: JSON.stringify(event), next })) {
      const input = document.createElement("input");
      input.type = "hidden";
      input.name = field;
      input.value = value;
      form.append(input);
    }
    document.body.append(form);
    form.submit();
    form.remove();
  } catch (error) {
    if (tab) tab.close();
    throw error;
  }
}

button.addEventListener("click", () => {
  openSatchel({ next: `/launch/lightning/${encodeURIComponent(invoice)}`, name: "alice" }).catch(console.error);
});
```

If your app keeps the key itself instead of using a NIP-07 signer, sign the
same template with your Nostr library. For example, with nostr-tools:
`finalizeEvent(template, secretKey)`.

**Content Security Policy:** if your page sends a CSP, its `form-action` must
include the Satchel origin, for example
`form-action 'self' https://wallet.example.org`. Satchel redirects only within
its own origin afterwards, and browsers check those redirects against
`form-action` too.

### What Satchel checks

- The event id (NIP-01 serialization) and its BIP-340 signature.
- Kind 27235, `u` exactly equal to the handoff URL, `method` equal to `POST`,
  and `created_at` within ±120 s.
- That the event id has not been used before. Used ids are kept for 10
  minutes.

Requests are also rate-limited per client address.

### What happens next

| Situation | Result |
| --- | --- |
| The key belongs to a wallet, the post came from an origin in `handoff_origins`, and the browser is signed out of Satchel or already in that wallet | Signed in, then `next` opens. |
| The key belongs to a wallet, and the post came from another origin (or without an `Origin` header) | A "Continue as alice?" page with the wallet's address and a short npub. Its button finishes the sign-in. |
| The key belongs to a wallet, and the browser is signed in to a different Satchel wallet | The same confirmation page, noting the current wallet. Continuing signs that wallet out in this browser. |
| No wallet uses the key | A "Create your wallet" page with the username prefilled from `name` and one **Create wallet** button. The new wallet signs in with the key from then on. |
| The wallet is frozen | Refused. |

Some details behind those results:

- **The redirect through `/auth/nostr/handoff/continue`:** for a key with a
  wallet, Satchel answers your form post with a redirect there, carrying a
  short-lived cookie. Browsers do not send Satchel's `SameSite=Lax` session
  cookie with a form post from another site, but they do send it on that
  redirect. That is how Satchel can tell whether the browser is already
  signed in, and as whom.
- **Why the confirmation page exists:** a site that is not listed could
  otherwise sign a visitor in to a wallet of that site's choosing (login
  CSRF). The confirmation button posts from Satchel's own page, which no
  other site can do.
- **Creating a wallet:** the "Create your wallet" page posts to Satchel's
  regular sign-up handler. In place of a password, the form holds a
  short-lived token that refers to the key on the server. The key never
  travels in a form field. Every sign-up check applies, including the
  proof-of-work when the operator has it enabled.

When a handoff is refused, the user sees a page explaining why. The status
codes are:

| Status | Meaning |
| --- | --- |
| 400 | Malformed event, or an expired or used token |
| 401 | Bad signature, wrong `u` or `method`, stale `created_at`, or a replayed event |
| 403 | Frozen wallet |
| 429 | Too many attempts from one address |

## 3. Address lookup

```
GET SATCHEL/api/v1/address
Authorization: Nostr <base64(signed event JSON)>
```

The event is a NIP-98 HTTP auth event:

- kind 27235 and empty content;
- `u` is the full request URL, `SATCHEL/api/v1/address`;
- `method` is `GET`;
- `created_at` is within ±60 s.

Encode the event JSON as standard base64.

```js
async function satchelAddress(signEvent) {
  const url = `${SATCHEL}/api/v1/address`;
  const event = await signEvent({
    kind: 27235,
    created_at: Math.floor(Date.now() / 1000),
    content: "",
    tags: [
      ["u", url],
      ["method", "GET"],
    ],
  });
  const response = await fetch(url, {
    headers: { Authorization: `Nostr ${btoa(JSON.stringify(event))}` },
  });
  if (response.status === 404) return null; // no wallet yet: offer "Open Satchel"
  if (!response.ok) throw new Error(`Satchel answered ${response.status}`);
  return (await response.json()).lightning_address;
}

// With a NIP-07 signer:
const address = await satchelAddress((template) => window.nostr.signEvent(template));
```

`btoa` handles only Latin-1 text. The event above is plain ASCII. If you add
tags with other characters, encode the JSON as UTF-8 bytes before base64.

Responses are JSON:

| Status | Body |
| --- | --- |
| 200 | `{"lightning_address": "alice@wallet.example.org", "username": "alice"}` |
| 404 | `{"error": "no_account", ...}`: no wallet uses the key, or it is frozen |
| 401 | `{"error": "unauthorized", "message": "..."}`, with `WWW-Authenticate: Nostr` |
| 429 | `{"error": "rate_limited", ...}` |

The lookup never creates a wallet. It is rate-limited per client address,
with the same limit as the LNURL endpoints (`rate_limits.lnurl_per_ip_per_minute`).

**CORS:** for an `Origin` listed in `handoff_origins`, Satchel:

- echoes the origin in `Access-Control-Allow-Origin`;
- allows the `GET` method and the `Authorization` header;
- answers the preflight, cached for 10 minutes.

Credentials are never allowed, so do not send cookies. Other origins get no
CORS headers, so browsers will not let their pages read the answer.
Server-to-server calls need no CORS. If your page sends a CSP, its
`connect-src` must include the Satchel origin.

## NIP-98 notes

- Both endpoints use kind 27235 events with `u` and `method` tags. Satchel
  ignores extra tags, and no `payload` tag is needed because neither request
  has a body.
- `u` is compared byte for byte. It uses Satchel's public origin exactly as
  the operator configured it, so `https://wallet.example.org/...`, without a
  default port or a trailing slash after the path.
- `pubkey`, `id`, and `sig` are lowercase hex, as NIP-01 defines them.
- Handoff events travel in a form field and work once. Address lookup events
  travel in the `Authorization` header. They may be reused within their
  60-second window, because the lookup only reads.
- Keep clocks in sync. Events more than 120 s (handoff) or 60 s (lookup) away
  from Satchel's clock are refused.
