CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    email         TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE sessions (
    token_hash TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);

CREATE TABLE domains (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL UNIQUE,
    spf_ok      INTEGER NOT NULL DEFAULT 0,
    dkim_ok     INTEGER NOT NULL DEFAULT 0,
    dmarc_ok    INTEGER NOT NULL DEFAULT 0,
    -- JSON array of {type, name, value}, from Stalwart when available
    dns_records TEXT    NOT NULL DEFAULT '[]',
    stalwart_synced INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT,
    checked_at  INTEGER,
    created_at  INTEGER NOT NULL
);

CREATE TABLE api_keys (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL,
    display      TEXT    NOT NULL,
    key_hash     TEXT    NOT NULL UNIQUE,
    test_mode    INTEGER NOT NULL DEFAULT 0,
    domain_id    INTEGER REFERENCES domains(id) ON DELETE SET NULL,
    daily_limit  INTEGER,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at   INTEGER
);

CREATE TABLE batches (
    id              TEXT    PRIMARY KEY,
    name            TEXT    NOT NULL,
    source          TEXT    NOT NULL,
    api_key_id      INTEGER REFERENCES api_keys(id) ON DELETE SET NULL,
    kind            TEXT    NOT NULL DEFAULT 'marketing',
    test_mode       INTEGER NOT NULL DEFAULT 0,
    from_addr       TEXT    NOT NULL,
    reply_to        TEXT,
    subject         TEXT    NOT NULL,
    html            TEXT,
    text            TEXT,
    rate_per_minute INTEGER NOT NULL,
    status          TEXT    NOT NULL,
    total           INTEGER NOT NULL,
    send_at         INTEGER NOT NULL,
    created_at      INTEGER NOT NULL,
    finished_at     INTEGER
);
CREATE INDEX batches_status ON batches(status, send_at);
CREATE INDEX batches_created ON batches(created_at);

CREATE TABLE recipients (
    id              INTEGER PRIMARY KEY,
    batch_id        TEXT    NOT NULL REFERENCES batches(id) ON DELETE CASCADE,
    email           TEXT    NOT NULL,
    vars            TEXT    NOT NULL DEFAULT '{}',
    status          TEXT    NOT NULL DEFAULT 'queued',
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    smtp_response   TEXT,
    message_id      TEXT,
    updated_at      INTEGER
);
CREATE INDEX recipients_batch_status ON recipients(batch_id, status, next_attempt_at);

CREATE TABLE suppressions (
    email      TEXT    PRIMARY KEY,
    reason     TEXT,
    source     TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);
