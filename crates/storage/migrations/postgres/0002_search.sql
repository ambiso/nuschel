-- SPDX-License-Identifier: EUPL-1.2
-- Lowercased visible text of each message for history search: LIKE never
-- reads or matches inline image data. Appends write it with the message;
-- History fills it for messages stored before this migration, newest first.
CREATE TABLE message_search (
    id BIGINT PRIMARY KEY REFERENCES messages (id),
    server TEXT NOT NULL,
    author TEXT NOT NULL,
    visible TEXT NOT NULL
);
CREATE INDEX message_search_server ON message_search (server, id DESC);
