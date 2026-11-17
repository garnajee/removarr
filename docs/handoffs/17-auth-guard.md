# Handoff — 17-auth-guard (wave 13)

## Delivered scope

Owned paths (architecture 15.2 row 17): `crates/web/src/auth/**` (new), `crates/web/tests/auth_flow.rs` (new), and the
allowed edits: `crates/web/templates/pages/login.html` (form), `crates/web/Cargo.toml` (`sha2`, `unicode-normalization`,
dev `tracing-subscriber`, all `workspace = true`), `crates/web/src/lib.rs` (`pub mod auth;` + doc line),
`crates/web/src/router.rs` (login routes from the guard, security-header layer), `crates/web/src/state.rs` (one defaulted
`Guard::routes` method), `crates/app/src/{wiring,serve,cli}.rs` (real guard, `hash-password`), `crates/app/Cargo.toml`
(`rustix` feature `termios`). `Cargo.lock` gained the three new edges of `removarr-web`. No root `Cargo.toml`, other crate,
locale, other template, Docker/CI or architecture file was edited.

| Module | Contents |
|---|---|
| `web::auth` | `Auth` (the production `Guard`), `AuthSettings::from_config(&Config)`, `AuthSetupError`, `AuthStats`, `security_headers(router)`, constants (`SESSION_COOKIE`, `LOGIN_COOKIE`, `LOGIN_TOKEN_MS`, `CSP`, `SECURITY_TARGET`, `MAX_USERNAME_BYTES`, `MIN_SESSION_KEY_CHARS`), re-export `hash_password` |
| `web::auth::password` | `Verifier` (PHC parsed once; Argon2 in `spawn_blocking` behind a 1-permit semaphore; constant-time output comparison by `password-hash`), `hash_password` (Argon2id m=19456 KiB, t=2, p=1, 16-byte OS-random salt), `MAX_PASSWORD_BYTES = 1024` |
| `web::auth::session` | `SessionStore`: max 32 sessions, absolute TTL `auth.session_ttl_hours`, idle 2 h, purge on every call, LRU eviction when full, `remove`, `clear` |
| `web::auth::limiter` | `LoginLimiter`: fixed 15-min windows per IP and per `(IP, normalized user)`, atomic reservation before Argon2, refund on success, `BTreeMap` tables capped at 4096 keys each (oldest window evicted), purge on every call, IPv6 grouped by /64 |
| `web::auth::request` | `client_ip` (trusted-proxy aware), `same_origin` (Origin, else Referer, vs Host / trusted `X-Forwarded-Host`), `presented_token` (header or urlencoded field), `form_value`, `ct_eq`, hex helpers, `random32` |
| `web::auth::login` (private) | `GET /login`, `POST /login`, `POST /logout` |
| `app::wiring::auth_guard(&Config)` | builds `Auth` with `SystemClock`; `serve` uses it, a failure exits 1 (`startup failed: REMOVARR_PASSWORD_HASH is not a valid Argon2 PHC string ...`, key names only) |
| `app::cli` | `removarr hash-password` (stdin only, echo off on a TTY, prints the PHC on stdout) and `hash_line` |

### Behaviour

* **Fail closed.** `REMOVARR_PASSWORD_HASH` and `REMOVARR_SESSION_KEY` are required by `config` (exit 2 when missing). `Auth::new`
  additionally parses the PHC string for real (Argon2 family, valid parameters, salt and output present), checks the key length
  (>= 64 chars), the user name (1..=128 bytes) and non-zero limits; any failure refuses startup (exit 1). There is no
  "auth disabled" mode. `wiring::default_guard()` (refuse everything) is kept for tests only.
* **Sessions.** On successful login a new random 32-byte id is always issued (encrypted `removarr_session` cookie: `HttpOnly`,
  `SameSite=Strict`, `Path=/`, `Max-Age` = TTL, `Secure` unless `auth.cookie_secure=false`, no `Domain`). Any session id the
  browser presented is removed first (fixation). Logout, absolute/idle expiry, LRU eviction and `Auth::revoke_all()` remove
  entries; a restart revokes everything (ADR-0006). Cookie key = SHA-512("removarr session cookie key v1\0" || secret), split by
  `cookie::Key` into HMAC and AES-GCM halves; tampered or forged cookies decrypt to nothing.
* **CSRF.** Commands (`Access::Command`, and `/logout`): valid session, then same origin, then the session's synchronizer token in
  `X-CSRF-Token` (if the header is present it must match; no fallback) or, for `application/x-www-form-urlencoded` bodies, the
  single `csrf_token` field (body buffered up to 1 MiB and passed on unchanged). `/login`: same origin, then a double-submit
  token (encrypted cookie `removarr_login_csrf` = `token.issued_ms`, `Path=/login`, 1 h) equal to the `csrf_token` field.
  Repeated or badly encoded fields are refused.
* **Login order.** origin -> form token -> rate-limit reservation -> length checks -> Argon2 (run for every well-formed attempt,
  whatever the user name: no timing oracle) -> constant-time user-name comparison -> rotation. Every credential failure is the same
  `401` page with key `login.failed`. Rate limited: `429`, `Retry-After: <secs>` (ceil, >= 1), key `error.rate_limited`, no
  hashing. Nothing is keyed by the user name alone (no global account lockout).
