CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    server TEXT NOT NULL,
    channel TEXT NOT NULL,
    author TEXT NOT NULL,
    observed_at_ms BIGINT NOT NULL,
    body TEXT NOT NULL
);
CREATE INDEX messages_channel ON messages (server, channel, id DESC);
