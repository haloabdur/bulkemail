-- Outgoing status webhooks, configured per API key in the dashboard.
ALTER TABLE api_keys ADD COLUMN webhook_url TEXT;
-- HMAC signing secret, encrypted with APP_SECRET (src/secretbox.rs).
ALTER TABLE api_keys ADD COLUMN webhook_secret TEXT;

-- Outbox: events wait here until the receiver answers 2xx, so a restart or an
-- outage on the receiving side loses nothing.
CREATE TABLE webhook_deliveries (
    id              INTEGER PRIMARY KEY,
    api_key_id      INTEGER NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    event_id        TEXT    NOT NULL,
    event           TEXT    NOT NULL,
    payload         TEXT    NOT NULL,
    -- pending | delivered | failed | dropped
    status          TEXT    NOT NULL DEFAULT 'pending',
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    last_error      TEXT,
    created_at      INTEGER NOT NULL,
    delivered_at    INTEGER
);
CREATE INDEX webhook_deliveries_due ON webhook_deliveries(status, next_attempt_at);
CREATE INDEX webhook_deliveries_key ON webhook_deliveries(api_key_id, id);
