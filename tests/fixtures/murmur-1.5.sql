-- A Murmur 1.5 database (schema version 9) for the import tests: the table
-- definitions are Murmur's own (src/murmur/ServerDB.cpp, SQLite branch, at
-- v1.5.939, with an empty table prefix); the rows are made up, including
-- malformed ones Murmur itself never writes.
CREATE TABLE `servers` (`server_id` INTEGER PRIMARY KEY AUTOINCREMENT);
CREATE TABLE `slog`(`server_id` INTEGER NOT NULL, `msg` TEXT, `msgtime` DATE);
CREATE INDEX `slog_time` ON `slog`(`msgtime`);
CREATE TRIGGER `slog_timestamp` AFTER INSERT ON `slog` FOR EACH ROW BEGIN UPDATE `slog` SET `msgtime` = datetime('now') WHERE rowid = new.rowid; END;
CREATE TRIGGER `slog_server_del` AFTER DELETE ON `servers` FOR EACH ROW BEGIN DELETE FROM `slog` WHERE `server_id` = old.`server_id`; END;
CREATE TABLE `config` (`server_id` INTEGER NOT NULL, `key` TEXT, `value` TEXT);
CREATE UNIQUE INDEX `config_key` ON `config`(`server_id`, `key`);
CREATE TRIGGER `config_server_del` AFTER DELETE ON `servers` FOR EACH ROW BEGIN DELETE FROM `config` WHERE `server_id` = old.`server_id`; END;
CREATE TABLE `channels` (`server_id` INTEGER NOT NULL, `channel_id` INTEGER NOT NULL, `parent_id` INTEGER, `name` TEXT, `inheritacl` INTEGER);
CREATE UNIQUE INDEX `channel_id` ON `channels`(`server_id`, `channel_id`);
CREATE TRIGGER `channels_parent_del` AFTER DELETE ON `channels` FOR EACH ROW BEGIN DELETE FROM `channels` WHERE `parent_id` = old.`channel_id` AND `server_id` = old.`server_id`; UPDATE `users` SET `lastchannel`=0 WHERE `lastchannel` = old.`channel_id` AND `server_id` = old.`server_id`; END;
CREATE TRIGGER `channels_server_del` AFTER DELETE ON `servers` FOR EACH ROW BEGIN DELETE FROM `channels` WHERE `server_id` = old.`server_id`; END;
CREATE TABLE `channel_info` (`server_id` INTEGER NOT NULL, `channel_id` INTEGER NOT NULL, `key` INTEGER, `value` TEXT);
CREATE UNIQUE INDEX `channel_info_id` ON `channel_info`(`server_id`, `channel_id`, `key`);
CREATE TRIGGER `channel_info_del_channel` AFTER DELETE on `channels` FOR EACH ROW BEGIN DELETE FROM `channel_info` WHERE `channel_id` = old.`channel_id` AND `server_id` = old.`server_id`; END;
CREATE TABLE `users` (`server_id` INTEGER NOT NULL, `user_id` INTEGER NOT NULL, `name` TEXT NOT NULL, `pw` TEXT, `salt` TEXT, `kdfiterations` INTEGER, `lastchannel` INTEGER, `texture` BLOB, `last_active` DATE, `last_disconnect` DATE);
CREATE UNIQUE INDEX `users_name` ON `users` (`server_id`,`name`);
CREATE UNIQUE INDEX `users_id` ON `users` (`server_id`, `user_id`);
CREATE TRIGGER `users_server_del` AFTER DELETE ON `servers` FOR EACH ROW BEGIN DELETE FROM `users` WHERE `server_id` = old.`server_id`; END;
CREATE TRIGGER `users_update_timestamp` AFTER UPDATE OF `lastchannel` ON `users` FOR EACH ROW BEGIN UPDATE `users` SET `last_active` = datetime('now') WHERE `user_id` = old.`user_id` AND `server_id` = old.`server_id`; END;
CREATE TABLE `user_info` (`server_id` INTEGER NOT NULL, `user_id` INTEGER NOT NULL, `key` INTEGER, `value` TEXT);
CREATE UNIQUE INDEX `user_info_id` ON `user_info`(`server_id`, `user_id`, `key`);
CREATE TRIGGER `user_info_del_user` AFTER DELETE on `users` FOR EACH ROW BEGIN DELETE FROM `user_info` WHERE `user_id` = old.`user_id` AND `server_id` = old.`server_id`; END;
CREATE TABLE `groups` (`group_id` INTEGER PRIMARY KEY AUTOINCREMENT, `server_id` INTEGER NOT NULL, `name` TEXT, `channel_id` INTEGER NOT NULL, `inherit` INTEGER, `inheritable` INTEGER);
CREATE UNIQUE INDEX `groups_name_channels` ON `groups`(`server_id`, `channel_id`, `name`);
CREATE TRIGGER `groups_del_channel` AFTER DELETE ON `channels` FOR EACH ROW BEGIN DELETE FROM `groups` WHERE `channel_id` = old.`channel_id` AND `server_id` = old.`server_id`; END;
CREATE TABLE `group_members` (`group_id` INTEGER NOT NULL, `server_id` INTEGER NOT NULL, `user_id` INTEGER NOT NULL, `addit` INTEGER);
CREATE TRIGGER `groups_members_del_group` AFTER DELETE ON `groups` FOR EACH ROW BEGIN DELETE FROM `group_members` WHERE `group_id` = old.`group_id`; END;
CREATE TRIGGER `groups_members_del_user` AFTER DELETE on `users` FOR EACH ROW BEGIN DELETE FROM `group_members` WHERE `user_id` = old.`user_id` AND `server_id` = old.`server_id`; END;
CREATE TABLE `acl` (`server_id` INTEGER NOT NULL, `channel_id` INTEGER NOT NULL, `priority` INTEGER, `user_id` INTEGER, `group_name` TEXT, `apply_here` INTEGER, `apply_sub` INTEGER, `grantpriv` INTEGER, `revokepriv` INTEGER);
CREATE UNIQUE INDEX `acl_channel_pri` ON `acl`(`server_id`, `channel_id`, `priority`);
CREATE TRIGGER `acl_del_channel` AFTER DELETE ON `channels` FOR EACH ROW BEGIN DELETE FROM `acl` WHERE `channel_id` = old.`channel_id` AND `server_id` = old.`server_id`; END;
CREATE TRIGGER `acl_del_user` AFTER DELETE ON `users` FOR EACH ROW BEGIN DELETE FROM `acl` WHERE `user_id` = old.`user_id` AND `server_id` = old.`server_id`; END;
CREATE TABLE `channel_links` (`server_id` INTEGER NOT NULL, `channel_id` INTEGER NOT NULL, `link_id` INTEGER NOT NULL);
CREATE TRIGGER `channel_links_del_channel` AFTER DELETE ON `channels` FOR EACH ROW BEGIN DELETE FROM `channel_links` WHERE `server_id` = old.`server_id` AND (`channel_id` = old.`channel_id` OR `link_id` = old.`channel_id`); END;
DELETE FROM `channel_links`;
CREATE TABLE `bans` (`server_id` INTEGER NOT NULL, `base` BLOB, `mask` INTEGER, `name` TEXT, `hash` TEXT, `reason` TEXT, `start` DATE, `duration` INTEGER);
CREATE TRIGGER `bans_del_server` AFTER DELETE ON `servers` FOR EACH ROW BEGIN DELETE FROM `bans` WHERE `server_id` = old.`server_id`; END;
CREATE TABLE `channel_listeners` (`server_id` INTEGER NOT NULL, `user_id` INTEGER NOT NULL, `channel_id` INTEGER NOT NULL, `volume_adjustment` FLOAT DEFAULT 1, `enabled` SMALLINT DEFAULT 1);
CREATE TRIGGER `channel_listeners_del_server` AFTER DELETE ON `servers` FOR EACH ROW BEGIN DELETE FROM `channel_listeners` WHERE `server_id` = old.`server_id`; END;
CREATE TRIGGER `channel_listeners_del_channel` AFTER DELETE ON `channels` FOR EACH ROW BEGIN DELETE FROM `channel_listeners` WHERE `server_id` = old.`server_id` AND `channel_id` = old.`channel_id`; END;
CREATE TRIGGER `channel_listeners_del_user` AFTER DELETE ON `users` FOR EACH ROW BEGIN DELETE FROM `channel_listeners` WHERE `server_id` = old.`server_id` AND `user_id` = old.`user_id`; END;
CREATE TABLE IF NOT EXISTS `meta` (`keystring` TEXT PRIMARY KEY, `value` TEXT);

