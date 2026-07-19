# atlapool

Atlassian credential proxy for MCP agents.

atlapool lets an MCP client use Jira and Confluence tools without holding the
real Atlassian API token. The server injects the token, strips caller headers,
enforces per-agent allowlists, gates writes, and logs every write attempt.

## Status

v1 core features are complete: `/health`, `/stats`, `/mcp` with
`jira_get_issue`, `jira_create_issue`, and `confluence_get_page`, three secret
backends (env, AWS Secrets Manager, GCP Secret Manager), per-agent allowlists,
write-gate, and fail-closed audit logging.

## What you need

- Docker (recommended) or a Rust toolchain.
- A running Atlassian Cloud site and an API token, **or** a local mock
  Atlassian server for testing.
- 5–10 minutes to configure and 30 minutes or less from clone to the first
  successful `/mcp` call.

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

The allowlist is deny-by-default: an agent must list the exact tool name and
must also match the `project`, `space`, `bitbucket_workspaces`, or `bitbucket_repos`
dimension. Read tools (`jira_get_issue`, `confluence_get_page`, `bitbucket_get_repo`,
`bitbucket_get_pull_request`) work when `enable_writes` is `false`. Write tools
(`jira_create_issue`, `jira_add_comment`, `confluence_create_page`, `confluence_update_page`) need `enable_writes = true` and a
writable `audit.path`.

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

The workspace for Bitbucket calls comes from `[bitbucket].workspace` in
`config.toml`, not from the caller.

## Configuration

See [`config.example.toml`](config.example.toml) for a fully annotated file.

### Secret reference formats

`atlassian.token`, `bitbucket.token` and each `keys` entry can be any of:

- `env:VAR_NAME` — local environment variable.
- `aws:secretsmanager:<secret-id>` — AWS Secrets Manager plain-string secret.
- `gcp:secretmanager:<project>/<secret>` — GCP Secret Manager latest version.
- `gcp:secretmanager:projects/<project>/secrets/<secret>/versions/<version>` —
  GCP Secret Manager specific version.

Examples:

```toml
[atlassian]
# cloud_id = "12345678-1234-1234-1234-123456789abc"
# base_url = "https://your-domain.atlassian.net"
# email = "agent@example.com"                        # optional
# token = "env:ATLASSIAN_TOKEN"
# token = "aws:secretsmanager:prod/atlassian/token"
# token = "gcp:secretmanager:my-project/atlassian-token"

[bitbucket]
# workspace = "my-workspace"
# base_url = "https://api.bitbucket.org/2.0"
# token = "env:BITBUCKET_TOKEN"
```

### Atlassian credentials

For Service Account scoped tokens you need two values:

1. **Scoped token** — a Service Account token with the required Atlassian OAuth
   scopes. atlapool sends it as `Authorization: Bearer <token>`.
   Required scopes:
   - Jira (OAuth-style): `read:jira-work` and/or `write:jira-work`
   - Confluence (granular): `read:page:confluence`, `write:page:confluence`,
     `read:comment:confluence`, `write:comment:confluence`,
     `read:attachment:confluence`, `write:attachment:confluence`
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

`email` is optional and only useful if you later switch to a classic token with
Basic Auth. `base_url` is optional and kept for human-readable links or for
looking up the cloud ID. REST calls are sent through the `api.atlassian.com`
gateway using `cloud_id`:

- Jira: `https://api.atlassian.com/ex/jira/{cloud_id}/rest/api/3/...`
- Confluence: `https://api.atlassian.com/ex/confluence/{cloud_id}/wiki/api/v2/...`

Bitbucket calls are sent to `https://api.bitbucket.org/2.0` (or `bitbucket.base_url`
if overridden):

- Repository: `https://api.bitbucket.org/2.0/repositories/{workspace}/{repo_slug}`
- Pull request: `https://api.bitbucket.org/2.0/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}`

### Agent allowlists

- `tools`: exact MCP tool names the agent may call.
- `projects`: Jira project keys allowed. Supports glob `*` (matches any sequence,
  including `/`).
- `spaces`: Confluence space keys allowed. Same glob semantics.
- `bitbucket_workspaces`: Bitbucket workspace slugs allowed. Same glob semantics.
- `bitbucket_repos`: Bitbucket repository slugs allowed. Same glob semantics.
- `enable_writes`: must be `true` for any write tool (`jira_create_issue`,
  `jira_add_comment`, `confluence_create_page`, `confluence_update_page`).

### Read vs. write and audit

`jira_get_issue`, `confluence_get_page`, `bitbucket_get_repo`, and
`bitbucket_get_pull_request` are read tools. They pass through the allowlist and
do not touch the audit log.

`jira_create_issue`, `jira_add_comment`, `confluence_create_page`, and
`confluence_update_page` are write tools. They require `enable_writes = true` and
a configured `audit.path`. If audit logging fails, the request is rejected before
the upstream call.

## Testing with a mock upstream

If you do not have a live Atlassian account, point `atlassian.base_url` at a
mock server and add a fake token. The proxy does not validate the token itself;
Jira will. For example, start a tiny Python mock:

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
