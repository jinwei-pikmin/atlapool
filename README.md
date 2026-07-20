# atlapool

A secure, cloud-native Atlassian gateway for AI agents. MCP clients get Jira,
Confluence, and Bitbucket tools **without holding any Atlassian credential** —
atlapool authenticates each agent, enforces per-agent default-deny policy,
injects the real token upstream, and audit-logs every write operation.

> Onboarding: [Quick start](#quick-start) · Configuration: [config.example.toml](config.example.toml) · Architecture decisions: [docs/adr/](docs/adr/)

## Design Principles

- **No Atlassian credential on the agent** — agents hold only an atlapool API
  key (revocable, policy-bounded, not an Atlassian token). The Atlassian and
  Bitbucket credentials live in exactly one place: atlapool.
- **Default-deny policy engine** — each agent gets an exact tool allowlist plus
  `projects`, `spaces`, `bitbucket_workspaces`, and `bitbucket_repos` allowlists;
  new tools and dimensions are denied until explicitly granted.
- **Fail-closed audit for writes** — every write tool writes an `attempt` record
  before the upstream call. If the audit write fails, the request is aborted.
- **Cloud-native** — runs as a single static binary or container, listens on the
  configured `port`, and resolves credentials from `env`, AWS Secrets Manager, or
  GCP Secret Manager.
- **Bearer token injection** — atlapool forwards upstream requests with
  `Authorization: Bearer <token>` after stripping caller-supplied sensitive
  headers.
- **Current limitation** — Jira and Confluence still forward the configured
  long-lived token. Bitbucket can now use OAuth 2.0 client credentials to
  obtain 2-hour access tokens with automatic refresh, but these tokens are
  workspace/consumer scoped; atlapool does **not** yet mint per-repo
  short-lived tokens like ghpool's GitHub App installation tokens. Repo-level
  isolation for Bitbucket is still enforced by the `bitbucket_repos` allowlist.

## Architecture

```text
                         Private Network / VPC
   ┌─────────┐      ┌───────────────────────────────────────┐
   │ Agent A │──────│              atlapool                 │
   └─────────┘      │  ┌─────────────────────────────────┐  │
                    │  │ Agent auth (X-Atlapool-Key)     │  │
                    │  │ + default-deny tool / project / │  │
                    │  │   space / workspace / repo      │  │
                    │  │   policy                        │  │
                    │  └─────────────────────────────────┘  │
                    │  ┌───────────────┐ ┌───────────────┐  │
                    │  │ Write gate    │ │ Audit log     │  │
                    │  │ (fail-closed) │ │ (JSONL)       │  │
                    │  └───────────────┘ └───────────────┘  │
                    │  ┌─────────────────────────────────┐  │
                    │  │ Secret resolver                 │  │
                    │  │ env / AWS / GCP                 │  │
                    │  └─────────────────────────────────┘  │
                    │  ┌─────────────────────────────────┐  │
                    │  │ Upstream client (Jira /       │  │
                    │  │ Confluence / Bitbucket)         │  │
                    │  └─────────────────────────────────┘  │
                    └───────────┬─────────────┬───────────┘
                                │             │
                                ▼             ▼
              ┌────────────────────────┐  ┌────────────────────┐
              │ api.atlassian.com      │  │ api.bitbucket.org  │
              │ (Jira + Confluence)    │  │ (Bitbucket)        │
              └────────────────────────┘  └────────────────────┘
```

Request flow for `POST /mcp`:

1. Authenticate `X-Atlapool-Key` and load the matching agent policy.
2. Resolve `tool`, `project`, `space`, `workspace`, and `repo` allowlists.
3. For write tools: verify `enable_writes = true` and write a fail-closed
   `attempt` audit record.
4. Build the upstream request from an empty header set, inject the current
   bearer token (static for Jira/Confluence; fetched or cached for Bitbucket),
   and forward it.
5. Return the upstream response to the caller.

## Status

| Milestone | Status | Highlights |
|---|---|---|
| **v0.1** | ✅ Complete | 12 MCP tools for Jira, Confluence, and Bitbucket (read + write); write-gate; fail-closed pre-flight `attempt` audit logging + post-flight `result` records; per-agent allowlists; `env` / AWS / GCP secret resolution; Bitbucket OAuth client credentials with cached, auto-refreshed short-lived tokens; `/health` and `/stats` endpoints. |
| **v0.2** | 📋 Planned | Additional read tools such as `jira_search_issues`; per-repo scoped short-lived token minting for Bitbucket. |

## How clients use it

atlapool exposes a single JSON-RPC 2.0 endpoint at `POST /mcp`. Any MCP client
that can set a custom HTTP header can connect.

### Claude Desktop (example)

```json
{
  "mcpServers": {
    "atlapool": {
      "command": "npx",
      "args": ["-y", "mcp-remote"],
      "env": {
        "MCP_REMOTE_URL": "http://localhost:8080/mcp",
        "MCP_REMOTE_HEADER": "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO"
      }
    }
  }
}
```

Replace `http://localhost:8080/mcp` with the deployed URL and set the header
value to the agent key configured in `config.toml`.

### Raw HTTP

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "jira_get_issue",
      "arguments": { "issue_key": "PROJ-123" }
    }
  }'
