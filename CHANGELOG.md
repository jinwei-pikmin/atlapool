# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- Bitbucket browse tools:
  - `bitbucket_list_branches`
  - `bitbucket_list_directory`
  - `bitbucket_get_file_content`
- MCP protocol completeness:
  - `initialize` handshake (no key required)
  - `tools/list` returning per-agent allowed tools with JSON Schema `inputSchema`
  - `notifications/initialized` support on `POST /mcp/notify`
  - `tools/call` returns standard MCP `CallToolResult` when `Mcp-Protocol-Version` header is present, including `isError: true` for non-2xx upstream responses
  - Full `initialize` → `tools/list` → `tools/call` flow verified with the official `rmcp` client library over a real TCP listener

## [0.1.0] - 2026-07-19

### Added

- `/health` and `/stats` endpoints for liveness and runtime introspection.
- MCP `/mcp` JSON-RPC endpoint with per-agent authentication via `X-Atlapool-Key`.
- Secret resolution from three backends:
  - Environment variables (`env:VAR_NAME`)
  - AWS Secrets Manager (`aws:secretsmanager:<secret-id>`)
  - GCP Secret Manager (`gcp:secretmanager:...`)
- Per-agent allowlists for `tools`, `projects`, `spaces`, `bitbucket_workspaces`, and `bitbucket_repos` with glob `*` support.
- Write-gate: write tools require `enable_writes = true`.
- Fail-closed audit logging: every write attempt is logged before the upstream call; if the audit write fails, the request is rejected.
- Jira tools:
  - `jira_get_issue`
  - `jira_create_issue`
  - `jira_add_comment`
- Confluence tools:
  - `confluence_get_page`
  - `confluence_create_page`
  - `confluence_update_page`
- Bitbucket tools:
  - `bitbucket_get_repo`
  - `bitbucket_get_pull_request`
  - `bitbucket_create_repo` (defaults to `is_private = true`; requires `repository:admin`)
  - `bitbucket_create_branch`
  - `bitbucket_create_commit` (uses Bitbucket `POST /src` with `application/x-www-form-urlencoded`)
  - `bitbucket_create_pull_request`
- Token is always sent as `Authorization: Bearer <token>`; caller-provided sensitive headers are stripped before forwarding.
- `config.example.toml` and README with setup, tool, and permission documentation.
