# Handoff — 05-config-secrets (wave 4)

## Delivered scope

`crates/config` (crate `removarr-config`), `examples/removarr.toml`, `.env.example`. Dependencies unchanged
(root `Cargo.toml`, crate manifest and `Cargo.lock` untouched). Domain and storage untouched.

| Module | Contents |
|---|---|
| `load` | `Config::load_from_process()`, `Config::load_with_env(&Env)` (resolves `REMOVARR_CONFIG_FILE`), `Config::from_sources(Option<&str>, &Env)` (pure merge + validation; what tests use) |
| `file` | TOML schema (`deny_unknown_fields`, no secret fields) |
| `model` | `Config` and its typed sections (below), `fingerprint()`, `scrub()`, `startup_summary()`, `live_mode()` |
| `roots` | `Roots`, `Root`, `RootPaths`, validation, `locate`, `abs_path`, data-dir and symlink checks |
| `mapping` | `PathMapper`, `MapRule` |
| `proxy` | `TrustedProxies` |
| `redact`, `secret` | `Secret`, `REDACTED`, `redact_url`, `redact_tracker_url`, `redact_header`, `redact_text`, `is_sensitive_key/header` |
| `live` | `LiveMode` (private constructor; `LiveMode::for_tests()` behind feature `test-support`) |
| `error` | `ConfigError { problems: Vec<Problem> }`, `Problem { key, env, kind: Missing | Invalid(reason) }`, `EXIT_CODE_CONFIG = 2` |

## Public contract for connector agents

### Entry points
```rust
let cfg = removarr_config::Config::load_from_process()?;   // Err(ConfigError): print it, exit(err.exit_code()) == 2
cfg.live_mode() -> Option<LiveMode>        // None while dry_run (the only constructor)
cfg.roots() -> &Roots
cfg.paths: PathMapper                       // use this for every service path
cfg.fingerprint() -> String                 // sha256 hex, Precondition::ConfigFingerprint
cfg.scrub(&str) -> String                   // strips configured secret values + redact_text; use on upstream error bodies
cfg.startup_summary() -> Vec<String>        // non-secret log lines
```
`Config` is `Debug + Clone` and safe to log (`Secret` prints `Secret([REDACTED])`). It is not `Serialize`.

### Typed fields (all `pub`)
- `server: ServerConfig { bind: SocketAddr, data_dir: PathBuf (canonical, outside roots), log_level: LogLevel, trusted_proxies: TrustedProxies, shutdown_grace_seconds: u64 }`
- `paths: PathMapper` (owns the validated `Roots`)
- `owner: Owner { uid, gid, from_process }` (default = process effective ids)
- `thresholds: Thresholds { incomplete_stalled_days, auto_min_age_hours, hide_recent_days, recheck_timeout_seconds, plan_ttl_hours }` (all `u32`)
- `files: FileRules { video_extensions, junk_extensions, subtitle_extensions }` (lower-case, dot-prefixed; `is_video/is_junk/is_subtitle(".EXT")` are case-insensitive; `is_junk` also matches `.r00`-`.r99`)
- `scan: ScanConfig { interval_minutes (0 = manual), anchor_utc: AnchorUtc{hour,minute}, qbit_refresh_minutes (0 = off) }`
- `limits: Limits { content_read_concurrency, ffprobe_concurrency, http_concurrency_per_service, max_response_bytes: u64 }`
- `matching: MatchingConfig { similarity_threshold: f64, sample_size_bytes, sample_count, compare_chunk_bytes, duration_tolerance_seconds, duration_tolerance_ratio, ffprobe_path: String }`
- `services: Services { qbittorrent: Option<QbitService{url: url::Url, username, password: Secret, external_prefixes}>, sonarr/radarr/jellyfin: Option<ApiService{url, api_key: Secret}> }`. `None` = not configured. A URL without its secret is a startup error, so `Some` always has its secret.
- `mode: Mode { dry_run (default true), auto_mode (default false) }`
- `capabilities: Capabilities` with `is_enabled(Capability)`. Defaults per architecture 13.2 (`arr_grab`, `arr_manual_import`, `arr_queue_remove`, `arr_set_monitored`, `auto_remove_empty_torrents` off).
- `auth: AuthConfig { username, password_hash: Secret (PHC), session_key: Secret, session_ttl_hours, cookie_secure, login_limit_per_ip, login_limit_per_ip_user }`

Use `secret.expose()` only at the point of use (header, hash check); `secret.ct_eq(&str)` for comparison.

