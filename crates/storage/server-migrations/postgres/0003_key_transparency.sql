-- The server's key-transparency directory (docs/e2ee.md §5.2): nuschel-kt's
-- ordered key-value store and nuscheld's records behind each binding. The
-- layout belongs to nuschel-kt and nuscheld; records are inserted or
-- overwritten, never deleted.
CREATE TABLE kt_records (
    key BYTEA PRIMARY KEY,
    value BYTEA NOT NULL
);
