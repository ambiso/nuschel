-- SPDX-License-Identifier: EUPL-1.2
-- Full-text document of each message's visible text, with the 'simple'
-- configuration (no stemming or stop words, as SQLite's FTS5 index).
-- Nullable, so adding it rewrites nothing.
ALTER TABLE message_search ADD COLUMN document tsvector;
CREATE INDEX message_search_document ON message_search USING GIN (document);
-- Rows of message_search below this ID are not indexed yet. Appends index
-- their message at once; History fills the rest in batches, newest first.
CREATE TABLE search_backfill (below BIGINT NOT NULL);
INSERT INTO search_backfill SELECT COALESCE(MAX(id), 0) + 1 FROM message_search;
