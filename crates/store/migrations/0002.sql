-- Auditable migrations and offline maintenance. No accepted-message rows change.
CREATE TABLE schema_migration (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    name TEXT NOT NULL,
    sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
    applied_at_ms INTEGER NOT NULL CHECK (applied_at_ms >= 0),
    adopted INTEGER NOT NULL CHECK (adopted IN (0, 1))
) STRICT;

CREATE TABLE maintenance_run (
    id TEXT PRIMARY KEY CHECK (length(id) = 32),
    started_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    min_age_seconds INTEGER NOT NULL CHECK (min_age_seconds >= 0),
    status TEXT NOT NULL CHECK (status IN ('running', 'complete', 'interrupted'))
) STRICT;

CREATE TABLE gc_action (
    run_id TEXT NOT NULL REFERENCES maintenance_run(id),
    kind TEXT NOT NULL CHECK (kind IN ('staging', 'orphan')),
    object_id TEXT NOT NULL CHECK (length(object_id) = 32),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    status TEXT NOT NULL CHECK (status IN ('planned', 'deleted')),
    PRIMARY KEY (run_id, kind, object_id)
) STRICT;
