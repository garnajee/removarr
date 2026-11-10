-- 0002: action side. Plans, actions, approvals, intent journal, gains, jobs.
-- Audit evidence (plans, plan_actions, approvals, live journal rows, gains, observed_deltas) is
-- protected by triggers: it can never be deleted, and its identifying content never changes.

CREATE TABLE IF NOT EXISTS plans (
    id         TEXT PRIMARY KEY,
    hash       TEXT NOT NULL UNIQUE CHECK (length(hash) = 64),
    status     TEXT NOT NULL CHECK (status IN ('draft','approved','executing','executed',
        'partially_executed','failed','expired','invalidated','discarded','interrupted')),
    mode       TEXT NOT NULL CHECK (mode IN ('dry_run','live')),
    origin     TEXT NOT NULL CHECK (origin IN ('user','auto_policy')),
    session_id TEXT NOT NULL REFERENCES scan_sessions (id),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    body       TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS plans_status ON plans (status);

CREATE TABLE IF NOT EXISTS plan_actions (
    plan_id    TEXT NOT NULL REFERENCES plans (id),
    idx        INTEGER NOT NULL CHECK (idx >= 0),
    kind       TEXT NOT NULL,
    state      TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','running','verifying',
        'verified','executed_unverified','compensating','compensated','failed',
        'failed_needs_review','not_executed','simulated')),
    outcome    TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (plan_id, idx)
) STRICT, WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS approvals (
    plan_id                    TEXT PRIMARY KEY REFERENCES plans (id),
    plan_hash                  TEXT NOT NULL REFERENCES plans (hash),
    approved_at                INTEGER NOT NULL,
    approved_by                TEXT NOT NULL CHECK (approved_by IN ('user','auto_policy')),
    confirmed_count            INTEGER NOT NULL,
    confirmed_torrent_removals INTEGER NOT NULL,
    acknowledged_samples       TEXT NOT NULL,
    acknowledged_unrecoverable TEXT NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS simulation_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id     TEXT NOT NULL REFERENCES plans (id),
    started_at  INTEGER NOT NULL,
    finished_at INTEGER NOT NULL
) STRICT;

-- Intent journal. A live intent is committed (WAL fsync, synchronous=FULL) before the mutation.
CREATE TABLE IF NOT EXISTS journal_intents (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT NOT NULL UNIQUE,
    plan_id       TEXT NOT NULL,
    action_idx    INTEGER NOT NULL,
    step          TEXT NOT NULL,
    mode          TEXT NOT NULL CHECK (mode IN ('live','simulated')),
    target        TEXT NOT NULL,
    observed      TEXT NOT NULL,
    expected_gain TEXT NOT NULL,
    hashes        TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    FOREIGN KEY (plan_id, action_idx) REFERENCES plan_actions (plan_id, idx)
) STRICT;
CREATE INDEX IF NOT EXISTS journal_intents_plan ON journal_intents (plan_id, action_idx);
CREATE INDEX IF NOT EXISTS journal_intents_live ON journal_intents (mode, seq);

