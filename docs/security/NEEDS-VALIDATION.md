# Needs validation: removarr-v2 run-1 (source ref cb62ef1)

Prioritized leads without severity. None is a confirmed vulnerability. Do not test any live service; every step below is local to a test harness or an owner-observed configuration fact.

## 1. `web-login-limiter-shared-bucket-without-trusted-proxies`

**Title:** Login limiter collapses to one shared bucket behind a reverse proxy when trusted_proxies is unset.
**Suggested owner (docs/architecture.md section 15.2):** `17-auth-guard` (limiter), `05-config-secrets` (default and startup warning), `20-devops` (compose/README guidance).
**Affected boundary:** unauthenticated network client versus availability of the operator's login.

**Claim.** `client_ip` returns the TCP peer unless the peer is in `server.trusted_proxies`, which defaults to empty. Behind a reverse proxy (the shipped compose publishes only `127.0.0.1:8012`, implying a host proxy) every client shares the proxy's address, so both the per-IP window (default 20 per 15 min) and the per-(IP, username) window (default 5 per 15 min) are global. A remote client sending 5 failed logins with the operator's username (default `admin`) per window keeps the operator's login at 429. `reserve()` runs before verification, so a correct password is also refused. Existing sessions are unaffected. The README (lines 55, 101) and a compose comment document setting `REMOVARR_TRUSTED_PROXIES`; no startup warning exists.

**Source trace (verified by two fresh verifiers):**
1. `crates/web/src/auth/login.rs:131` login handler takes the client address from `client_ip`.
2. `crates/web/src/auth/request.rs:22` `client_ip` returns the ConnectInfo peer unless the peer is trusted.
3. `crates/config/src/proxy.rs:60` `TrustedProxies::client_ip` returns the peer for an empty set.
4. `crates/config/src/load.rs:426` default is `TrustedProxies::none()`; limits at `load.rs:859` (20) and `load.rs:867` (5).
5. `crates/web/src/auth/limiter.rs:135` `LoginLimiter::reserve` refuses when either window is full, before Argon2; `login.rs:166` calls it before verification; `login.rs:213` `succeeded()` only after success.

**Evidence also read:** `docker-compose.yml:31`, `README.md:101`, `.env.example:44`.

**Exact blockers (not answerable from source):**
1. Whether the operator places a reverse proxy in front of the container.
2. Whether `REMOVARR_TRUSTED_PROXIES` or `server.trusted_proxies` is set to the proxy address or network.
3. Which peer address the container sees under Docker port publishing (bridge gateway versus the proxy address).
4. Whether an attacker can sustain 5 failed logins (operator username) or 20 (any names) per 15-minute window through the proxy; a proxy or WAF rate limit may prevent it.

**Bounded local resolution (no network).** In `crates/web/tests/auth_flow.rs`, using the existing World/Client harness (`settings()` with `TrustedProxies::none()`, `Client.send` inserting `ConnectInfo(peer)`, the `xff` field, `FakeClock`, in-process router): with a fixed peer such as `172.18.0.1:5000`, send 5 bad-password POSTs for the operator username, each with a different `X-Forwarded-For`, expect 401; then submit the correct credentials with another `X-Forwarded-For`. Expected: 429 with `Retry-After`. Also send bad logins for other usernames to confirm the per-IP window of 20 trips the same way. Repeat with `trusted_proxies` parsed from `172.18.0.0/16` and show that distinct XFF clients get separate buckets and the correct login returns 303. Advance `FakeClock` past 15 minutes to confirm the window expires.

**Owner-observed deployment check (no probing).** (1) Is the container reached through a reverse proxy? (2) Is `REMOVARR_TRUSTED_PROXIES` / `server.trusted_proxies` set to the proxy's address or network? (3) In existing `login_failed`, `login_succeeded` or `login_rate_limited` log lines from the owner's own normal use, is `client_ip` the proxy or Docker gateway address? If it is and the setting is empty, the exposure applies. Smallest fix if confirmed: warn at startup when the peer of the first request looks like a private proxy with `trusted_proxies` empty, and/or key the limiter on a trusted `X-Forwarded-For` only; at minimum make the compose example set `REMOVARR_TRUSTED_PROXIES` explicitly.
