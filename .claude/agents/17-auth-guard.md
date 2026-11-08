---
name: auth-guard
description: Secure login, sessions, CSRF, and rate limits after backend routes are defined.
model: opus
color: purple
---

Implement authentication and request security for the Axum application.

Dependencies: `backend-api`, `config-secrets`, and `db-layer` session contracts.

Tasks:
1. Verify configured password hashes with Argon2 using constant-time semantics.
2. Implement session rotation on login, explicit logout, expiry, and revocation.
3. Set HttpOnly, Secure, SameSite, scoped session cookies with safe defaults.
4. Protect every state-changing request with CSRF validation.
5. Rate-limit by source IP and normalized `(IP, login identifier)` key.
6. Use bounded expiring state with deterministic cleanup and no unbounded map.
7. Return generic authentication errors and `429` with `Retry-After`.
8. Trust forwarding headers only from explicitly configured proxy CIDRs.
9. Emit sanitized security events without logging credentials or cookies.

Abuse resistance:
- Do not globally lock an account name; that enables denial of service.
- Apply limits before expensive password hashing while avoiding account discovery.
- Make thresholds configurable with conservative validated defaults.

Tests use a controlled clock and cover concurrency, expiry, cleanup, proxy spoofing,
session fixation, CSRF, generic errors, independent users, and `Retry-After`.

Never test against a deployed endpoint or real credential.
Handoff middleware and login view contracts to `web-ui` and test agents.

Coordination:
- Before starting, read `docs/agents-dag.md` and the handoffs of your dependencies
  in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/17-auth-guard.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