```

## Quick start (Docker)

1. Clone the repo and copy the example config.

```sh
git clone https://github.com/jinwei-pikmin/atlapool.git
cd atlapool
cp config.example.toml config.toml
```

2. Edit `config.toml`. The smallest working setup is one agent with
   environment-backed credentials.

```toml
port = 8080

[atlassian]
cloud_id = "env:ATLASSIAN_CLOUD_ID"
token = "env:ATLASSIAN_TOKEN"

[mcp]
enabled = true

[[agents]]
id = "demo"
keys = ["env:ATLAPOOL_KEY_DEMO"]
projects = ["PROJ"]
tools = ["jira_get_issue"]
enable_writes = false
```

Then export the secrets:

```sh
export ATLASSIAN_CLOUD_ID="your-cloud-id"
export ATLASSIAN_TOKEN="your-atlassian-scoped-token"
export ATLAPOOL_KEY_DEMO="demo-secret-key"
```

3. Build and run the container.

```sh
docker build -t atlapool .
docker run -d --name atlapool \
  -p 8080:8080 \
  -e PORT=8080 \
  -e ATLASSIAN_CLOUD_ID \
  -e ATLASSIAN_TOKEN \
  -e ATLAPOOL_KEY_DEMO \
  atlapool
```

4. Check that the server is alive.

```sh
curl -s http://localhost:8080/health
```

Expected response:

```json
{"status":"ok"}
```

5. Call the MCP tool. Replace `PROJ-123` with an issue key from the project
   listed in `config.toml`.

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "jira_get_issue",
      "arguments": { "issue_key": "PROJ-123" }
    }
  }'
```

Expected successful response (the `result` object is the upstream Jira JSON):

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": { "id": "10000", "key": "PROJ-123", ... }
}
```

To stop the container:

```sh
docker stop atlapool && docker rm atlapool
```

## Quick start without Docker

If you already have a Rust toolchain:

```sh
cp config.example.toml config.toml
# edit config.toml as above
export ATLASSIAN_CLOUD_ID="your-cloud-id"
export ATLASSIAN_TOKEN="your-atlassian-scoped-token"
export ATLAPOOL_KEY_DEMO="demo-secret-key"
cargo run
```

Then use the same `curl` commands as above.

## MCP tools

Call `POST /mcp` with a JSON-RPC 2.0 envelope:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "<tool-name>",
    "arguments": { ... }
  }
}
```

