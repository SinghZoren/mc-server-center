-- Schema v1. Timestamps are Unix epoch milliseconds (INTEGER, UTC). IDs are ULID text.
-- See planning/04-data-and-security.md §1. Never edit an applied migration; add a new one.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE java_runtimes (
    id           TEXT PRIMARY KEY NOT NULL,
    vendor       TEXT NOT NULL,
    major        INTEGER NOT NULL CHECK (major > 0),
    full_version TEXT NOT NULL,
    arch         TEXT NOT NULL,
    path         TEXT NOT NULL UNIQUE,
    source       TEXT NOT NULL CHECK (source IN ('managed', 'system')),
    sha256       TEXT,
    verified_at  INTEGER,
    installed_at INTEGER NOT NULL
) STRICT;

CREATE TABLE instances (
    id                TEXT PRIMARY KEY NOT NULL,
    name              TEXT NOT NULL,
    software          TEXT NOT NULL,
    mc_version        TEXT NOT NULL,
    build             TEXT,
    install_root      TEXT NOT NULL UNIQUE,
    port              INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    memory_json       TEXT NOT NULL CHECK (json_valid(memory_json)),
    state             TEXT NOT NULL,
    state_version     INTEGER NOT NULL DEFAULT 0,
    java_runtime_id   TEXT REFERENCES java_runtimes (id) ON DELETE SET NULL,
    eula_accepted_at  INTEGER,
    eula_accepted_by  TEXT,
    deleted_at        INTEGER,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
) STRICT;

-- Ports must be unique among instances that are not deleted.
CREATE UNIQUE INDEX instances_port_live ON instances (port) WHERE deleted_at IS NULL;

CREATE TABLE journal (
    id           TEXT PRIMARY KEY NOT NULL,
    kind         TEXT NOT NULL,
    instance_id  TEXT REFERENCES instances (id) ON DELETE CASCADE,
    steps_json   TEXT NOT NULL CHECK (json_valid(steps_json)),
    current_step INTEGER NOT NULL DEFAULT 0,
    status       TEXT NOT NULL CHECK (status IN ('active', 'committed', 'rolled_back', 'failed')),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
) STRICT;

CREATE INDEX journal_active ON journal (status) WHERE status = 'active';

CREATE TABLE tasks (
    id            TEXT PRIMARY KEY NOT NULL,
    kind          TEXT NOT NULL,
    target_ref    TEXT,
    state         TEXT NOT NULL,
    phase         TEXT,
    progress_json TEXT CHECK (progress_json IS NULL OR json_valid(progress_json)),
    error_json    TEXT CHECK (error_json IS NULL OR json_valid(error_json)),
    journal_id    TEXT REFERENCES journal (id) ON DELETE SET NULL,
    created_by    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    finished_at   INTEGER
) STRICT;

CREATE TABLE request_dedup (
    idempotency_key TEXT NOT NULL,
    principal       TEXT NOT NULL,
    response_json   TEXT NOT NULL CHECK (json_valid(response_json)),
    expires_at      INTEGER NOT NULL,
    PRIMARY KEY (idempotency_key, principal)
) STRICT;

-- Append-only audit trail. Rows are never updated or deleted by the application.
CREATE TABLE audit (
    seq                  INTEGER PRIMARY KEY AUTOINCREMENT,
    id                   TEXT NOT NULL UNIQUE,
    at                   INTEGER NOT NULL,
    principal            TEXT NOT NULL,
    action               TEXT NOT NULL,
    target_ref           TEXT,
    params_redacted_json TEXT CHECK (params_redacted_json IS NULL OR json_valid(params_redacted_json)),
    outcome              TEXT NOT NULL CHECK (outcome IN ('ok', 'error', 'started')),
    error_code           TEXT
);

CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;

CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;

CREATE TABLE settings (
    key        TEXT PRIMARY KEY NOT NULL,
    value_json TEXT NOT NULL CHECK (json_valid(value_json))
) STRICT;