* **Client IP.** `TrustedProxies::client_ip(peer, every X-Forwarded-For line)`: headers are ignored unless the TCP peer is in
  `server.trusted_proxies`; then the right-most untrusted entry. A trusted peer may also supply a single-valued
  `X-Forwarded-Host` for the origin check.
* **Security headers** on every response (also 404, 5xx, health): the CSP of architecture 12.3, `X-Content-Type-Options:
  nosniff`, `Referrer-Policy: same-origin`, `X-Frame-Options: DENY` (set only when absent).
* **Security events** (`target: removarr::security`): `login_succeeded` (INFO), `login_failed`, `login_rate_limited`
  (`retry_after_secs`), `csrf_rejected`, `origin_rejected` (WARN), `session_expired`, `session_evicted`, `logout`,
  `sessions_revoked` (INFO). Fields: event name and `client_ip` only. Never the submitted user name, password, token, cookie or
  hash (asserted by a log-capture test).

## Contract for `web-ui` (19)

* **Login view.** `GET /login` is now `auth::login` (public). It renders the existing `handlers::login::{LoginPage, LoginView}`
  (`LoginView { error_key: Option<&'static str>, retry_after: Option<u64> }`, template `pages/login.html`, standalone page,
  never a fragment). `ctx.chrome.csrf` holds the **login form token** (non-empty only when the real guard serves the page; the
  `FailClosedGuard` placeholder leaves it empty and shows `login.unavailable`). `ctx.chrome.user` is empty, `dry_run` false.
* The form must `POST /login` as `application/x-www-form-urlencoded` (<= 8 KiB) with exactly the fields `csrf_token` (hidden, =
  `ctx.chrome.csrf`), `username`, `password`. Do not prefill or echo the user name. The current minimal form in `login.html` is
  functional; 19 owns its markup and styling but must keep these field names, the action and the `{% if
  !ctx.chrome.csrf.is_empty() %}` guard.
* **Outcomes:** `303 /` (success; sets `removarr_session`, removes `removarr_login_csrf`); `401` + `login.failed`; `403` +
  `error.forbidden` (form token missing/expired/superseded, cross-origin); `429` + `error.rate_limited` + `retry_after`
  (also the `Retry-After` header); `413` above 8 KiB. Every refusal re-renders the form with a fresh token.
* **Locale keys to add (19 owns `locales/`)**: `login.username`, `login.password`, `login.submit`, `login.failed` (generic, e.g.
  "Sign-in failed. Check your user name and password."), and `logout.submit` for the logout button. Until then `Tr::t` shows the
  key itself.
