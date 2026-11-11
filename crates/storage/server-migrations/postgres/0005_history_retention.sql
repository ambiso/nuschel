-- Per-conversation retention (docs/extension.md §6), overriding the server
-- default; kept per conversation so a deleted channel's archive keeps it.
-- `mode` is 'forever', 'days' (with `days`) or 'none' (not archived).
-- Numbered 0005: 0004 is taken by channel listeners.
CREATE TABLE history_retention (
    conversation TEXT PRIMARY KEY,
    mode TEXT NOT NULL,
    days BIGINT
);
-- The highest seq a live-only conversation used without storing a row, so
-- sequence numbers are never reused after a restart.
CREATE TABLE history_heads (
    conversation TEXT PRIMARY KEY,
    head BIGINT NOT NULL
);
