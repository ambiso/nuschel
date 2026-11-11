-- SPDX-License-Identifier: EUPL-1.2
-- Full-text index of each message's visible text. The text itself stays in
-- message_search (external content), so it is stored once. Tokens are runs
-- of letters and digits, case-folded, with diacritics kept (as PostgreSQL's
-- 'simple' configuration does); short prefixes have their own index.
CREATE VIRTUAL TABLE message_fts USING fts5 (
    visible,
    content = 'message_search',
    content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 0',
    prefix = '2 3'
);
-- Rows of message_search below this ID are not indexed yet. Appends index
-- their message at once; History fills the rest in batches, newest first.
CREATE TABLE search_backfill (below INTEGER NOT NULL);
INSERT INTO search_backfill SELECT COALESCE(MAX(id), 0) + 1 FROM message_search;
