# ADR-0006: Server-rendered UI with Askama + HTMX; private-cookie session ids in memory

Status: accepted (wave 1). Details: `docs/architecture.md` section 12.

## Context
MASTER §3, §6 and §8: Axum, Askama, HTMX as the only JavaScript, server-side SVG, single-user
form login with an Argon2 hash in env, encrypted HttpOnly SameSite session, CSRF, login rate
limit, minimal RAM and disk writes.

## Decision
- Axum 0.8 routes render Askama 0.14 templates into `Html<String>`. `HX-Request` selects a
  fragment. Every action works without JavaScript (progressive enhancement). HTMX is vendored and
  embedded. CSP is `script-src 'self'`, with no eval and no `hx-on`.
- Sessions: an `axum-extra` `PrivateCookieJar` (AES-GCM, key from `REMOVARR_SESSION_KEY`) carries a
  random 32-byte session id. A bounded in-memory session table (max 32) holds expiry and the
  CSRF token. There is no session persistence, so a restart logs out.
- CSRF uses a synchronizer token (hidden field + `X-CSRF-Token`), a double-submit cookie for
  `/login`, and an `Origin`/`Referer` check.
- The login rate limit uses bounded in-memory windows per IP and per (IP, username), applied
  before Argon2.

## Consequences
- No session writes to SQLite, and no SPA or JS build chain.
- Single process only. Horizontal scaling is not supported (not a goal).
- Users must log in again after each container restart.
