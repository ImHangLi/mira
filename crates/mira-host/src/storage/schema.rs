//! The workspace ledger schema. A new database runs [`SCHEMA`] once and records
//! [`super::SCHEMA_VERSION`] in `PRAGMA user_version`.

pub(super) const SCHEMA: &str = r#"
-- Exactly one row: the workspace this database belongs to and its persisted counters.
CREATE TABLE workspace_meta (
    id                  INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    workspace_id        TEXT NOT NULL,
    root                TEXT NOT NULL,
    created_at          INTEGER NOT NULL,
    catalog_revision    INTEGER NOT NULL DEFAULT 0 CHECK (catalog_revision >= 0),
    catalog_set_hash    TEXT,
    next_view_revision  INTEGER NOT NULL DEFAULT 1 CHECK (next_view_revision >= 1)
) STRICT;

-- Scalar columns carry what indexes and retention need; record_json is the canonical
-- RunRecord, decoded again on every read. No env values, input bodies, or stdin.
CREATE TABLE runs (
    run_id            TEXT PRIMARY KEY NOT NULL,
    action_ref        TEXT,
    source            TEXT NOT NULL,
    definition_hash   TEXT NOT NULL,
    catalog_revision  INTEGER NOT NULL,
    started_at        INTEGER NOT NULL,
    ended_at          INTEGER,
    lifecycle         TEXT NOT NULL
        CHECK (lifecycle IN ('starting', 'running', 'stopping', 'finished')),
    outcome           TEXT
        CHECK (outcome IN ('succeeded', 'failed', 'cancelled', 'timed_out', 'interrupted')),
    record_json       TEXT NOT NULL,
    CHECK ((lifecycle = 'finished') = (outcome IS NOT NULL))
) STRICT;
CREATE INDEX runs_by_start ON runs (started_at DESC, run_id DESC);
CREATE INDEX runs_by_action ON runs (action_ref, started_at DESC, run_id DESC);
CREATE INDEX runs_active ON runs (lifecycle) WHERE lifecycle <> 'finished';

-- Idempotency reservations; kept 24 h independent of run history.
CREATE TABLE request_keys (
    scope        TEXT NOT NULL,
    request_key  TEXT NOT NULL,
    fingerprint  TEXT NOT NULL,
    reference    TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (scope, request_key)
) STRICT;
CREATE INDEX request_keys_by_age ON request_keys (created_at);

-- The last committed body of each view.
CREATE TABLE views (
    view_ref         TEXT PRIMARY KEY NOT NULL,
    revision         INTEGER NOT NULL CHECK (revision >= 1),
    kind             TEXT NOT NULL,
    recorded_at      INTEGER NOT NULL,
    source_run_id    TEXT,
    source_kind      TEXT NOT NULL,
    definition_hash  TEXT NOT NULL,
    data_hash        TEXT NOT NULL,
    data_bytes       INTEGER NOT NULL,
    data_json        TEXT NOT NULL
) STRICT;

CREATE TABLE view_log_items (
    view_ref     TEXT NOT NULL REFERENCES views (view_ref) ON DELETE CASCADE,
    log_seq      INTEGER NOT NULL,
    recorded_at  INTEGER NOT NULL,
    item_json    TEXT NOT NULL,
    PRIMARY KEY (view_ref, log_seq)
) STRICT;

CREATE TABLE schedule_settings (
    action_ref  TEXT PRIMARY KEY NOT NULL,
    enabled     INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    updated_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE artifacts (
    run_id         TEXT NOT NULL REFERENCES runs (run_id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    bytes          INTEGER NOT NULL CHECK (bytes >= 0),
    sha256         TEXT NOT NULL,
    over_budget    INTEGER NOT NULL CHECK (over_budget IN (0, 1)),
    registered_at  INTEGER NOT NULL,
    PRIMARY KEY (run_id, name)
) STRICT;

-- Views removed by retention, so reads say "cleaned up" instead of "no data".
CREATE TABLE cleaned_views (
    view_ref    TEXT PRIMARY KEY NOT NULL,
    revision    INTEGER NOT NULL,
    cleaned_at  INTEGER NOT NULL
) STRICT;
"#;
