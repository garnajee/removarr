-- 0001: observation side. Current filesystem state, torrent snapshot, ARR snapshots,
-- match funnel output, issues and the ignore list. Forward-only: never edit after release.
-- Conventions: instants are UTC unix milliseconds; u64 values are stored as the same 64 bits in
-- INTEGER; JSON columns hold canonical JSON (sorted keys); paths are (root, rel), never absolute.

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

-- Small key/value settings ("auto_paused", ...). Skips writes when the value is unchanged.
CREATE TABLE IF NOT EXISTS app_state (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS scan_sessions (
    id          TEXT PRIMARY KEY,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    status      TEXT NOT NULL CHECK (status IN
        ('running','complete','complete_with_errors','cancelled','interrupted','failed')),
    metrics     TEXT NOT NULL,
    CHECK ((status = 'running') = (finished_at IS NULL))
) STRICT;
CREATE INDEX IF NOT EXISTS scan_sessions_started ON scan_sessions (started_at);

CREATE TABLE IF NOT EXISTS session_roots (
    session_id         TEXT NOT NULL REFERENCES scan_sessions (id),
    root               TEXT NOT NULL,
    scanned            INTEGER NOT NULL CHECK (scanned IN (0, 1)),
    error_count        INTEGER NOT NULL,
    protected_count    INTEGER NOT NULL,
    root_sig           TEXT,
    inventory_saved    INTEGER NOT NULL DEFAULT 0 CHECK (inventory_saved IN (0, 1)),
    inventory_finished INTEGER NOT NULL DEFAULT 0 CHECK (inventory_finished IN (0, 1)),
    PRIMARY KEY (session_id, root)
) STRICT, WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS session_services (
    session_id  TEXT NOT NULL REFERENCES scan_sessions (id),
    service     TEXT NOT NULL,
    state       TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, service)
) STRICT, WITHOUT ROWID;

-- Current state per (root, rel). Rows are only written when the signature changed.
CREATE TABLE IF NOT EXISTS fs_entries (
    root            TEXT NOT NULL,
    rel             TEXT NOT NULL,
    kind            TEXT NOT NULL,
    dev             INTEGER NOT NULL,
    ino             INTEGER NOT NULL,
    size            INTEGER NOT NULL,
    nlink           INTEGER NOT NULL,
    mtime_ns        INTEGER NOT NULL,
    mode            INTEGER NOT NULL,
    uid             INTEGER NOT NULL,
    gid             INTEGER NOT NULL,
    allocated       INTEGER NOT NULL,
    symlink_target  TEXT,
    first_seen_at   INTEGER NOT NULL,
    changed_session TEXT NOT NULL REFERENCES scan_sessions (id),
    gone_session    TEXT REFERENCES scan_sessions (id),
    gone_at         INTEGER,
    PRIMARY KEY (root, rel),
    CHECK ((gone_session IS NULL) = (gone_at IS NULL))
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS fs_entries_phys ON fs_entries (dev, ino);
CREATE INDEX IF NOT EXISTS fs_entries_gone ON fs_entries (gone_at) WHERE gone_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS scan_errors (
    session_id TEXT NOT NULL REFERENCES scan_sessions (id),
    root       TEXT NOT NULL,
    rel        TEXT NOT NULL,
    io         TEXT NOT NULL,
    message    TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS scan_errors_session ON scan_errors (session_id, root);

CREATE TABLE IF NOT EXISTS protected_zones (
    session_id TEXT NOT NULL REFERENCES scan_sessions (id),
    root       TEXT NOT NULL,
    rel        TEXT NOT NULL,
    reason     TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS protected_zones_session ON protected_zones (session_id, root);

-- Current torrent snapshot, upsert-if-changed. `info` and `files` are canonical JSON.
CREATE TABLE IF NOT EXISTS torrents (
    hash     TEXT PRIMARY KEY,
    name     TEXT NOT NULL,
    state    TEXT NOT NULL,
    category TEXT NOT NULL,
    size     INTEGER NOT NULL,
    info     TEXT NOT NULL,
    files    TEXT NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS torrent_snapshot (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    session_id TEXT NOT NULL REFERENCES scan_sessions (id),
    taken_at   INTEGER NOT NULL,
    version    TEXT NOT NULL,
    major      INTEGER NOT NULL
) STRICT;

-- Persisted funnel output. The session that produced the current set is meta.match_session.
CREATE TABLE IF NOT EXISTS match_candidates (
    a_root   TEXT NOT NULL,
    a_rel    TEXT NOT NULL,
    b_root   TEXT NOT NULL,
    b_rel    TEXT NOT NULL,
    sig_a    TEXT NOT NULL,
    sig_b    TEXT NOT NULL,
    identity TEXT NOT NULL,
    content  TEXT NOT NULL,
    reasons  TEXT NOT NULL,
    PRIMARY KEY (a_root, a_rel, b_root, b_rel)
) STRICT, WITHOUT ROWID;

-- One row per distinct (media, fingerprint). A repeat only advances captured_at.
CREATE TABLE IF NOT EXISTS arr_snapshots (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    service     TEXT NOT NULL,
    media_key   TEXT NOT NULL,
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) > 0),
    captured_at INTEGER NOT NULL,
    body        TEXT NOT NULL,
    UNIQUE (media_key, fingerprint)
) STRICT;
CREATE INDEX IF NOT EXISTS arr_snapshots_media ON arr_snapshots (media_key, captured_at);

-- Current issues. `resolved_at` is set when a later evaluation no longer reports the issue.
-- `body_hash` hashes the content without session and first_seen_at, to skip unchanged rows.
CREATE TABLE IF NOT EXISTS issues (
    id             TEXT PRIMARY KEY CHECK (length(id) = 32),
    category       TEXT NOT NULL,
    subject        TEXT NOT NULL,
    severity       TEXT NOT NULL,
    outcome        TEXT NOT NULL,
    unit           TEXT NOT NULL,
    evidence       TEXT NOT NULL,
    blockers       TEXT NOT NULL,
    torrent_hashes TEXT NOT NULL,
    physical_ids   TEXT NOT NULL,
    logical_bytes  INTEGER NOT NULL,
    potential_bytes INTEGER NOT NULL,
    age_basis      INTEGER NOT NULL,
    first_seen_at  INTEGER NOT NULL,
    resolved_at    INTEGER,
    body_hash      TEXT NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS issues_category ON issues (category) WHERE resolved_at IS NULL;
CREATE INDEX IF NOT EXISTS issues_resolved ON issues (resolved_at) WHERE resolved_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS ignores (
    subject_key TEXT PRIMARY KEY,
    reason      TEXT,
    created_at  INTEGER NOT NULL,
    restored_at INTEGER
) STRICT, WITHOUT ROWID;
