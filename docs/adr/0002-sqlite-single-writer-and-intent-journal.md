# ADR-0002: SQLite via sqlx, single writer with synchronous=FULL, intent journal in SQLite

Status: accepted (wave 1). Details: `docs/architecture.md` section 9.

## Context
MASTER §3 requires SQLite via sqlx on the config volume, WAL, batched commits and no frantic
flushing, with an fsync only for the intent journal. INV-08 requires every intent to be durable
before its mutation.

## Decision
- One writer pool (`max_connections = 1`, WAL, `synchronous = FULL`, `foreign_keys = ON`,
  `busy_timeout = 5000`) and one read-only reader pool (4 connections).
- The intent journal is two tables (`journal_intents`, `journal_results`). `record_intent` returns
  after `COMMIT`, which fsyncs the WAL under `synchronous = FULL`. There is no separate JSONL file.
- Writes are batched (1000-row inventory diff transactions, stage-only job updates, unchanged
  rows skipped), so FULL costs only a few fsyncs per scan.
- Runtime queries (`query_as` + `FromRow`), embedded forward-only migrations (`sqlx::migrate!`).
  No compile-time `query!` macros.
- Each port method is one transaction. No transaction handle crosses a port.

## Consequences
- One durable store for plans, journal, gains and jobs, so recovery reads a single source.
- Writes are serialized, which is acceptable for a single-user app with one execution lane.
- The schema is coupled to `reporting`'s SQL. Schema changes must update `reporting` queries
  (tests catch it).