CREATE TABLE IF NOT EXISTS journal_results (
    intent_id TEXT PRIMARY KEY REFERENCES journal_intents (id),
    outcome   TEXT NOT NULL,
    at        INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- One row per unique physical file version (gain_id). A duplicate is rejected: no double count.
CREATE TABLE IF NOT EXISTS gains (
    gain_id     TEXT PRIMARY KEY CHECK (length(gain_id) = 32),
    plan_id     TEXT NOT NULL,
    action_idx  INTEGER NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN
        ('attributable_deletion','attributable_consolidation','added_cost')),
    dev         INTEGER NOT NULL,
    ino         INTEGER NOT NULL,
    bytes       INTEGER NOT NULL,
    recorded_at INTEGER NOT NULL,
    FOREIGN KEY (plan_id, action_idx) REFERENCES plan_actions (plan_id, idx)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS gains_plan ON gains (plan_id);

CREATE TABLE IF NOT EXISTS observed_deltas (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id             TEXT REFERENCES plans (id),
    root                TEXT NOT NULL,
    free_before         INTEGER NOT NULL,
    free_after_local    INTEGER NOT NULL,
    free_after_services INTEGER,
    recorded_at         INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS jobs (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    dedupe_key  TEXT NOT NULL,
    lane        TEXT NOT NULL CHECK (lane IN ('observe','execute')),
    status      TEXT NOT NULL CHECK (status IN
        ('queued','running','succeeded','failed','cancelled','interrupted')),
    enqueued_at INTEGER NOT NULL,
    started_at  INTEGER,
    finished_at INTEGER,
    stage       TEXT,
    attempts    INTEGER NOT NULL DEFAULT 0,
    outcome     TEXT
) STRICT;
CREATE UNIQUE INDEX IF NOT EXISTS jobs_active_dedupe ON jobs (dedupe_key)
    WHERE status IN ('queued', 'running');
CREATE INDEX IF NOT EXISTS jobs_queue ON jobs (lane, status, enqueued_at);

-- ---- audit protection -------------------------------------------------------------------

CREATE TRIGGER IF NOT EXISTS plans_no_delete BEFORE DELETE ON plans
BEGIN SELECT RAISE(ABORT, 'audit: plans are never deleted'); END;
CREATE TRIGGER IF NOT EXISTS plans_immutable BEFORE UPDATE OF
    id, hash, mode, origin, session_id, created_at, expires_at, body ON plans
BEGIN SELECT RAISE(ABORT, 'audit: plan content is immutable'); END;
CREATE TRIGGER IF NOT EXISTS plans_status_needs_approval BEFORE UPDATE OF status ON plans
WHEN NEW.status IN ('approved','executing','executed','partially_executed')
    AND NOT EXISTS (SELECT 1 FROM approvals WHERE plan_id = NEW.id AND plan_hash = NEW.hash)
BEGIN SELECT RAISE(ABORT, 'audit: plan status requires a matching approval'); END;

CREATE TRIGGER IF NOT EXISTS plan_actions_no_delete BEFORE DELETE ON plan_actions
BEGIN SELECT RAISE(ABORT, 'audit: plan actions are never deleted'); END;
CREATE TRIGGER IF NOT EXISTS plan_actions_immutable BEFORE UPDATE OF plan_id, idx, kind
    ON plan_actions
BEGIN SELECT RAISE(ABORT, 'audit: plan action identity is immutable'); END;

CREATE TRIGGER IF NOT EXISTS approvals_no_update BEFORE UPDATE ON approvals
BEGIN SELECT RAISE(ABORT, 'audit: approvals are immutable'); END;
CREATE TRIGGER IF NOT EXISTS approvals_no_delete BEFORE DELETE ON approvals
BEGIN SELECT RAISE(ABORT, 'audit: approvals are never deleted'); END;

CREATE TRIGGER IF NOT EXISTS intents_live_needs_executing BEFORE INSERT ON journal_intents
WHEN NEW.mode = 'live'
    AND (SELECT status FROM plans WHERE id = NEW.plan_id) IS NOT 'executing'
BEGIN SELECT RAISE(ABORT, 'audit: a live intent requires an executing plan'); END;
CREATE TRIGGER IF NOT EXISTS intents_no_update BEFORE UPDATE ON journal_intents
BEGIN SELECT RAISE(ABORT, 'audit: journal intents are immutable'); END;
CREATE TRIGGER IF NOT EXISTS intents_no_delete_live BEFORE DELETE ON journal_intents
WHEN OLD.mode = 'live'
BEGIN SELECT RAISE(ABORT, 'audit: live journal intents are never deleted'); END;

CREATE TRIGGER IF NOT EXISTS results_no_update BEFORE UPDATE ON journal_results
BEGIN SELECT RAISE(ABORT, 'audit: journal results are immutable'); END;
CREATE TRIGGER IF NOT EXISTS results_no_delete_live BEFORE DELETE ON journal_results
WHEN (SELECT mode FROM journal_intents WHERE id = OLD.intent_id) = 'live'
BEGIN SELECT RAISE(ABORT, 'audit: live journal results are never deleted'); END;

CREATE TRIGGER IF NOT EXISTS gains_no_update BEFORE UPDATE ON gains
BEGIN SELECT RAISE(ABORT, 'audit: gains are immutable'); END;
CREATE TRIGGER IF NOT EXISTS gains_no_delete BEFORE DELETE ON gains
BEGIN SELECT RAISE(ABORT, 'audit: gains are never deleted'); END;
CREATE TRIGGER IF NOT EXISTS deltas_no_update BEFORE UPDATE ON observed_deltas
BEGIN SELECT RAISE(ABORT, 'audit: observed deltas are immutable'); END;
CREATE TRIGGER IF NOT EXISTS deltas_no_delete BEFORE DELETE ON observed_deltas
BEGIN SELECT RAISE(ABORT, 'audit: observed deltas are never deleted'); END;
