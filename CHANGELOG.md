# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- Jira tools:
  - Read: `jira_search_issues` with forced `project = "..."` JQL prefix, `project`/`projectKey` keyword blacklist, and `max_results` clamping
- Bitbucket tools:
  - Browse: `bitbucket_list_branches`, `bitbucket_list_directory`, `bitbucket_get_file_content`, `bitbucket_list_pull_requests` (with `has_more` pagination hint), `bitbucket_list_pull_request_changes` (diffstat only)
  - Read: `bitbucket_get_pipeline_status` with Bitbucket Pipelines state/result normalization
  - Read: `bitbucket_get_pull_request_diff` with secret redaction, binary detection, and `max_lines` truncation
  - Write: `bitbucket_merge_pull_request` (merge_commit strategy, optional `close_source_branch`)
  - Write: `bitbucket_decline_pull_request` and `bitbucket_delete_branch`
  - Write: `bitbucket_add_pull_request_comment`
- MCP protocol completeness:
  - `initialize` handshake (no key required)
  - `tools/list` returning per-agent allowed tools with JSON Schema `inputSchema`
  - `notifications/initialized` support on `POST /mcp/notify`
  - `tools/call` always returns standard MCP `CallToolResult`, including `isError: true` for non-2xx upstream responses and policy denials
  - Full `initialize` → `tools/list` → `tools/call` flow verified with the official `rmcp` client library over a real TCP listener

### Changed

- **Critical:** `tools/call` no longer switches response format based on the `Mcp-Protocol-Version` header. Success paths now unconditionally return `CallToolResult{isError: false}`, eliminating format inconsistency between success and error responses in the same session.

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
