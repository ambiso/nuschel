-- SPDX-License-Identifier: EUPL-1.2
-- Messages of Nuschl servers carry their per-conversation sequence number,
-- so a message seen live and again in a fetched history page is stored once;
-- edits and deletions find their row by it. A deleted message stays as a
-- tombstone without text. All three are NULL for local observations.
ALTER TABLE messages ADD COLUMN seq BIGINT;
ALTER TABLE messages ADD COLUMN edited_ms BIGINT;
ALTER TABLE messages ADD COLUMN deleted_ms BIGINT;
CREATE UNIQUE INDEX messages_seq ON messages (server, channel, seq) WHERE seq IS NOT NULL;
-- Pages run in time order: history pages fetched later hold older messages
-- than rows stored before them, so row IDs no longer follow the timeline.
CREATE INDEX messages_time ON messages (server, channel, observed_at_ms, id);
