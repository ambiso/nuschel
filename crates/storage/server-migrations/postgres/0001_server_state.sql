-- nuschld's server state. Tracked in _nuschld_migrations, separate from the
-- history schema, so both can share one database.
CREATE TABLE channels (
    id BIGINT PRIMARY KEY,
    parent BIGINT REFERENCES channels (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    position BIGINT NOT NULL DEFAULT 0,
    max_users BIGINT NOT NULL DEFAULT 0,
    inherit_acl BIGINT NOT NULL DEFAULT 1,
    conversation_id TEXT NOT NULL UNIQUE,
    history_visibility TEXT NOT NULL DEFAULT 'all'
);
CREATE TABLE channel_acls (
    channel BIGINT NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    priority BIGINT NOT NULL,
    apply_here BIGINT NOT NULL,
    apply_subs BIGINT NOT NULL,
    user_id BIGINT,
    group_name TEXT,
    grant_flags BIGINT NOT NULL,
    deny_flags BIGINT NOT NULL,
    PRIMARY KEY (channel, priority)
);
CREATE TABLE channel_groups (
    channel BIGINT NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    inherit BIGINT NOT NULL,
    inheritable BIGINT NOT NULL,
    PRIMARY KEY (channel, name)
);
CREATE TABLE channel_group_members (
    channel BIGINT NOT NULL,
    name TEXT NOT NULL,
    user_id BIGINT NOT NULL,
    added BIGINT NOT NULL,
    PRIMARY KEY (channel, name, user_id, added),
    FOREIGN KEY (channel, name) REFERENCES channel_groups (channel, name) ON DELETE CASCADE
);
CREATE TABLE channel_links (
    a BIGINT NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    b BIGINT NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    PRIMARY KEY (a, b)
);
CREATE TABLE users (
    id BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    name_key TEXT NOT NULL UNIQUE,
    password_hash TEXT,
    certificate_hash TEXT,
    comment TEXT NOT NULL DEFAULT '',
    texture TEXT NOT NULL DEFAULT '',
    last_channel BIGINT,
    last_active BIGINT
);
CREATE INDEX users_certificate ON users (certificate_hash);
CREATE TABLE bans (
    address TEXT,
    mask BIGINT NOT NULL,
    name TEXT NOT NULL,
    hash TEXT NOT NULL,
    reason TEXT NOT NULL,
    start BIGINT NOT NULL,
    duration BIGINT NOT NULL
);
CREATE TABLE server_meta (
    key TEXT PRIMARY KEY,
    value BIGINT NOT NULL
);
