# ADR-0005: Configuration as an optional TOML file plus a REMOVARR_* environment overlay

Status: accepted (wave 1). Details: `docs/architecture.md` section 11.

## Context
MASTER §4 asks for a configuration file with secrets in environment variables. MASTER §8 says
the user edits only `docker-compose.yml` and `.env`. MASTER contradicts itself here.

## Decision
- The TOML file at `REMOVARR_CONFIG_FILE` (default `/config/removarr.toml`) is optional.
  `deny_unknown_fields`, and it has no secret fields.
- Every non-secret key has a flat `REMOVARR_*` environment variable that overrides the file.
  Lists are comma-separated. Mappings are `service_prefix=app_path`.
- Secrets (`REMOVARR_QBIT_PASSWORD`, `*_API_KEY`, `REMOVARR_PASSWORD_HASH`,
  `REMOVARR_SESSION_KEY`) are environment-only and wrapped in a redacting `Secret` type.
- No root or mapping has a default. Missing required keys stop startup (exit 2), and the
  diagnostics name the keys, never the values.

## Consequences
- Compose-only deployments work. Advanced users may use the file.
- Paths containing `,` or `=` cannot be configured (rejected at startup).
- The environment variable names are a public contract, so renaming them is a breaking change.