| Tool | What it does | Required arguments | Allowlist dimension | Write-gate | Audit |
|---|---|---|---|---|---|
| `jira_get_issue` | Fetch a Jira issue by key | `issue_key` (e.g. `PROJ-123`) | `projects` (parsed from key) | No | No |
| `jira_create_issue` | Create a Jira issue | `project`, `summary`, plus any Jira `fields` | `projects` (from `project`) | Yes (`enable_writes = true`) | Yes |
| `jira_add_comment` | Add a comment to a Jira issue | `issue_key`, `body` (ADF, forwarded as-is) | `projects` (parsed from key) | Yes | Yes |
| `confluence_get_page` | Fetch a Confluence page by ID | `page_id` (numeric page ID), `space` (key for allowlist) | `spaces` | No | No |
| `confluence_create_page` | Create a Confluence page | `space` (key for allowlist), `space_id` (numeric ID), `title`, `body` (storage HTML) | `spaces` | Yes | Yes |
| `confluence_update_page` | Update a Confluence page | `space` (key for allowlist), `space_id` (numeric ID), `page_id` (numeric ID), `title`, `version`, `body` (storage HTML) | `spaces` | Yes | Yes |
| `bitbucket_get_repo` | Fetch a Bitbucket repository | `repo_slug` (from config `workspace`) | `bitbucket_workspaces`, `bitbucket_repos` | No | No |
| `bitbucket_get_pull_request` | Fetch a Bitbucket pull request | `repo_slug`, `pull_request_id` (from config `workspace`) | `bitbucket_workspaces`, `bitbucket_repos` | No | No |
| `bitbucket_create_repo` | Create a Bitbucket repository | `repo_slug` (from config `workspace`), optional `is_private` (default `true`) | `bitbucket_workspaces`, `bitbucket_repos` | Yes | Yes |
| `bitbucket_create_branch` | Create a branch in a repository | `repo_slug`, `branch_name`, `target_hash` | `bitbucket_workspaces`, `bitbucket_repos` | Yes | Yes |
| `bitbucket_create_commit` | Create a commit by uploading files | `repo_slug`, `message`, `branch`, `files` (map of path → content) | `bitbucket_workspaces`, `bitbucket_repos` | Yes | Yes |
| `bitbucket_create_pull_request` | Create a pull request | `repo_slug`, `title`, `source_branch`, optional `destination_branch`, `description` | `bitbucket_workspaces`, `bitbucket_repos` | Yes | Yes |

The allowlist is deny-by-default: an agent must list the exact tool name and
must also match the `project`, `space`, `bitbucket_workspaces`, or `bitbucket_repos`
dimension. Read tools work when `enable_writes` is `false`. Write tools need
`enable_writes = true` and a writable `audit.path`.

### Examples

**Read a Jira issue**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "jira_get_issue",
      "arguments": { "issue_key": "PROJ-123" }
    }
  }'
```

**Create a Jira issue**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "jira_create_issue",
      "arguments": {
        "project": "PROJ",
        "summary": "Issue summary",
        "issuetype": { "name": "Story" }
      }
    }
  }'
```

> For real Jira, `description` and `comment` bodies must be in Atlassian
> Document Format (ADF). atlapool currently forwards the body as-is, so the
> caller is responsible for building valid ADF.

**Add a comment to a Jira issue**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "jira_add_comment",
      "arguments": {
        "issue_key": "PROJ-123",
        "body": { "type": "doc", "version": 1, "content": [] }
      }
    }
  }'
```

**Read a Confluence page**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "confluence_get_page",
      "arguments": { "space": "SPACE", "page_id": "12345" }
    }
  }'
```

**Create a Confluence page**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "confluence_create_page",
      "arguments": {
        "space": "SPACE",
        "space_id": "12345",
        "title": "New page",
        "body": "<p>Hello</p>"
      }
    }
  }'
```

**Update a Confluence page**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "confluence_update_page",
      "arguments": {
        "space": "SPACE",
        "space_id": "12345",
        "page_id": "67890",
        "title": "Updated page",
        "version": 2,
        "body": "<p>Updated</p>"
      }
    }
  }'
```

For `confluence_create_page` and `confluence_update_page`, the `body` argument
can also be a full JSON object with `representation` and `value` if the caller
wants full control over the body format.

**Read a Bitbucket repository**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "bitbucket_get_repo",
      "arguments": { "repo_slug": "my-repo" }
    }
  }'
```

**Read a Bitbucket pull request**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "bitbucket_get_pull_request",
      "arguments": { "repo_slug": "my-repo", "pull_request_id": "42" }
    }
  }'
```

**Create a Bitbucket repository**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "bitbucket_create_repo",
      "arguments": { "repo_slug": "new-repo" }
    }
  }'
```

Omit `is_private` to default the new repository to private.

**Create a Bitbucket branch**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "bitbucket_create_branch",
      "arguments": {
        "repo_slug": "my-repo",
        "branch_name": "feature/x",
        "target_hash": "abc123"
      }
    }
  }'
```

**Create a Bitbucket commit**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "bitbucket_create_commit",
      "arguments": {
        "repo_slug": "my-repo",
        "message": "Add README",
        "branch": "main",
        "files": { "README.md": "Hello world" }
      }
    }
  }'