### Path mapping (`cfg.paths`)
```rust
paths.to_app(ServiceKind, raw: &str) -> MappedPath        // domain type: Mapped / OutsideRoots / DeclaredExternal / NoRule / Invalid
paths.to_service(ServiceKind, &RootedPath) -> Option<String>   // reverse; None when no rule covers it
paths.roots().locate(abs_app_path) -> Option<RootedPath>   // classify an application path
paths.roots().abs_path(&RootedPath) -> Option<String>      // host/container absolute path of a rooted path
paths.roots().{completed, incomplete, media, download_roots, media_roots, all, get(&RootId)}
```
Rules: lexical normalization (reject relative, `.`, `..`, NUL; collapse `//`), longest service prefix on component
boundaries (`/dl` is not a prefix of `/dl2`), classification against roots, `DeclaredExternal` only for qBittorrent
`external_prefixes`. No rule means `NoRule` (identity is never assumed), also for Sonarr, Radarr and Jellyfin. `Root.path`
is a canonical UTF-8 string; `Root.id` is `RootId::completed()` / `incomplete()` / `media(path)`. Mapping is lexical:
**fs (06) must still open every path through directory handles with no-follow beneath the root.**
Rule validation at startup: both sides absolute and normalized; app path not `/`; app path inside a root or an
ancestor of roots (an ancestor rule still classifies results strictly against roots); no symlink on the part of the app
path below a root; no duplicate service prefix; no two prefixes for one app path; nested app paths need nested service
prefixes with the same suffix (keeps reverse mapping exact; proptest-verified round trip); external prefixes must not
map inside a root.

### Roots
`roots.completed` and `roots.media` (>= 1) are required, `roots.incomplete` optional. Each must be absolute, exist, be a
directory, equal its canonical path (no symlink component), not be `/`. All roots must be pairwise distinct and
non-nested, else startup fails (identical and nested produce distinct messages). `server.data_dir` must exist, be canonical,
lie outside every root and contain none (so reports and the DB can never land in a root). The only filesystem calls
in this crate are `canonicalize`, `metadata`, `symlink_metadata` on configured paths and reading the config file.

### Trusted proxies
`cfg.server.trusted_proxies.client_ip(peer: IpAddr, forwarded_for_header_values: &[&str]) -> IpAddr`. Untrusted peer:
headers ignored. Trusted peer: walk `X-Forwarded-For` right to left, skip trusted hops, return the first untrusted
address; an unparsable entry stops the walk at the last trusted hop; IPv4-mapped IPv6 is canonicalized; bare IPs
accepted; `/0` rejected. `Forwarded:` is not read. web (16/17) must pass every XFF header line in order, and must use
this for rate limiting and login logs.

### Environment variables (public contract; also `.env.example`)
As in architecture section 11 table, plus these exact names: `REMOVARR_CONFIG_FILE`, `REMOVARR_BIND`, `REMOVARR_DATA_DIR`,
`REMOVARR_LOG_LEVEL`, `REMOVARR_TRUSTED_PROXIES`, `REMOVARR_SHUTDOWN_GRACE_SECONDS`, `REMOVARR_COMPLETED_ROOT`,
`REMOVARR_INCOMPLETE_ROOT`, `REMOVARR_MEDIA_ROOTS`, `REMOVARR_{QBIT,SONARR,RADARR,JELLYFIN}_PATH_MAP`,
`REMOVARR_QBIT_EXTERNAL_PREFIXES`, `REMOVARR_EXPECTED_OWNER`, `REMOVARR_INCOMPLETE_STALLED_DAYS`,
`REMOVARR_AUTO_MIN_AGE_HOURS`, `REMOVARR_HIDE_RECENT_DAYS`, `REMOVARR_RECHECK_TIMEOUT_SECONDS`, `REMOVARR_PLAN_TTL_HOURS`,
`REMOVARR_{VIDEO,JUNK,SUBTITLE}_EXTENSIONS`, `REMOVARR_SCAN_INTERVAL_MINUTES`, `REMOVARR_SCAN_ANCHOR_UTC`,
`REMOVARR_QBIT_REFRESH_MINUTES`, `REMOVARR_CONTENT_READ_CONCURRENCY`, `REMOVARR_FFPROBE_CONCURRENCY`,
`REMOVARR_HTTP_CONCURRENCY`, `REMOVARR_MAX_RESPONSE_BYTES`, `REMOVARR_SIMILARITY_THRESHOLD`, `REMOVARR_SAMPLE_SIZE_BYTES`,
`REMOVARR_SAMPLE_COUNT`, `REMOVARR_COMPARE_CHUNK_BYTES`, `REMOVARR_DURATION_TOLERANCE_SECONDS`,
`REMOVARR_DURATION_TOLERANCE_RATIO`, `REMOVARR_FFPROBE_PATH`, `REMOVARR_{QBIT,SONARR,RADARR,JELLYFIN}_URL`,
`REMOVARR_QBIT_USERNAME`, `REMOVARR_DRY_RUN`, `REMOVARR_AUTO_MODE`, `REMOVARR_CAP_<NAME>` (9 flags), `REMOVARR_USERNAME`,
`REMOVARR_SESSION_TTL_HOURS`, `REMOVARR_COOKIE_SECURE`, `REMOVARR_LOGIN_LIMIT_PER_IP`, `REMOVARR_LOGIN_LIMIT_PER_IP_USER`.
Secrets (env only): `REMOVARR_QBIT_PASSWORD`, `REMOVARR_SONARR_API_KEY`, `REMOVARR_RADARR_API_KEY`,
`REMOVARR_JELLYFIN_API_KEY`, `REMOVARR_PASSWORD_HASH` (a doubled `$$` is collapsed to `$`), `REMOVARR_SESSION_KEY` (>= 64 chars).
Precedence: environment over file over default; an empty or blank env value counts as unset. Booleans accept
true/false/1/0/yes/no/on/off. Lists are comma-separated. Mappings in the file are `[[path_mappings.<svc>]] service=..., app=...`.

