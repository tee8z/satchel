-- Accounts share one LND node; their money lives only in the ledger below.
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT,
    nostr_pubkey TEXT UNIQUE,
    frozen INTEGER NOT NULL DEFAULT 0 CHECK (frozen IN (0, 1)),
    created_at INTEGER NOT NULL,
    CHECK (password_hash IS NOT NULL OR nostr_pubkey IS NOT NULL)
);

-- Cookie tokens are stored as SHA-256 hashes. A NULL account is an operator session.
CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    account_id INTEGER REFERENCES accounts (id),
    csrf_token TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX sessions_expiry ON sessions (expires_at);

-- Invoices this server created in LND, each owned by one account.
CREATE TABLE invoices (
    payment_hash TEXT PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts (id),
    bolt11 TEXT NOT NULL,
    amount_msat INTEGER NOT NULL CHECK (amount_msat > 0),
    memo TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL CHECK (source IN ('lnurl', 'wallet')),
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'settled', 'canceled')),
    amount_paid_msat INTEGER,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    settled_at INTEGER
);
CREATE INDEX invoices_open ON invoices (created_at) WHERE state = 'open';
CREATE INDEX invoices_account ON invoices (account_id, created_at);

-- What people see in their history. Money moves only through ledger entries.
CREATE TABLE payments (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts (id),
    direction TEXT NOT NULL CHECK (direction IN ('in', 'out')),
    kind TEXT NOT NULL CHECK (kind IN ('lightning', 'internal', 'faucet', 'operator')),
    status TEXT NOT NULL CHECK (status IN ('pending', 'succeeded', 'failed')),
    amount_msat INTEGER NOT NULL CHECK (amount_msat > 0),
    fee_msat INTEGER NOT NULL DEFAULT 0 CHECK (fee_msat >= 0),
    fee_limit_msat INTEGER NOT NULL DEFAULT 0 CHECK (fee_limit_msat >= 0),
    payment_hash TEXT,
    request_key TEXT UNIQUE,
    counterparty TEXT NOT NULL DEFAULT '',
    memo TEXT NOT NULL DEFAULT '',
    failure TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX payments_account ON payments (account_id, id);
CREATE INDEX payments_pending ON payments (created_at) WHERE status = 'pending';
CREATE INDEX payments_faucet ON payments (created_at) WHERE kind = 'faucet';
-- One live Lightning send per payment hash; a failed attempt may be retried.
CREATE UNIQUE INDEX payments_one_live_send ON payments (payment_hash)
WHERE direction = 'out' AND kind = 'lightning' AND status <> 'failed';
-- An invoice credits at most one account, once, whether paid over Lightning or internally.
CREATE UNIQUE INDEX payments_one_credit_per_invoice ON payments (payment_hash)
WHERE direction = 'in' AND payment_hash IS NOT NULL;

-- Append-only ledger: a balance is the sum of its account's entries.
CREATE TABLE ledger (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts (id),
    amount_msat INTEGER NOT NULL CHECK (amount_msat <> 0),
    kind TEXT NOT NULL CHECK (kind IN ('credit', 'debit', 'refund')),
    payment_id INTEGER NOT NULL REFERENCES payments (id),
    idempotency_key TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);
CREATE INDEX ledger_account ON ledger (account_id);

CREATE TRIGGER ledger_no_update BEFORE UPDATE ON ledger
BEGIN
    SELECT RAISE(ABORT, 'ledger is append-only');
END;

CREATE TRIGGER ledger_no_delete BEFORE DELETE ON ledger
BEGIN
    SELECT RAISE(ABORT, 'ledger is append-only');
END;

CREATE TRIGGER ledger_no_negative_balance AFTER INSERT ON ledger
WHEN (SELECT SUM(amount_msat) FROM ledger WHERE account_id = NEW.account_id) < 0
BEGIN
    SELECT RAISE(ABORT, 'insufficient balance');
END;

-- Small named counters, such as the LND invoice settle index already processed.
CREATE TABLE cursors (
    name TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
