-- Channel listeners of registered users, restored when they reconnect (as
-- Murmur's channel_listeners table). Numbered 0004: 0003 is reserved for the
-- key-transparency tables.
CREATE TABLE channel_listeners (
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    channel INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    volume REAL NOT NULL DEFAULT 1,
    PRIMARY KEY (user_id, channel)
);