INSERT INTO meta VALUES ('version', '9');
INSERT INTO meta VALUES ('pbkdf2_iterations', '1000');
INSERT INTO servers (server_id) VALUES (1);
INSERT INTO servers (server_id) VALUES (2);

INSERT INTO config VALUES (1, 'welcometext', 'Welcome to the <b>imported</b> server');
INSERT INTO config VALUES (1, 'password', 'serverpw');
INSERT INTO config VALUES (1, 'registername', 'Imported');
INSERT INTO config VALUES (1, 'users', '42');
INSERT INTO config VALUES (1, 'registerhostname', 'murmur.example');
INSERT INTO config VALUES (2, 'welcometext', 'the other virtual server');

-- The tree: Lobby/Sub, Secret; an invalid name with a child; an orphan; a
-- malformed id; server 2's root must not leak in.
INSERT INTO channels VALUES (1, 0, NULL, 'Root', 1);
INSERT INTO channels VALUES (1, 1, 0, 'Lobby', 1);
INSERT INTO channels VALUES (1, 2, 0, 'Secret', 1);
INSERT INTO channels VALUES (1, 3, 1, 'Sub', 0);
INSERT INTO channels VALUES (1, 4, 0, 'bad>name', 1);
INSERT INTO channels VALUES (1, 6, 4, 'Child of bad', 1);
INSERT INTO channels VALUES (1, 5, 99, 'Orphan', 1);
INSERT INTO channels VALUES (1, 'seven', 0, 'Malformed', 1);
INSERT INTO channels VALUES (2, 0, NULL, 'Other root', 1);
INSERT INTO channel_info VALUES (1, 1, 0, 'The <i>lobby</i>');
INSERT INTO channel_info VALUES (1, 1, 1, '5');
INSERT INTO channel_info VALUES (1, 1, 2, '10');
INSERT INTO channel_info VALUES (1, 2, 1, 'not a number');

