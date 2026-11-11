-- The server's key-transparency directory (docs/e2ee.md §5.2): nuschl-kt's
-- ordered key-value store and nuschld's records behind each binding. The
-- layout belongs to nuschl-kt and nuschld; records are inserted or
-- overwritten, never deleted.
CREATE TABLE kt_records (
    key BLOB PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
) WITHOUT ROWID;