```

**Create a Bitbucket pull request**

```sh
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -H "X-Atlapool-Key: $ATLAPOOL_KEY_DEMO" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "bitbucket_create_pull_request",
      "arguments": {
        "repo_slug": "my-repo",
        "title": "Feature X",
        "source_branch": "feature/x",
        "destination_branch": "main",
        "description": "Adds feature X"
      }
    }
  }'
```

The workspace for Bitbucket calls comes from `[bitbucket].workspace` in
`config.toml`, not from the caller.

## Configuration

See [`config.example.toml`](config.example.toml) for a fully annotated file.

### Minimal working config

This is the smallest setup used in real testing. It enables one agent to call
Jira, Confluence, and Bitbucket tools:

```toml
port = 8080

[atlassian]
cloud_id = "env:ATLASSIAN_CLOUD_ID"
token = "env:ATLASSIAN_TOKEN"

[bitbucket]
workspace = "my-workspace"
token = "env:BITBUCKET_TOKEN"
# Or use short-lived OAuth tokens:
# [bitbucket.oauth]
# client_id = "env:BITBUCKET_CLIENT_ID"
# client_secret = "env:BITBUCKET_CLIENT_SECRET"

[[agents]]
id = "demo"
keys = ["env:ATLAPOOL_KEY_DEMO"]
tools = [
  "jira_get_issue",
  "jira_create_issue",
  "jira_add_comment",
  "confluence_get_page",
  "confluence_create_page",
  "confluence_update_page",
  "bitbucket_get_repo",
  "bitbucket_get_pull_request",
  "bitbucket_create_repo",
  "bitbucket_create_branch",
  "bitbucket_create_commit",
  "bitbucket_create_pull_request",
]
projects = ["PROJ"]
spaces = ["SPACE"]
bitbucket_workspaces = ["my-workspace"]
bitbucket_repos = ["my-repo"]
# Multiple repos or a glob are also valid, e.g.
# bitbucket_repos = ["repo-a", "repo-b", "repo-c"]
# bitbucket_repos = ["*"]  # allow all repos in the workspace
enable_writes = true
```

### Allowlists

Each agent policy contains four target allowlists. A request is denied unless the
called tool is in `tools` **and** every target dimension that the tool provides is
allowed by the corresponding list.

| Allowlist | Tool dimensions it gates | Examples |
|---|---|---|
| `projects` | Jira `project` / `issue_key` prefix | `["PROJ"]`, `["PROJ-A", "PROJ-B"]`, `["PROJ/*"]`, `["*"]` |
| `spaces` | Confluence `space` key | `["SPACE"]`, `["SPACE", "DOCS"]`, `["SPACE/*"]`, `["*"]` |
| `bitbucket_workspaces` | Bitbucket `workspace` slug | `["my-workspace"]`, `["work-a", "work-b"]`, `["*"]` |
| `bitbucket_repos` | Bitbucket `repo_slug` | `["my-repo"]`, `["repo-a", "repo-b"]`, `["my-repo/*"]`, `["*"]` |

#### Array and glob semantics

- Each allowlist is an **array** and may contain multiple values.
- Values are matched with a simple glob: `*` matches any character sequence,
  including empty sequences and `/`. No other metacharacters are supported.
  - `PROJ/*` matches `PROJ/123` and `PROJ/123/sub`.
  - `PROJ*` matches `PROJ` and `PROJ-123` but **not** `OTHER`.
  - `*` alone matches any value.

#### Deny-by-default

An **empty allowlist (`[]`) denies everything** for that dimension. It does *not*
mean "allow all". If you want to allow all values in a dimension, use `["*"]`.

#### AND-across-provided, OR-within

For a single call, **each provided dimension must pass independently**:

- If a tool call resolves a `workspace` and a `repo` (all Bitbucket tools do),
  *both* `bitbucket_workspaces` and `bitbucket_repos` must contain a matching
  pattern. `WORKSPACE-A/my-repo` is allowed only when the workspace allowlist
  matches `WORKSPACE-A` **and** the repo allowlist matches `my-repo`.
- If a tool call resolves only a `project` (e.g. `jira_get_issue`), only
  `projects` is checked; the other allowlists are irrelevant.
- Within one dimension, the value only has to match **one** pattern in the list
  (OR semantics).

Example: with

```toml
bitbucket_workspaces = ["my-workspace"]
bitbucket_repos = ["my-repo", "other-repo"]
```

- `my-workspace/my-repo` is allowed.
- `my-workspace/other-repo` is allowed.
- `my-workspace/third-repo` is denied (repo does not match).
- `other-workspace/my-repo` is denied (workspace does not match).

Notes:

- `base_url` is **not** required for Atlassian or Bitbucket; the defaults are the
  public cloud endpoints.
- `atlassian.cloud_id` takes precedence over `atlassian.base_url` when both are
  set.
- `bitbucket.workspace` is required for all Bitbucket tools and is injected by
  the server; the caller cannot override it.

### Secret reference formats

`atlassian.token`, `bitbucket.token`, `bitbucket.oauth.client_id`,
`bitbucket.oauth.client_secret`, and each `keys` entry can be any of:

- `env:VAR_NAME` — local environment variable.
- `aws:secretsmanager:<secret-id>` — AWS Secrets Manager plain-string secret.
- `gcp:secretmanager:<project>/<secret>` — GCP Secret Manager latest version.
- `gcp:secretmanager:projects/<project>/secrets/<secret>/versions/<version>` —
  GCP Secret Manager specific version.

### Field reference

| Section | Field | Required | Default | Notes |
|---|---|---|---|---|
| top-level | `port` | No | `8080` | HTTP listen port. |
| `[atlassian]` | `cloud_id` | Atlassian tools: **Yes** | — | Cloud ID from `https://<domain>.atlassian.net/_edge/tenant_info`. |
| `[atlassian]` | `base_url` | No | — | Fallback when `cloud_id` is not set; computed as `https://api.atlassian.com/ex/jira/{cloud_id}` when `cloud_id` is present. Only needed for private installs. |
| `[atlassian]` | `token` | Atlassian tools: **Yes** | — | Atlassian API token, in any secret-ref format above. |
| `[bitbucket]` | `workspace` | Bitbucket tools: **Yes** | — | Workspace slug; server injects it into every Bitbucket path. |
| `[bitbucket]` | `base_url` | No | `https://api.bitbucket.org/2.0` | Only override for private Bitbucket Server. |
| `[bitbucket]` | `token` | Bitbucket tools: **One of token/oauth** | — | App password, Workspace/Project access token (Premium), or long-lived OAuth access token. |
| `[bitbucket.oauth]` | `client_id` | OAuth: **Yes** | — | OAuth consumer key. Secret reference. |
| `[bitbucket.oauth]` | `client_secret` | OAuth: **Yes** | — | OAuth consumer secret. Secret reference. |
| `[bitbucket.oauth]` | `token_url` | No | `https://bitbucket.org/site/oauth2/access_token` | Token endpoint override. |
| `[mcp]` | `enabled` | No | `false` | Set `true` to enable the `/mcp` endpoint. |
| `[mcp]` | `enable_writes` | No | `false` | Default write-gate value per agent; can be overridden by `agents.enable_writes`. |
| `[audit]` | `path` | Writes: **Yes** | `atlapool-audit.jsonl` | Must be writable. Write tools fail when audit cannot be written. |
| `[[agents]]` | `id` | **Yes** | — | Human-readable identifier. |
| `[[agents]]` | `keys` | **Yes** | `[]` | One or more secrets accepted as `X-Atlapool-Key`. |
| `[[agents]]` | `tools` | **Yes** | `[]` | Exact MCP tool names this agent may call. |
| `[[agents]]` | `projects` | No | `[]` | Jira project keys allowed. Supports glob `*`. |
| `[[agents]]` | `spaces` | No | `[]` | Confluence space keys allowed. Supports glob `*`. |
| `[[agents]]` | `bitbucket_workspaces` | No | `[]` | Bitbucket workspace slugs allowed. Supports glob `*`. |
| `[[agents]]` | `bitbucket_repos` | No | `[]` | Bitbucket repository slugs allowed. Supports glob `*`. |
| `[[agents]]` | `enable_writes` | No | `false` | Must be `true` for any write tool. |

### Service account permissions

atlapool sends the configured token as `Authorization: Bearer <token>`. For
Bitbucket, it can instead fetch short-lived OAuth access tokens from
`[bitbucket.oauth]` and cache them until expiry. You do **not** need `email` or
Basic Auth; atlapool has never used the `email` field.

#### Atlassian Cloud

You need two values:

1. **Scoped token** — a Service Account token with the required Atlassian OAuth
   scopes:
   - Jira:
     - `read:jira-work`
     - `write:jira-work` (only if you enable write tools)
   - Confluence:
     - `read:page:confluence`
     - `write:page:confluence` (only if you enable write tools)
     - `read:attachment:confluence`
     - `write:attachment:confluence` (only if you enable write tools)
     - `read:comment:confluence`
     - `write:comment:confluence` (only if you enable write tools)
2. **Cloud ID** — the unique identifier of your Atlassian Cloud site. To find it,
   sign in to your site and open:

   ```
   https://<your-domain>.atlassian.net/_edge/tenant_info
   ```

   The response is a JSON object; copy the value of the `cloudId` field. For
   example, if your site domain is `example`, the URL is:

   ```
   https://example.atlassian.net/_edge/tenant_info
   ```

   and `cloudId` looks like `12345678-1234-1234-1234-123456789abc`.

REST calls are sent through the `api.atlassian.com` gateway using `cloud_id`:

- Jira: `https://api.atlassian.com/ex/jira/{cloud_id}/rest/api/3/...`
- Confluence: `https://api.atlassian.com/ex/confluence/{cloud_id}/wiki/api/v2/...`

#### Bitbucket Cloud

Choose one of the following authentication methods:

1. **App password** (deprecated by Atlassian; use for legacy integrations only):
   configure `[bitbucket].token`. Tied to an individual user account.

2. **Workspace or Project access token** (Bitbucket Premium): configure
   `[bitbucket].token`. Tied to the workspace/project, not a person, and supports
   the same `repository:*` / `pullrequest:*` scopes as an app password.

3. **OAuth 2.0 client credentials** (free plan friendly; new in PR #57): configure
   `[bitbucket.oauth]`. atlapool will fetch a 2-hour access token and refresh it
   before expiry. The consumer must be granted at least:

   - `repository:read` — for `bitbucket_get_repo`
   - `pullrequest:read` — for `bitbucket_get_pull_request`
   - `repository:write` — for `bitbucket_create_branch`, `bitbucket_create_commit`
   - `repository:admin` — for `bitbucket_create_repo` (Bitbucket's API requires admin scope to create repositories; this is different from `repository:write`)
   - `pullrequest:write` — for `bitbucket_create_pull_request`

   > **Why OAuth client credentials instead of Workspace/Project access tokens?**
   > Workspace and Project access tokens are a Bitbucket Premium-only feature.
   > An OAuth consumer works on free plans and is the only no-cost way to obtain
   > credentials that are not tied to an individual user account.

> **Note:** earlier test deployments that used a Bitbucket Workspace access token
> should switch to `[bitbucket.oauth]` per the setup above; Workspace access tokens
> require a Premium plan.

Bitbucket calls are sent to `https://api.bitbucket.org/2.0` (or `bitbucket.base_url`
if overridden):

- Repository: `https://api.bitbucket.org/2.0/repositories/{workspace}/{repo_slug}`
- Pull request: `https://api.bitbucket.org/2.0/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}`
- Branches: `https://api.bitbucket.org/2.0/repositories/{workspace}/{repo_slug}/refs/branches`
- Source (commit): `https://api.bitbucket.org/2.0/repositories/{workspace}/{repo_slug}/src`
- Pull requests: `https://api.bitbucket.org/2.0/repositories/{workspace}/{repo_slug}/pullrequests`

### Bitbucket scopes

Create an **App password** or **OAuth consumer** with the scopes below. For OAuth,
grant these scopes on the consumer; atlapool cannot request narrower scopes at
runtime.

```
repository:admin
repository:read
repository:write
pullrequest:read
pullrequest:write
```

Scope-to-tool mapping:

- `repository:admin` — `bitbucket_create_repo` (Bitbucket requires admin scope for repository creation)
- `repository:read` — `bitbucket_get_repo`
- `repository:write` — `bitbucket_create_branch`, `bitbucket_create_commit`
- `pullrequest:read` — `bitbucket_get_pull_request`
- `pullrequest:write` — `bitbucket_create_pull_request`

### Agent allowlists

- `tools`: exact MCP tool names the agent may call.
- `projects`: Jira project keys allowed. Supports glob `*` (matches any sequence,
  including `/`).
- `spaces`: Confluence space keys allowed. Same glob semantics.
- `bitbucket_workspaces`: Bitbucket workspace slugs allowed. Same glob semantics.
- `bitbucket_repos`: Bitbucket repository slugs allowed. Same glob semantics.
- `enable_writes`: must be `true` for any write tool.

### Read vs. write and audit

Read tools (`jira_get_issue`, `confluence_get_page`, `bitbucket_get_repo`,
`bitbucket_get_pull_request`) pass through the allowlist and do not touch the
audit log.

Write tools (`jira_create_issue`, `jira_add_comment`, `confluence_create_page`,
`confluence_update_page`, `bitbucket_create_repo`, `bitbucket_create_branch`,
`bitbucket_create_commit`, `bitbucket_create_pull_request`) require
`enable_writes = true` and a configured `audit.path`.

Audit guarantee:

- **Pre-flight `attempt` record:** written before the upstream call. If this
  write fails, the request is rejected immediately (fail-closed).
- **Post-flight `result` record:** written after the upstream call returns.
  `result` is `"success"` when the upstream status is 2xx (including 204 and
  empty bodies) and `"failure"` for 4xx/5xx or connection errors. If the result
  record write fails, the response to the caller is still returned; the failure
  is logged by the server but does not abort the already-completed upstream
  call.

Each audit line is a JSON object with the fields from `src/audit.rs`:

| Field | Type | Present in | Meaning |
|---|---|---|---|
| `agent_id` | string | always | Agent identifier from `config.toml`. |
| `tool` | string | always | MCP tool name that was called. |
| `target` | string | always | Allowlist dimension used for the call (`project`, `space`, `workspace`, or `repo` slug). |
| `timestamp` | string | always | RFC 3339 UTC timestamp. |
| `result` | string | always | `"attempt"`, `"success"`, or `"failure"`. |
| `status` | number | `success` / `failure` only | Upstream HTTP status code; `0` when the request never reached upstream. |
| `message` | string | `failure` only | Human-readable error or upstream response summary. |

### Credential model

atlapool forwards the **same long-lived Service Account token** to every
upstream request (`Authorization: Bearer <token>`). It does **not** mint
per-repo or per-session short-lived tokens. This means the configured token
must have enough scope for all operations the agent may perform, and a
compromised token can be used for any allowed resource. Per-request short-lived
credential minting is a known architectural limitation planned for a future
release.

## Testing with a mock upstream

If you do not have a live Atlassian account, point `atlassian.base_url` or
`bitbucket.base_url` at a mock server and add a fake token. The proxy does not
validate the token itself; the upstream server will. For example, start a tiny
Python mock for Jira:

```python
from http.server import HTTPServer, BaseHTTPRequestHandler
import json

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith('/rest/api/3/issue/'):
            key = self.path.split('/')[-1]
            self.send_response(200)
            self.end_headers()
            self.wfile.write(json.dumps({"id": "12345", "key": key}).encode())
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, *args): pass

HTTPServer(("127.0.0.1", 9001), Handler).serve_forever()
```

Run it in one terminal, set `base_url = "http://127.0.0.1:9001"` in
`config.toml`, then `cargo run` in another.

## Troubleshooting

| Symptom | Likely cause | What to check |
|---|---|---|
| `missing or empty X-Atlapool-Key` | Request lacks the `X-Atlapool-Key` header. | Add `-H "X-Atlapool-Key: your-key"`. |
| `unknown key` | The key is not in any `agents.keys` list. | Check `config.toml` keys and secret resolution. |
| `not permitted by agent policy` | Tool, project, space, or Bitbucket workspace/repo is not allowed. | Verify `tools`, `projects`, `spaces`, `bitbucket_workspaces`, and `bitbucket_repos` arrays for the agent. |
| `write tools not enabled for agent` | Calling a write tool without `enable_writes = true`. | Set `enable_writes = true` for that agent. |
| `audit log not configured` / `audit log write failed` | The audit log path is missing or unwritable. | Set `audit.path` or ensure the directory exists. |
| `upstream not configured` / `confluence upstream not configured` / `bitbucket upstream not configured` | The corresponding upstream section is missing or required fields are empty. | Fill in `token` and `cloud_id` / `base_url` (or `workspace` for Bitbucket). |
| `unsupported tool` | The tool name is not implemented or not in the agent `tools` list. | Use a supported tool name. |
| Jira returns 401 or 403 | The Atlassian token is invalid or lacks permissions. | Regenerate `ATLASSIAN_TOKEN` and check project access. |

## License

MIT
