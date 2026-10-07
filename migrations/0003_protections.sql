-- Where each account was created from: the client key rate limits use (an
-- IPv4 address or an IPv6 prefix). Accounts from before this migration have none.
ALTER TABLE accounts ADD COLUMN signup_client TEXT;
CREATE INDEX accounts_created ON accounts (created_at);
CREATE INDEX accounts_signup_client ON accounts (signup_client);

-- Operator blocks: IPv4 and IPv6 networks refused on every public route.
-- A NULL expiry never expires.
CREATE TABLE blocks (
    id INTEGER PRIMARY KEY,
    cidr TEXT NOT NULL UNIQUE,
    reason TEXT NOT NULL,
    expires_at INTEGER,
    created_at INTEGER NOT NULL
);

-- Recent activity per client key: the per-address faucet cap and the
-- operator's busiest-addresses view. Rows older than a week are pruned.
CREATE TABLE client_events (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('signup', 'faucet', 'lnurl_invoice')),
    client TEXT NOT NULL,
    account_id INTEGER REFERENCES accounts (id),
    amount_msat INTEGER NOT NULL DEFAULT 0 CHECK (amount_msat >= 0),
    created_at INTEGER NOT NULL
);
CREATE INDEX client_events_recent ON client_events (kind, created_at);
CREATE INDEX client_events_client ON client_events (kind, client, created_at);

-- Counting an account's open invoices for the open-invoice cap.
CREATE INDEX invoices_open_account ON invoices (account_id, source) WHERE state = 'open';
