-- Bounce / complaint events received on /hooks/bounce.
-- Kept (capped) so you can see what Stalwart or another system actually sent.
CREATE TABLE bounce_events (
    id           INTEGER PRIMARY KEY,
    received_at  INTEGER NOT NULL,
    -- bounced | complained | soft | duplicate | unmatched | suppressed | ignored
    outcome      TEXT    NOT NULL,
    kind         TEXT,
    code         TEXT,
    email        TEXT,
    message_id   TEXT,
    recipient_id INTEGER,
    detail       TEXT,
    payload      TEXT    NOT NULL
);
