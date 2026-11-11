-- The Nuschel extension's history (docs/extension.md §3–§7).
-- Immutable 128-bit account ids; registration numbers are only aliases.
ALTER TABLE users ADD COLUMN account TEXT;
CREATE UNIQUE INDEX users_account ON users (account);
-- One row per seq of a conversation: messages (with their current content;
-- edits replace it), edit and deletion events, and voids.
CREATE TABLE history_messages (
    conversation TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    target INTEGER,
    author_account TEXT,
    author_user_id INTEGER,
    author_name TEXT NOT NULL,
    format INTEGER NOT NULL,
    content TEXT NOT NULL,
    reply_to INTEGER,
    sent_ms INTEGER NOT NULL,
    edited_ms INTEGER,
    edits INTEGER NOT NULL DEFAULT 0,
    deleted_ms INTEGER,
    delete_reason INTEGER,
    idempotency_key TEXT,
    body_hash TEXT,
    PRIMARY KEY (conversation, seq)
);
-- Scoped, atomic idempotency: NULL keys (unregistered senders) never collide.
CREATE UNIQUE INDEX history_idempotency ON history_messages (conversation, author_account, idempotency_key);
CREATE INDEX history_author ON history_messages (author_account);
CREATE INDEX history_sent ON history_messages (sent_ms);
CREATE TABLE history_directs (
    conversation TEXT PRIMARY KEY,
    account_a TEXT NOT NULL,
    account_b TEXT NOT NULL,
    UNIQUE (account_a, account_b)
);
CREATE TABLE history_access (
    conversation TEXT NOT NULL,
    account TEXT NOT NULL,
    start_seq INTEGER NOT NULL,
    end_seq INTEGER,
    PRIMARY KEY (conversation, account, start_seq)
);
CREATE TABLE history_read_markers (
    account TEXT NOT NULL,
    conversation TEXT NOT NULL,
    seq INTEGER NOT NULL,
    PRIMARY KEY (account, conversation)
);
