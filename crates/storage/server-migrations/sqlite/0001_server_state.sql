-- nuschld's server state. Tracked in _nuschld_migrations, separate from the
-- history schema, so both can share one database.
CREATE TABLE channels (
    id INTEGER PRIMARY KEY,
    parent INTEGER REFERENCES channels (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    position INTEGER NOT NULL DEFAULT 0,
    max_users INTEGER NOT NULL DEFAULT 0,
    inherit_acl INTEGER NOT NULL DEFAULT 1,
    conversation_id TEXT NOT NULL UNIQUE,
    history_visibility TEXT NOT NULL DEFAULT 'all'
);
CREATE TABLE channel_acls (
    channel INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    priority INTEGER NOT NULL,
    apply_here INTEGER NOT NULL,
    apply_subs INTEGER NOT NULL,
    user_id INTEGER,
    group_name TEXT,
    grant_flags INTEGER NOT NULL,
    deny_flags INTEGER NOT NULL,
    PRIMARY KEY (channel, priority)
);
CREATE TABLE channel_groups (
    channel INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    inherit INTEGER NOT NULL,
    inheritable INTEGER NOT NULL,
    PRIMARY KEY (channel, name)
);
CREATE TABLE channel_group_members (
    channel INTEGER NOT NULL,
    name TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    added INTEGER NOT NULL,
    PRIMARY KEY (channel, name, user_id, added),
    FOREIGN KEY (channel, name) REFERENCES channel_groups (channel, name) ON DELETE CASCADE
);
CREATE TABLE channel_links (
    a INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    b INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    PRIMARY KEY (a, b)
);
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    name_key TEXT NOT NULL UNIQUE,
    password_hash TEXT,
    certificate_hash TEXT,
    comment TEXT NOT NULL DEFAULT '',
    texture TEXT NOT NULL DEFAULT '',
    last_channel INTEGER,
    last_active INTEGER
);
CREATE INDEX users_certificate ON users (certificate_hash);
CREATE TABLE bans (
    address TEXT,
    mask INTEGER NOT NULL,
    name TEXT NOT NULL,
    hash TEXT NOT NULL,
    reason TEXT NOT NULL,
    start INTEGER NOT NULL,
    duration INTEGER NOT NULL
);
CREATE TABLE server_meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
