-- Handoff sign-in: another app posts an event signed with the user's Nostr key.

-- Event ids already used, kept well past the events' two-minute validity window
-- so none can be replayed.
CREATE TABLE handoff_events (
    event_id TEXT PRIMARY KEY,
    expires_at INTEGER NOT NULL
);
CREATE INDEX handoff_events_expiry ON handoff_events (expires_at);

-- Verified handoffs waiting for the next step: the redirect that reads this
-- site's cookies, the confirmation page, or the create-wallet page. Tokens are
-- stored as SHA-256 hashes; pages hold the token, never the key.
CREATE TABLE handoff_pending (
    token_hash TEXT PRIMARY KEY,
    purpose TEXT NOT NULL CHECK (purpose IN ('signin', 'signup')),
    nostr_pubkey TEXT NOT NULL,
    next TEXT NOT NULL,
    -- Posted from an origin in server.handoff_origins.
    trusted INTEGER NOT NULL DEFAULT 0 CHECK (trusted IN (0, 1)),
    expires_at INTEGER NOT NULL
);
CREATE INDEX handoff_pending_expiry ON handoff_pending (expires_at);