-- SuperUser and alice: PBKDF2-HMAC-SHA384 ("superpw", "hunter2"); bob: the
-- legacy SHA-1 ("password"); carol: certificate only; eve: PBKDF2 of a
-- non-ASCII password ("pässwörd") with a comment and an avatar; dave: a
-- malformed hash; an invalid name, a duplicate name in other case, and a
-- malformed id.
INSERT INTO users (server_id, user_id, name, pw, salt, kdfiterations, lastchannel, last_active) VALUES (1, 0, 'SuperUser', '05db443a64c1081ec29f11d032d26f26b50cd25ef14fa05e322820a1cf1ef5bfe10a9582221ee9b300d9f4d6f7449c1d', 'a1b2c3d4e5f60718', 2000, 0, '2025-11-01 10:00:00');
INSERT INTO users (server_id, user_id, name, pw, salt, kdfiterations, lastchannel, last_active) VALUES (1, 1, 'alice', 'c2fa5023e808bd1adafccf5f995d5147a0d17e0113e9bda3fa8380faa58088467346ba1eb6bbf7e6374da78caa9b784c', '0011223344556677', 1000, 1, '2025-11-02 11:00:00');
INSERT INTO users (server_id, user_id, name, pw, salt, kdfiterations, lastchannel, last_active) VALUES (1, 2, 'bob', '5baa61e4c9b93f3f0682250b6cf8331b7ee68fd8', NULL, NULL, 2, NULL);
INSERT INTO users (server_id, user_id, name, pw, salt, kdfiterations, lastchannel) VALUES (1, 3, 'carol', NULL, NULL, NULL, 4);
INSERT INTO users (server_id, user_id, name, pw, salt, kdfiterations, lastchannel, texture) VALUES (1, 7, 'eve', 'f8dc070bb20e5c910cf3f4e0e92ccbc51a9521f7dc011dc8bbc3e298eafbcc9b50b13d6a47518d83dd65e00ee284b859', 'a1b2c3d4e5f60718', 4000, NULL, X'89504E470D0A1A0A0000000D49484452');
INSERT INTO users (server_id, user_id, name, pw, salt, kdfiterations) VALUES (1, 6, 'dave', 'not hex at all', 'a1b2c3d4e5f60718', 1000);
INSERT INTO users (server_id, user_id, name, pw) VALUES (1, 4, 'bad>user', NULL);
INSERT INTO users (server_id, user_id, name, pw) VALUES (1, 5, 'Alice', NULL);
INSERT INTO users (server_id, user_id, name, pw) VALUES (1, 'x', 'malformed', NULL);
INSERT INTO users (server_id, user_id, name, pw) VALUES (2, 1, 'other-server-user', NULL);
INSERT INTO user_info VALUES (1, 1, 3, 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA');
INSERT INTO user_info VALUES (1, 3, 3, 'cccccccccccccccccccccccccccccccccccccccc');
INSERT INTO user_info VALUES (1, 2, 1, 'bob@example.org');
INSERT INTO user_info VALUES (1, 7, 2, 'Hi, I am <b>eve</b>');
INSERT INTO user_info VALUES (1, 6, 3, 'not a hash');

-- Murmur's default root ACLs, a channel only "insiders" may enter, a user
-- entry, and entries for a user and a channel that do not exist.
INSERT INTO groups (group_id, server_id, name, channel_id, inherit, inheritable) VALUES (1, 1, 'admin', 0, 1, 1);
INSERT INTO groups (group_id, server_id, name, channel_id, inherit, inheritable) VALUES (2, 1, 'insiders', 2, 1, 0);
INSERT INTO group_members VALUES (1, 1, 1, 1);
INSERT INTO group_members VALUES (2, 1, 2, 1);
INSERT INTO group_members VALUES (2, 1, 999, 1);
INSERT INTO group_members VALUES (3, 1, 1, 1);
INSERT INTO acl VALUES (1, 0, 1, NULL, 'admin', 1, 1, 1, 0);
INSERT INTO acl VALUES (1, 0, 2, NULL, 'auth', 1, 1, 1024, 0);
INSERT INTO acl VALUES (1, 0, 3, NULL, 'all', 1, 0, 524288, 0);
INSERT INTO acl VALUES (1, 2, 1, NULL, 'all', 1, 1, 0, 4);
INSERT INTO acl VALUES (1, 2, 2, NULL, 'insiders', 1, 1, 516, 0);
INSERT INTO acl VALUES (1, 1, 1, 2, NULL, 1, 1, 64, 0);
INSERT INTO acl VALUES (1, 1, 2, 999, NULL, 1, 1, 64, 0);
INSERT INTO acl VALUES (1, 4, 1, NULL, 'all', 1, 1, 0, 4);

-- Murmur stores each link in both directions.
INSERT INTO channel_links VALUES (1, 1, 3);
INSERT INTO channel_links VALUES (1, 3, 1);
INSERT INTO channel_links VALUES (1, 1, 99);

-- A permanent ban of ::ffff:192.0.2.1, an expired one, one with an invalid
-- prefix length and one with a truncated address.
INSERT INTO bans VALUES (1, X'00000000000000000000FFFFC0000201', 128, 'troll', '', 'spam', '2025-01-01T00:00:00', 0);
INSERT INTO bans VALUES (1, X'00000000000000000000FFFFC0000202', 128, 'old', '', 'long ago', '2020-01-01T00:00:00', 60);
INSERT INTO bans VALUES (1, X'00000000000000000000FFFFC0000203', 4, 'wide', '', 'too wide', '2025-01-01T00:00:00', 0);
INSERT INTO bans VALUES (1, X'C0000204', 128, 'short', '', 'four bytes', '2025-01-01T00:00:00', 0);

INSERT INTO channel_listeners VALUES (1, 1, 2, 0.5, 1);
INSERT INTO channel_listeners VALUES (1, 2, 1, 1, 0);
INSERT INTO channel_listeners VALUES (1, 1, 99, 1, 1);