### Redaction helpers (use them in every connector log/error)
`redact_url` (drops userinfo and fragment, masks sensitive query values), `redact_tracker_url` (scheme + host + port only;
use it for any torrent tracker URL), `redact_header(name, value)` (Authorization, Cookie, Set-Cookie, X-Api-Key, tokens,
anything containing pass/token/secret/key/auth/sig/cookie/session), `redact_text` (key=value, JSON `"key":"v"`, `Bearer`/`Basic`
tokens, URL userinfo), `Config::scrub` (also strips exact secret values). Qbit cookie `SID` and ARR `X-Api-Key` must go through these.

### Startup diagnostics
Every problem is collected (`ConfigError.problems`), listed one per line as
``missing required setting `roots.completed` (env REMOVARR_COMPLETED_ROOT)`` or ``invalid setting `scan.anchor_utc`: must be HH:MM ... (env ...)``.
Reasons never include values (tests assert this for bad numbers, secrets, TOML type errors). TOML errors show only the line
number (and the key for unknown fields). `app` should print `ConfigError` to stderr and exit with `err.exit_code()` (2).

## Deviations from the architecture

1. `Config::fingerprint()` covers roots, mappings, external prefixes, owner, thresholds, files, matching, capabilities
   and mode only. Service URLs, bind, auth and limits are excluded (they do not alter what a plan may do).
2. `ffprobe_path` is validated syntactically (bare name or absolute path), not for executability, so config loads on machines
   without ffprobe. 07 or `app` should check it at first use and report a service-state problem.
3. `server.data_dir` is checked for existence, canonical form and separation from roots, not writability (storage proves
   that when it opens the DB). Non-existence of `/config` is therefore a startup error, as the table requires "exists".
4. The `,`/`=` path restriction applies to every configured path, also in the TOML file (consistency with ADR-0005).
5. `scan.interval_minutes` 1..=59 is rejected with a message (0 or 60..=10080), as the table says.
6. `ProblemKind`/`ConfigError` are new public types (no contract existed). `Config::load_*` returns `ConfigError`.
7. `Config.paths` (a `PathMapper` holding `Roots`) replaces a separate `roots`/`mappings` pair; use `cfg.roots()`.

## Open TODOs / notes

- `test-support` feature is empty except `LiveMode::for_tests()`; consumers enable it only as a dev-dependency feature
  (`removarr-config = { workspace = true, features = ["test-support"] }` in their `[dev-dependencies]`).
- Owner check against real file ownership is not here (fs/issue-engine use `cfg.owner`).
- Argon2 PHC validation is a prefix check (`$argon2`); 17 verifies the hash for real at login.
- No log-redaction layer for `tracing` is installed here; `app` may wrap events with `redact_text`.
- `.env.example` is owned by 20 from wave 13. Keys there must stay in sync with this list.
- Gap: `cargo fmt --all --check` currently fails only in `crates/domain/tests/parity_props.rs` (agent 21's file, not mine), and
  `removarr-storage` did not compile during my run (agent 04 in progress). Neither is in `removarr-config`.

## Commands run and results (host target)

- `cargo fmt -p removarr-config --check`: pass.
- `cargo clippy -p removarr-config --all-targets -- -D warnings`: pass; also with `--features test-support`: pass.
- `cargo test -p removarr-config`: pass. 11 unit tests (path lexing, proxy, redaction), 15 in `tests/load.rs` (defaults,
  precedence, invalid values with non-leak assertion, missing fields, secrets, Debug redaction, fingerprint, examples),
  7 in `tests/roots.rs` (missing, nonexistent, relative, traversal, identical, nested, symlink, data dir), 11 in
  `tests/mapping.rs` (classification, traversal, ambiguous prefix, rule confinement, symlink escape, external, reverse, and
  3 proptests: confinement + round trip, rooted-path round trip, accepted rule sets are reversible).
- Workspace-wide: `cargo fmt --all --check` fails on agent 21's file only; workspace clippy/test blocked by `removarr-storage`
  (in flux, agent 04). Re-run when wave 4 settles.
