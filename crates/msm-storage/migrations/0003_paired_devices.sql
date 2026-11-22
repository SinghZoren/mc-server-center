-- Schema v3: remote controllers paired with this daemon (REM-02).
CREATE TABLE paired_devices (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    cert_sha256 TEXT NOT NULL UNIQUE,
    scopes_json TEXT NOT NULL CHECK (json_valid(scopes_json)),
    created_at  INTEGER NOT NULL,
    last_seen   INTEGER,
    revoked_at  INTEGER
) STRICT;