* **Logout button** (in `base.html`, 19's): `<form method="post" action="/logout"><input type="hidden" name="csrf_token"
  value="{{ ctx.chrome.csrf }}"><button>...</button></form>`. Result: `303 /login` (HTMX: `200` + `HX-Redirect: /login`) and the
  cookie removed; without a session `401`, without a valid token or same origin `403`.
* **Every other form** keeps the hidden `csrf_token = ctx.chrome.csrf`; HTMX requests may instead send `X-CSRF-Token` via
  `hx-headers` on `<body>` (if the header is present it wins and must be right). The token is stable for the session.
* **Expired session:** pages `303 /login`; HTMX `401` + `HX-Redirect: /login`; commands `401`.
* **CSP:** `script-src 'self'` and `style-src 'self'` forbid inline `<script>`, `<style>`, `style="..."` and `hx-on`; use
  `/static/*` files and the HTMX meta config of architecture 12.3. `img-src` allows `data:`.

## Contract for tests (21, 22, 23)

* Build: `Auth::new(AuthSettings { username, password_hash: Secret, session_key: Secret (>= 64 chars), session_ttl_hours,
  cookie_secure, login_limit_per_ip, login_limit_per_ip_user, trusted_proxies }, Arc<dyn Clock>)`, then `AppState.guard =
  Arc::new(auth.clone())`. Use a cheap Argon2 PHC (for example `Params::new(256, 1, 1, None)`) over a fake password; never a real
  credential. `auth.stats()` gives `sessions`, `limiter_keys`, `password_checks`; `auth.revoke_all()`.
* Requests must carry `ConnectInfo<SocketAddr>` (as `serve` does), `Host`, and for POSTs a matching `Origin` (or `Referer`).
  Without `ConnectInfo` all requests share the `0.0.0.0` rate-limit bucket.
* Constants: `session::{MAX_SESSIONS = 32, IDLE_MS = 2 h}`, `limiter::{WINDOW_MS = 15 min, DEFAULT_CAPACITY = 4096}`,
  `LOGIN_TOKEN_MS = 1 h`. `crates/web/tests/auth_flow.rs` has a reusable cookie-jar client and fakes.

## Deviations from the architecture

1. **`Guard::routes()`** (state.rs, defaulted to `None`): the guard serves `GET/POST /login` and `POST /logout` itself, merged
   into the public group by `router.rs`; `None` keeps 16's placeholder `GET /login`. An `auth` field on `AppState` would have
   broken 16's literal `AppState` constructions in `handlers/tests.rs`, which 17 may not edit. `/logout` is therefore not in the
   command group; it runs the same `Access::Command` check (session, origin, CSRF) itself.
2. The window length is a constant (15 min); only the counts are configurable (`auth.login_limit_per_ip` 20,
   `_per_ip_user` 5, validated 1..=1000 by config, which is read-only after wave 4).
3. Rate-limit tables are capped (4096 keys each) and evict the oldest window when full rather than refusing new clients
   (refusing would let an IP flood lock out the owner). IPv6 is keyed per /64. A success refunds the reservation.
4. Login form token: encrypted cookie with an issue time (1 h), `Path=/login`, removed on success; not single-use.
5. `X-Forwarded-Host` is honoured for the origin check only from a trusted proxy and only when single-valued.
6. A command without `X-CSRF-Token` whose urlencoded body exceeds 1 MiB is refused with `403` (the guard cannot read the token)
   instead of the handler's `413`; with the header the handler's `413` is unchanged.
7. `handlers::login::page` (16's file) stays: it is the placeholder used when the guard serves no routes.
8. Revocation is an API (`Auth::revoke_all`), not a route; sessions are also revoked by logout, expiry and restart.
9. The cookie key is derived with SHA-512 over the whole `REMOVARR_SESSION_KEY` (not the raw first 64 bytes).
10. The session cookie keeps the architecture name `removarr_session` (no `__Host-` prefix, which would be impossible with
    `cookie_secure=false`).
11. The password lives in a plain `String` until the verification task ends (no `zeroize` dependency in the workspace).

## Open TODOs

* 19: locale keys and logout button above; final login markup (keep field names and the token guard).
* 20/README: document `removarr hash-password` (`docker run --rm -it <image> hash-password`, or pipe the password on stdin), the
  `$$` escaping in compose, `REMOVARR_COOKIE_SECURE=false` only for plain-HTTP LAN, and `REMOVARR_TRUSTED_PROXIES` for a reverse
  proxy (which must preserve `Host` or send `X-Forwarded-Host`).
* 22/23: brute-force and spoofing scenarios over a real listener (`into_make_service_with_connect_info`), and Argon2 latency with
  production parameters on the target hardware.
* Orchestrator: no root `Cargo.toml` change was needed.

## Tests

* Unit (in `crates/web/src/auth/`): limiter (exact `Retry-After` and expiry, refused attempts not counted, identifiers independent
  but sharing the IP budget, normalization, IPv6 /64, refund, deterministic purge, bounded tables with oldest eviction); sessions
  (valid, idle, absolute TTL over activity, LRU bound, purge/remove/clear); request helpers (same-origin matrix, trusted
  `X-Forwarded-Host`, form decoding and ambiguity, constant-time equality, hex, randomness); password (verify, unusable hashes,
  `hash_password` round trip with production parameters).
* Integration (`crates/web/tests/auth_flow.rs`, real router, fakes, `FakeClock`): hardened cookies and security headers;
  `cookie_secure=false`; session + CSRF header/field/Referer, rebuilt body reaches the handler; **every destructive route** refused
  without session (401), without/with wrong/foreign/overridden token, cross-origin, or without Origin/Referer (403), zero
  mutations; logout needs CSRF and kills copies of the cookie; login form CSRF (missing, superseded, repeated, expired,
  cross-origin) with no hashing; session fixation and forged/tampered cookies; idle and absolute expiry (HTMX 401 +
  `HX-Redirect`); revocation; generic failures with identical bodies and Argon2 run for unknown names; 429 + `Retry-After`
  900/300 before hashing, then recovery; per-IP budget, independent IPs and users, no account lockout; X-Forwarded-For ignored
  from untrusted peers, honoured from a trusted proxy (spoofed left entries ignored); 40 concurrent attempts -> exactly 5
  hashed, 35 x 429; 40 concurrent logins -> 40 distinct ids, table bounded at 32; limiter cleanup after the window; log capture
  free of password, user name, tokens, cookies, hash and session key; invalid settings refuse to start without echoing values.
* `crates/app`: `hash-password` parsing (never from argv) and `hash_line`.

## Commands run and results (host target, macOS)

* `cargo fmt --all --check`: pass.
* `cargo clippy --workspace --all-targets -- -D warnings`: pass.
* `cargo test --workspace`: pass, 919 passed, 0 failed, 1 ignored (opt-in real ffprobe). New: 21 unit tests in
  `web::auth`, 19 in `crates/web/tests/auth_flow.rs` (stable over repeated runs), 1 in `app::cli`. 16's route tests are
  unchanged and pass (the `FailClosedGuard` placeholder login page still renders).
* `cargo build --workspace --release`: pass.
* Smoke: `printf 'fake-smoke-password\n' | target/release/removarr hash-password` prints an `$argon2id$v=19$m=19456,t=2,p=1$...`
  string (exit 0); an argument after `hash-password` exits 64; an empty line exits 1.
* No network beyond crates.io (no new crate downloaded); no deployed endpoint or real credential used; nothing committed.
