---
name: config-secrets
description: Implement validated configuration, mappings, and secret redaction after scaffolding.
model: sonnet
color: yellow
---

Implement configuration and secret-handling infrastructure.

Dependencies: `rust-scaffold`, `domain-model`, and architecture contracts.

Tasks:
1. Define typed configuration for roots, schedules, APIs, mappings, and policies:
   `download_roots`, `media_roots`, `excluded_roots`, `path_mappings`, `expected_owner`
   (UID/GID), thresholds, `dry_run` (default on), and `auto_mode` (default off).
2. Load non-secret defaults from files and secrets from environment variables.
3. Validate root separation, report placement, extensions, thresholds, and intervals.
4. Implement bounded container-to-host path mappings with normalization.
5. Support explicit trusted-proxy CIDRs for client IP resolution.
6. Redact secrets, credentials, tracker parameters, and sensitive headers.
7. Provide startup diagnostics that identify missing fields without exposing values.
8. Add a safe example configuration and environment template.

Security rules:
- Reject traversal, ambiguous prefixes, symlink escapes, and mappings outside roots.
- The configured excluded personal root must never be scanned or normalized into scope.
  The container mounts the common parent of all roots, so excluded roots are visible:
  refuse to start if an excluded root overlaps a scanned root ambiguously.
- Debug formatting must not reveal secret values.
- Do not read historical secrets from legacy scripts.

Tests:
- Precedence, invalid values, redaction, path escape, and trusted-proxy cases.
- Property tests for mapping round trips and confinement.

Handoff with the complete configuration contract for all connector agents.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/05-config-secrets.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
