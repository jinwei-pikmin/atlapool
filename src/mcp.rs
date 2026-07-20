use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::upstream::{RequestBody, UpstreamClient};
use crate::{agents, AppState};

const MCP_KEY_HEADER: &str = "x-atlapool-key";

#[derive(Deserialize)]
struct McpRequest {
    #[serde(default)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Deserialize)]
struct McpToolParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

struct ToolTarget {
    project: Option<String>,
    space: Option<String>,
    workspace: Option<String>,
    repo: Option<String>,
    method: Method,
    path: String,
    body: RequestBody,
    /// Staging fields for `jira_update_issue`; the final PUT body is built
    /// after authorization and write-gate, because `description_append` may
    /// require fetching the existing ADF first.
    update: Option<UpdateIssueFields>,
    /// Staging fields for `bitbucket_list_directory` and
    /// `bitbucket_get_file_content`; when `ref` is omitted the default branch is
    /// resolved after authorization.
    resolve_ref: Option<ResolveRef>,
}

#[derive(Default)]
struct UpdateIssueFields {
    issue_key: String,
    summary: Option<String>,
    assignee: Option<String>,
    description_append: Option<String>,
}

struct ResolveRef {
    /// Path prefix of the repository, e.g. `/repositories/WORK/my-repo`.
    repo_prefix: String,
    /// Sub-path within the repository; empty for root.
    sub_path: String,
    /// When true the resolved URL ends with `/` (directory listing).
    trailing_slash: bool,
}

impl ToolTarget {
    fn target_string(&self) -> String {
        self.project
            .as_deref()
            .or(self.space.as_deref())
            .or(self.workspace.as_deref())
            .or(self.repo.as_deref())
            .unwrap_or("unknown")
            .to_string()
    }
}

pub async fn mcp_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !state.config.mcp.enabled {
        return mcp_error(
            None,
            StatusCode::SERVICE_UNAVAILABLE,
            "mcp endpoint disabled",
        );
    }

    let request: McpRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return mcp_error(
                None,
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON: {}", e),
            )
        }
    };

    if request.jsonrpc != "2.0" {
        return mcp_error(request.id, StatusCode::BAD_REQUEST, "jsonrpc must be 2.0");
    }

    match request.method.as_str() {
        "initialize" => return initialize_handler(request.id).await,
        "tools/list" | "tools/call" => {}
        _ => {
            return mcp_error(
                request.id,
                StatusCode::NOT_IMPLEMENTED,
                "method not supported",
            );
        }
    }

    // tools/list and tools/call require a valid agent key.
    let key = headers
        .get(MCP_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());

    let Some(key) = key else {
        return mcp_error(
            request.id,
            StatusCode::UNAUTHORIZED,
            "missing or empty X-Atlapool-Key",
        );
    };

    let agent = match agents::find_agent(&state.config.agents, key) {
        Some(a) => a,
        None => {
            tracing::warn!("rejected request: unknown X-Atlapool-Key");
            return mcp_error(request.id, StatusCode::UNAUTHORIZED, "unknown key");
        }
    };

    if request.method == "tools/list" {
        return tools_list_handler(request.id, agent);
    }

    // tools/call from here on.
    let params: McpToolParams = match request.params {
        Some(v) => match serde_json::from_value(v) {
            Ok(p) => p,
            Err(e) => {
                return mcp_error(
                    request.id,
                    StatusCode::BAD_REQUEST,
                    &format!("invalid params: {}", e),
                );
            }
        },
        None => return mcp_error(request.id, StatusCode::BAD_REQUEST, "missing params"),
    };

    // Unknown or unsupported tools are rejected explicitly before allowlist or
    // write-gate checks, so those layers do not have to reason about empty targets.
    let workspace = state
        .config
        .bitbucket
        .as_ref()
        .and_then(|c| c.workspace.as_deref());
    let mut target = match resolve_target(&params.name, params.arguments.as_ref(), workspace) {
        Ok(t) => t,
        Err(msg) => return mcp_error(request.id, StatusCode::BAD_REQUEST, &msg),
    };

    if !agent.authorize(
        &params.name,
        target.project.as_deref(),
        target.space.as_deref(),
        target.workspace.as_deref(),
        target.repo.as_deref(),
    ) {
        tracing::warn!(
            agent_id = %agent.id,
            tool = %params.name,
            "rejected request: agent policy denies tool"
        );
        return mcp_error(
            request.id,
            StatusCode::FORBIDDEN,
            "not permitted by agent policy",
        );
    }

    // Write gate: conservative classification. Anything that is not a read prefix
    // is treated as a write. Per-agent enable_writes takes precedence over the
    // global [mcp] enable_writes default.
    let is_write = agents::classify_tool(&params.name) == agents::ToolKind::Write;
    let writes_enabled = agent
        .enable_writes
        .unwrap_or(state.config.mcp.enable_writes);
    if is_write && !writes_enabled {
        tracing::warn!(
            agent_id = %agent.id,
            tool = %params.name,
            "rejected request: write tools not enabled for agent"
        );
        return mcp_error(
            request.id,
            StatusCode::FORBIDDEN,
            "write tools not enabled for agent",
        );
    }

    // Fail-closed audit: write operations must be logged before the upstream call.
    // If the audit log cannot be written, the operation is aborted with 500.
    let target_string = target.target_string();
    let audit = if is_write { state.audit.clone() } else { None };

    if is_write {
        let Some(ref audit) = audit else {
            tracing::error!(
                agent_id = %agent.id,
                tool = %params.name,
                "rejected request: audit log not configured"
            );
            return mcp_error(
                request.id,
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit log not configured",
            );
        };

        if let Err(e) = audit
            .record_attempt(&agent.id, &params.name, &target_string)
            .await
        {
            tracing::error!(
                agent_id = %agent.id,
                tool = %params.name,
                error = %e,
                "rejected request: audit log write failed"
            );
            return mcp_error(
                request.id,
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit log write failed",
            );
        }
    }

    tracing::info!(
        agent_id = %agent.id,
        tool = %params.name,
        "forwarding authorized request to upstream"
    );

    // Route to the correct upstream client based on the tool namespace.
    let forward_result = if params.name.starts_with("confluence_") {
        let Some(confluence) = state.confluence.as_ref() else {
            return mcp_error(
                request.id,
                StatusCode::SERVICE_UNAVAILABLE,
                "confluence upstream not configured",
            );
        };
        forward(
            confluence,
            audit,
            agent.id.clone(),
            params.name.clone(),
            request.id.clone(),
            target,
        )
        .await
    } else if params.name.starts_with("bitbucket_") {
        let Some(bitbucket) = state.bitbucket.as_ref() else {
            return mcp_error(
                request.id,
                StatusCode::SERVICE_UNAVAILABLE,
                "bitbucket upstream not configured",
            );
        };

        // bitbucket_list_directory and bitbucket_get_file_content may need to
        // resolve the repository default branch before hitting the Source API.
        if let Some(resolve) = target.resolve_ref.take() {
            match resolve_bitbucket_default_branch(bitbucket, resolve).await {
                Ok(path) => target.path = path,
                Err((status, message)) => {
                    return mcp_error(request.id, status, &message);
                }
            }
        }

        forward(
            bitbucket,
            audit,
            agent.id.clone(),
            params.name.clone(),
            request.id.clone(),
            target,
        )
        .await
    } else {
        let Some(jira) = state.jira.as_ref() else {
            return mcp_error(
                request.id,
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream not configured",
            );
        };

        // jira_update_issue may need to fetch the existing description before
        // building the upstream PUT body. The read is safe to issue now because
        // authorization and the write-gate have already passed.
        if let Some(update) = target.update.take() {
            match prepare_jira_update_body(jira, update).await {
                Ok(body) => target.body = body,
                Err((status, message)) => {
                    if let Some(ref audit) = audit {
                        audit
                            .record_result(
                                &agent.id,
                                &params.name,
                                &target.target_string(),
                                false,
                                Some(status.as_u16()),
                                Some(message.as_str()),
                            )
                            .await;
                    }
                    return mcp_error(request.id, status, &message);
                }
            }
        }

        forward(
            jira,
            audit,
            agent.id.clone(),
            params.name.clone(),
            request.id.clone(),
            target,
        )
        .await
    };

    match forward_result {
        Ok(response) => response,
        Err((status, message)) => mcp_error(request.id, status, &message),
    }
}

async fn forward<C: UpstreamClient>(
    client: &C,
    audit: Option<crate::audit::AuditLog>,
    agent_id: String,
    tool: String,
    request_id: Option<Value>,
    target: ToolTarget,
) -> Result<(StatusCode, Json<Value>), (StatusCode, String)> {
    let target_string = target.target_string();

    let upstream_request = match client
        .build_request(target.method, &target.path, target.body)
        .await
    {
        Ok(req) => req,
        Err(e) => {
            let error_message = e.to_string();
            if let Some(ref audit) = audit {
                audit
                    .record_result(
                        &agent_id,
                        &tool,
                        &target_string,
                        false,
                        None,
                        Some(error_message.as_str()),
                    )
                    .await;
            }
            return Err((StatusCode::BAD_GATEWAY, error_message));
        }
    };

    let upstream_resp = match client.execute(upstream_request).await {
        Ok(resp) => resp,
        Err(e) => {
            let message = format!("upstream request failed: {}", e);
            if let Some(ref audit) = audit {
                audit
                    .record_result(
                        &agent_id,
                        &tool,
                        &target_string,
                        false,
                        None,
                        Some(message.as_str()),
                    )
                    .await;
            }
            return Err((StatusCode::BAD_GATEWAY, message));
        }
    };

    let upstream_status = upstream_resp.status();
    let upstream_text = match upstream_resp.text().await {
        Ok(text) => text,
        Err(e) => {
            let message = format!("failed to read upstream: {}", e);
            if let Some(ref audit) = audit {
                audit
                    .record_result(
                        &agent_id,
                        &tool,
                        &target_string,
                        false,
                        Some(upstream_status.as_u16()),
                        Some(message.as_str()),
                    )
                    .await;
            }
            return Err((StatusCode::BAD_GATEWAY, message));
        }
    };
    let upstream_body: Value = if upstream_text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&upstream_text).unwrap_or_else(|_| json!(upstream_text))
    };

    let (status, envelope) = if upstream_status.is_success() {
        if let Some(ref audit) = audit {
            audit
                .record_result(
                    &agent_id,
                    &tool,
                    &target_string,
                    true,
                    Some(upstream_status.as_u16()),
                    None,
                )
                .await;
        }
        (
            StatusCode::OK,
            json!({"jsonrpc":"2.0","id": request_id, "result": upstream_body}),
        )
    } else {
        let message = format!("upstream returned {}", upstream_status);
        if let Some(ref audit) = audit {
            audit
                .record_result(
                    &agent_id,
                    &tool,
                    &target_string,
                    false,
                    Some(upstream_status.as_u16()),
                    Some(message.as_str()),
                )
                .await;
        }
        (
            StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32000,
                    "message": message,
                    "data": upstream_body
                }
            }),
        )
    };

    Ok((status, Json(envelope)))
}

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

fn mcp_success(id: Option<Value>, result: Value) -> (StatusCode, Json<Value>) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    });
    (StatusCode::OK, Json(body))
}

fn mcp_error(id: Option<Value>, status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": message }
    });
    (status, Json(body))
}

async fn initialize_handler(id: Option<Value>) -> (StatusCode, Json<Value>) {
    mcp_success(
        id,
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "atlapool",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

struct Tool {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
}

fn tools_list_handler(id: Option<Value>, agent: &agents::AgentConfig) -> (StatusCode, Json<Value>) {
    let allowed: Vec<Value> = all_tools()
        .into_iter()
        .filter(|t| agent.tools.iter().any(|allowed| allowed == t.name))
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema
            })
        })
        .collect();
    mcp_success(id, json!({ "tools": allowed }))
}

fn all_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "jira_get_issue",
            description: "Fetch a Jira issue by key.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue_key": { "type": "string", "description": "Issue key, e.g. PROJ-123" }
                },
                "required": ["issue_key"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "jira_create_issue",
            description: "Create a new Jira issue.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Jira project key" },
                    "summary": { "type": "string", "description": "Issue summary" },
                    "fields": { "type": "object", "description": "Additional Jira fields" }
                },
                "required": ["project", "summary"],
                "additionalProperties": true
            }),
        },
        Tool {
            name: "jira_add_comment",
            description: "Add a comment to a Jira issue.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue_key": { "type": "string", "description": "Issue key, e.g. PROJ-123" },
                    "body": { "description": "Comment body (ADF object or string)" }
                },
                "required": ["issue_key", "body"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "jira_update_issue",
            description: "Update a Jira issue (summary, assignee, or append description).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue_key": { "type": "string", "description": "Issue key, e.g. PROJ-123" },
                    "summary": { "type": "string", "description": "Overwrite summary" },
                    "assignee": { "type": "string", "description": "Atlassian accountId" },
                    "description_append": { "type": "string", "description": "Text to append to description" }
                },
                "required": ["issue_key"],
                "minProperties": 1,
                "additionalProperties": false
            }),
        },
        Tool {
            name: "jira_get_transitions",
            description: "List available transitions for a Jira issue.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue_key": { "type": "string", "description": "Issue key, e.g. PROJ-123" }
                },
                "required": ["issue_key"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "jira_transition_issue",
            description: "Transition a Jira issue to a new status.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "issue_key": { "type": "string", "description": "Issue key, e.g. PROJ-123" },
                    "transition_id": { "type": "string", "description": "Numeric transition id" }
                },
                "required": ["issue_key", "transition_id"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "confluence_get_page",
            description: "Fetch a Confluence page by ID.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "page_id": { "type": "string", "description": "Numeric page ID" },
                    "space": { "type": "string", "description": "Space key for allowlist" }
                },
                "required": ["page_id", "space"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "confluence_create_page",
            description: "Create a Confluence page.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "space": { "type": "string", "description": "Space key for allowlist" },
                    "space_id": { "type": "string", "description": "Numeric space ID" },
                    "title": { "type": "string" },
                    "body": { "type": "string", "description": "Storage-format HTML" },
                    "parent_id": { "type": "string", "description": "Optional numeric parent page ID" }
                },
                "required": ["space", "space_id", "title", "body"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "confluence_update_page",
            description: "Update a Confluence page.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "space": { "type": "string", "description": "Space key for allowlist" },
                    "space_id": { "type": "string", "description": "Numeric space ID" },
                    "page_id": { "type": "string", "description": "Numeric page ID" },
                    "title": { "type": "string" },
                    "version": { "type": "string", "description": "Positive integer version" },
                    "body": { "type": "string", "description": "Storage-format HTML" }
                },
                "required": ["space", "space_id", "page_id", "title", "version", "body"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_get_repo",
            description: "Fetch a Bitbucket repository.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" }
                },
                "required": ["repo_slug"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_get_pull_request",
            description: "Fetch a Bitbucket pull request.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "pull_request_id": { "type": "string", "description": "Numeric pull request ID" }
                },
                "required": ["repo_slug", "pull_request_id"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_list_branches",
            description: "List branches in a Bitbucket repository.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" }
                },
                "required": ["repo_slug"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_list_directory",
            description: "List files and directories at a path in a Bitbucket repository.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "path": { "type": "string", "description": "Directory path, defaults to root" },
                    "ref": { "type": "string", "description": "Branch/tag/commit hash, defaults to default branch" }
                },
                "required": ["repo_slug"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_get_file_content",
            description: "Read the raw contents of a file in a Bitbucket repository.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "path": { "type": "string", "description": "File path" },
                    "ref": { "type": "string", "description": "Branch/tag/commit hash, defaults to default branch" }
                },
                "required": ["repo_slug", "path"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_create_repo",
            description: "Create a new Bitbucket repository.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "is_private": { "type": "boolean", "description": "Defaults to true" }
                },
                "required": ["repo_slug"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_create_branch",
            description: "Create a branch in a Bitbucket repository.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "branch_name": { "type": "string" },
                    "target_hash": { "type": "string", "description": "Commit hash to point the new branch to" }
                },
                "required": ["repo_slug", "branch_name", "target_hash"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_create_commit",
            description: "Create a commit by uploading files to a Bitbucket repository.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "message": { "type": "string", "description": "Commit message" },
                    "branch": { "type": "string" },
                    "parents": { "type": "array", "items": { "type": "string" } },
                    "files": { "type": "object", "description": "Map of file path to content" }
                },
                "required": ["repo_slug", "message", "branch", "files"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_create_pull_request",
            description: "Create a Bitbucket pull request.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "title": { "type": "string" },
                    "source_branch": { "type": "string" },
                    "destination_branch": { "type": "string" },
                    "description": { "type": "string" }
                },
                "required": ["repo_slug", "title", "source_branch"],
                "additionalProperties": false
            }),
        },
    ]
}

fn resolve_target(
    tool: &str,
    args: Option<&Value>,
    workspace: Option<&str>,
) -> Result<ToolTarget, String> {
    let valid_repo_slug = |s: &str| -> Result<(), String> {
        if s.is_empty() {
            return Err("repo_slug must not be empty".into());
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
        {
            return Err("repo_slug contains invalid characters".into());
        }
        Ok(())
    };
    let valid_repo_path = |s: &str| -> Result<(), String> {
        if s.starts_with('/') {
            return Err("path must not start with '/'".into());
        }
        for segment in s.split('/') {
            if segment == ".." {
                return Err("path contains invalid '..' segment".into());
            }
        }
        Ok(())
    };
    let valid_ref_name = |s: &str| -> Result<(), String> {
        if s.is_empty() {
            return Err("ref must not be empty".into());
        }
        if s == "." || s == ".." {
            return Err("ref must not be '.' or '..'".into());
        }
        if s.starts_with('/') {
            return Err("ref must not start with '/'".into());
        }
        if s.contains('/') {
            return Err("ref must not contain '/'; use a commit hash for branches with '/'".into());
        }
        Ok(())
    };
    let valid_issue_key = |s: &str| -> Result<(), String> {
        let Some((project, number)) = s.split_once('-') else {
            return Err("issue_key must be in PROJECT-NUMBER format".into());
        };
        if project.is_empty()
            || !project
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err("issue_key project part contains invalid characters".into());
        }
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
            return Err("issue_key number part must be numeric".into());
        }
        Ok(())
    };

    match tool {
        "jira_get_issue" => {
            let args = args.ok_or("missing arguments")?;
            let issue_key = args
                .get("issue_key")
                .and_then(|v| v.as_str())
                .ok_or("missing issue_key")?;
            valid_issue_key(issue_key)?;
            let project = issue_key.split_once('-').map(|(p, _)| p.to_string());
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project,
                space: None,
                method: Method::GET,
                path: format!("/rest/api/3/issue/{issue_key}"),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
            })
        }
        "jira_create_issue" => {
            let args = args.ok_or("missing arguments")?;
            let project = args
                .get("project")
                .and_then(|v| v.as_str())
                .ok_or("missing project")?
                .to_string();
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project: Some(project),
                space: None,
                method: Method::POST,
                path: "/rest/api/3/issue".into(),
                body: RequestBody::json(args.clone()),
                update: None,
                resolve_ref: None,
            })
        }
        "jira_add_comment" => {
            let args = args.ok_or("missing arguments")?;
            let issue_key = args
                .get("issue_key")
                .and_then(|v| v.as_str())
                .ok_or("missing issue_key")?;
            valid_issue_key(issue_key)?;
            let project = issue_key.split_once('-').map(|(p, _)| p.to_string());
            let body = args.get("body").cloned().ok_or("missing body")?;
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project,
                space: None,
                method: Method::POST,
                path: format!("/rest/api/3/issue/{issue_key}/comment"),
                body: RequestBody::json(json!({ "body": body })),
                update: None,
                resolve_ref: None,
            })
        }
        "confluence_get_page" => {
            let args = args.ok_or("missing arguments")?;
            let space = args
                .get("space")
                .and_then(|v| v.as_str())
                .ok_or("missing space")?
                .to_string();
            if space.is_empty() {
                return Err("space must not be empty".into());
            }
            let page_id = args
                .get("page_id")
                .and_then(|v| v.as_str())
                .ok_or("missing page_id")?;
            if page_id.is_empty() || !page_id.chars().all(|c| c.is_ascii_digit()) {
                return Err("page_id must be a non-empty numeric id".into());
            }
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project: None,
                space: Some(space),
                method: Method::GET,
                path: format!("/wiki/api/v2/pages/{page_id}?body-format=view"),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
            })
        }
        "confluence_create_page" => {
            let args = args.ok_or("missing arguments")?;
            let space = args
                .get("space")
                .and_then(|v| v.as_str())
                .ok_or("missing space")?
                .to_string();
            if space.is_empty() {
                return Err("space must not be empty".into());
            }
            let space_id = args
                .get("space_id")
                .and_then(|v| v.as_str())
                .ok_or("missing space_id")?;
            if space_id.is_empty() || !space_id.chars().all(|c| c.is_ascii_digit()) {
                return Err("space_id must be a non-empty numeric id".into());
            }
            let title = args
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("missing title")?;
            let body = args
                .get("body")
                .filter(|v| !v.is_null())
                .cloned()
                .ok_or("missing body")?;
            let body_value = if let Some(s) = body.as_str() {
                json!({ "representation": "storage", "value": s })
            } else {
                body
            };
            let parent_id = args
                .get("parent_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            if let Some(id) = parent_id {
                if !id.chars().all(|c| c.is_ascii_digit()) {
                    return Err("parent_id must be a non-empty numeric id".into());
                }
            }
            let mut payload = json!({
                "spaceId": space_id,
                "status": "current",
                "title": title,
                "body": body_value
            });
            if let Some(id) = parent_id {
                payload["parentId"] = json!(id);
            }
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project: None,
                space: Some(space),
                method: Method::POST,
                path: "/wiki/api/v2/pages".into(),
                body: RequestBody::json(payload),
                update: None,
                resolve_ref: None,
            })
        }
        "confluence_update_page" => {
            let args = args.ok_or("missing arguments")?;
            let space = args
                .get("space")
                .and_then(|v| v.as_str())
                .ok_or("missing space")?
                .to_string();
            if space.is_empty() {
                return Err("space must not be empty".into());
            }
            let space_id = args
                .get("space_id")
                .and_then(|v| v.as_str())
                .ok_or("missing space_id")?;
            if space_id.is_empty() || !space_id.chars().all(|c| c.is_ascii_digit()) {
                return Err("space_id must be a non-empty numeric id".into());
            }
            let page_id = args
                .get("page_id")
                .and_then(|v| v.as_str())
                .ok_or("missing page_id")?;
            if page_id.is_empty() || !page_id.chars().all(|c| c.is_ascii_digit()) {
                return Err("page_id must be a non-empty numeric id".into());
            }
            let title = args
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("missing title")?;
            let version = if let Some(n) = args.get("version").and_then(|v| v.as_u64()) {
                n
            } else if let Some(s) = args.get("version").and_then(|v| v.as_str()) {
                s.parse::<u64>()
                    .map_err(|_| "version must be a positive integer")?
            } else {
                return Err("missing version".into());
            };
            if version == 0 {
                return Err("version must be a positive integer".into());
            }
            let body = args
                .get("body")
                .filter(|v| !v.is_null())
                .cloned()
                .ok_or("missing body")?;
            let body_value = if let Some(s) = body.as_str() {
                json!({ "representation": "storage", "value": s })
            } else {
                body
            };
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project: None,
                space: Some(space),
                method: Method::PUT,
                path: format!("/wiki/api/v2/pages/{page_id}"),
                body: RequestBody::json(json!({
                    "id": page_id,
                    "spaceId": space_id,
                    "status": "current",
                    "title": title,
                    "version": { "number": version },
                    "body": body_value
                })),
                update: None,
                resolve_ref: None,
            })
        }
        "bitbucket_list_branches" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::GET,
                path: format!("/repositories/{workspace}/{repo_slug}/refs/branches"),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
            })
        }
        "bitbucket_list_directory" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let sub_path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !sub_path.is_empty() {
                valid_repo_path(&sub_path)?;
            }
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            let repo_prefix = format!("/repositories/{workspace}/{repo_slug}");
            if let Some(ref_name) = args.get("ref").and_then(|v| v.as_str()) {
                valid_ref_name(ref_name)?;
                let path = build_bitbucket_src_path(&repo_prefix, ref_name, &sub_path, true);
                Ok(ToolTarget {
                    workspace: Some(workspace.into()),
                    repo: Some(repo_slug.into()),
                    project: None,
                    space: None,
                    method: Method::GET,
                    path,
                    body: RequestBody::None,
                    update: None,
                    resolve_ref: None,
                })
            } else {
                Ok(ToolTarget {
                    workspace: Some(workspace.into()),
                    repo: Some(repo_slug.into()),
                    project: None,
                    space: None,
                    method: Method::GET,
                    path: String::new(),
                    body: RequestBody::None,
                    update: None,
                    resolve_ref: Some(ResolveRef {
                        repo_prefix,
                        sub_path,
                        trailing_slash: true,
                    }),
                })
            }
        }
        "bitbucket_get_file_content" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let sub_path = args
                .get("path")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("missing path")?
                .to_string();
            valid_repo_path(&sub_path)?;
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            let repo_prefix = format!("/repositories/{workspace}/{repo_slug}");
            if let Some(ref_name) = args.get("ref").and_then(|v| v.as_str()) {
                valid_ref_name(ref_name)?;
                let path = build_bitbucket_src_path(&repo_prefix, ref_name, &sub_path, false);
                Ok(ToolTarget {
                    workspace: Some(workspace.into()),
                    repo: Some(repo_slug.into()),
                    project: None,
                    space: None,
                    method: Method::GET,
                    path,
                    body: RequestBody::None,
                    update: None,
                    resolve_ref: None,
                })
            } else {
                Ok(ToolTarget {
                    workspace: Some(workspace.into()),
                    repo: Some(repo_slug.into()),
                    project: None,
                    space: None,
                    method: Method::GET,
                    path: String::new(),
                    body: RequestBody::None,
                    update: None,
                    resolve_ref: Some(ResolveRef {
                        repo_prefix,
                        sub_path,
                        trailing_slash: false,
                    }),
                })
            }
        }
        "bitbucket_get_repo" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::GET,
                path: format!("/repositories/{workspace}/{repo_slug}"),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
            })
        }
        "bitbucket_get_pull_request" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let pull_request_id = args
                .get("pull_request_id")
                .and_then(|v| v.as_str())
                .ok_or("missing pull_request_id")?;
            if pull_request_id.is_empty() || !pull_request_id.chars().all(|c| c.is_ascii_digit()) {
                return Err("pull_request_id must be a non-empty numeric id".into());
            }
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::GET,
                path: format!(
                    "/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}"
                ),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
            })
        }
        "bitbucket_create_branch" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let branch_name = args
                .get("branch_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("missing branch_name")?;
            let target_hash = args
                .get("target_hash")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("missing target_hash")?;
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::POST,
                path: format!("/repositories/{workspace}/{repo_slug}/refs/branches"),
                body: RequestBody::json(json!({
                    "name": branch_name,
                    "target": { "hash": target_hash }
                })),
                update: None,
                resolve_ref: None,
            })
        }
        "bitbucket_create_commit" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("missing message")?;
            let branch = args
                .get("branch")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("missing branch")?;
            let parents = if let Some(arr) = args.get("parents").and_then(|v| v.as_array()) {
                arr.iter()
                    .map(|v| v.as_str().unwrap_or("").to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let files = args
                .get("files")
                .and_then(|v| v.as_object())
                .ok_or("missing files")?;
            let files: Vec<(String, String)> = files
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .filter(|(_, v)| !v.is_empty())
                .collect();
            if files.is_empty() {
                return Err("files must contain at least one non-empty file".into());
            }
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            let mut form_fields = vec![
                ("message".into(), message.into()),
                ("branch".into(), branch.into()),
            ];
            for parent in parents {
                form_fields.push(("parents".into(), parent));
            }
            for (path, content) in files {
                form_fields.push((path, content));
            }
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::POST,
                path: format!("/repositories/{workspace}/{repo_slug}/src"),
                body: RequestBody::form(form_fields),
                update: None,
                resolve_ref: None,
            })
        }
        "bitbucket_create_pull_request" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let title = args
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("missing title")?;
            let source_branch = args
                .get("source_branch")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or("missing source_branch")?;
            let destination_branch = args
                .get("destination_branch")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let description = args.get("description").and_then(|v| v.as_str());
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            let mut body = json!({
                "title": title,
                "source": { "branch": { "name": source_branch } }
            });
            if let Some(destination) = destination_branch {
                body["destination"] = json!({ "branch": { "name": destination } });
            }
            if let Some(description) = description {
                body["description"] = json!(description);
            }
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::POST,
                path: format!("/repositories/{workspace}/{repo_slug}/pullrequests"),
                body: RequestBody::json(body),
                update: None,
                resolve_ref: None,
            })
        }
        "bitbucket_create_repo" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let is_private = args
                .get("is_private")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::POST,
                path: format!("/repositories/{workspace}/{repo_slug}"),
                body: RequestBody::json(json!({ "is_private": is_private })),
                update: None,
                resolve_ref: None,
            })
        }
        "jira_update_issue" => {
            let args = args.ok_or("missing arguments")?;
            let issue_key = args
                .get("issue_key")
                .and_then(|v| v.as_str())
                .ok_or("missing issue_key")?;
            valid_issue_key(issue_key)?;
            let project = issue_key.split_once('-').map(|(p, _)| p.to_string());
            let summary = args
                .get("summary")
                .and_then(|v| v.as_str())
                .map(String::from);
            let assignee = args
                .get("assignee")
                .and_then(|v| v.as_str())
                .map(String::from);
            let description_append = args
                .get("description_append")
                .and_then(|v| v.as_str())
                .map(String::from);
            if summary.is_none() && assignee.is_none() && description_append.is_none() {
                return Err(
                    "at least one of summary, assignee, or description_append must be provided"
                        .into(),
                );
            }
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project,
                space: None,
                method: Method::PUT,
                path: format!("/rest/api/3/issue/{issue_key}"),
                body: RequestBody::None,
                update: Some(UpdateIssueFields {
                    issue_key: issue_key.to_string(),
                    summary,
                    assignee,
                    description_append,
                }),
                resolve_ref: None,
            })
        }
        "jira_get_transitions" => {
            let args = args.ok_or("missing arguments")?;
            let issue_key = args
                .get("issue_key")
                .and_then(|v| v.as_str())
                .ok_or("missing issue_key")?;
            valid_issue_key(issue_key)?;
            let project = issue_key.split_once('-').map(|(p, _)| p.to_string());
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project,
                space: None,
                method: Method::GET,
                path: format!("/rest/api/3/issue/{issue_key}/transitions"),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
            })
        }
        "jira_transition_issue" => {
            let args = args.ok_or("missing arguments")?;
            let issue_key = args
                .get("issue_key")
                .and_then(|v| v.as_str())
                .ok_or("missing issue_key")?;
            valid_issue_key(issue_key)?;
            let project = issue_key.split_once('-').map(|(p, _)| p.to_string());
            let transition_id = args
                .get("transition_id")
                .and_then(|v| v.as_str())
                .ok_or("missing transition_id")?;
            if transition_id.is_empty() || !transition_id.chars().all(|c| c.is_ascii_digit()) {
                return Err("transition_id must be a non-empty numeric id".into());
            }
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project,
                space: None,
                method: Method::POST,
                path: format!("/rest/api/3/issue/{issue_key}/transitions"),
                body: RequestBody::json(json!({ "transition": { "id": transition_id } })),
                update: None,
                resolve_ref: None,
            })
        }
        _ => Err("unsupported tool".into()),
    }
}

/// Percent-encode a single URL path segment using the `url` crate's rules.
fn encode_path_segment(segment: &str) -> String {
    let mut url = reqwest::Url::parse("https://x.com").expect("static base URL");
    url.path_segments_mut()
        .expect("mutable path segments")
        .push(segment);
    url.path().trim_start_matches('/').to_string()
}

/// Build the Bitbucket Source API path `/repositories/{workspace}/{repo}/src/{ref}/{path}`,
/// encoding each path segment individually.
fn build_bitbucket_src_path(
    repo_prefix: &str,
    ref_name: &str,
    sub_path: &str,
    trailing_slash: bool,
) -> String {
    let mut path = format!("{}/src/{}", repo_prefix, encode_path_segment(ref_name));
    let encoded_sub: Vec<String> = sub_path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(encode_path_segment)
        .collect();
    if !encoded_sub.is_empty() {
        path.push('/');
        path.push_str(&encoded_sub.join("/"));
    }
    if trailing_slash {
        path.push('/');
    }
    path
}

/// Build the upstream PUT body for `jira_update_issue`.
///
/// `summary` and `assignee` are set directly. For `description_append`, the
/// existing issue description is fetched, the new content is appended as an
/// ADF `panel`, and the merged ADF is included in the payload.
async fn prepare_jira_update_body<C: UpstreamClient>(
    client: &C,
    update: UpdateIssueFields,
) -> Result<RequestBody, (StatusCode, String)> {
    let mut fields = serde_json::Map::new();
    if let Some(summary) = update.summary {
        fields.insert("summary".into(), Value::String(summary));
    }
    if let Some(assignee) = update.assignee {
        fields.insert("assignee".into(), json!({ "accountId": assignee }));
    }
    if let Some(append) = update.description_append {
        let get_request = client
            .build_request(
                Method::GET,
                &format!("/rest/api/3/issue/{}?fields=description", update.issue_key),
                RequestBody::None,
            )
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        let get_response = client.execute(get_request).await.map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("upstream GET failed: {}", e),
            )
        })?;
        if !get_response.status().is_success() {
            return Err((
                StatusCode::from_u16(get_response.status().as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY),
                format!("upstream GET returned {}", get_response.status()),
            ));
        }
        let issue: Value = get_response.json().await.map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("failed to parse upstream GET response: {}", e),
            )
        })?;
        let existing = issue.get("fields").and_then(|f| f.get("description"));
        let merged =
            merge_adf_description(existing, &append).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        fields.insert("description".into(), merged);
    }
    Ok(RequestBody::json(json!({ "fields": fields })))
}

/// Resolve the repository default branch and build the final Source API path
/// for `bitbucket_list_directory` and `bitbucket_get_file_content`.
async fn resolve_bitbucket_default_branch<C: UpstreamClient>(
    client: &C,
    resolve: ResolveRef,
) -> Result<String, (StatusCode, String)> {
    let repo_request = client
        .build_request(Method::GET, &resolve.repo_prefix, RequestBody::None)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let repo_response = client.execute(repo_request).await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("upstream GET failed: {}", e),
        )
    })?;
    if !repo_response.status().is_success() {
        return Err((
            StatusCode::from_u16(repo_response.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY),
            format!("upstream GET returned {}", repo_response.status()),
        ));
    }
    let repo: Value = repo_response.json().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to parse upstream GET response: {}", e),
        )
    })?;
    let branch = repo
        .get("mainbranch")
        .and_then(|m| m.get("name"))
        .and_then(|n| n.as_str())
        .ok_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                "missing mainbranch.name in repository response".to_string(),
            )
        })?;
    Ok(build_bitbucket_src_path(
        &resolve.repo_prefix,
        branch,
        &resolve.sub_path,
        resolve.trailing_slash,
    ))
}

/// Append `text` to an existing ADF description as a new `panel` node titled
/// "📋 分析補充".
fn merge_adf_description(existing: Option<&Value>, text: &str) -> Result<Value, String> {
    let mut doc = match existing {
        Some(Value::Null) | None => {
            json!({ "type": "doc", "version": 1, "content": [] })
        }
        Some(Value::Object(_)) => existing.cloned().unwrap(),
        Some(_) => return Err("existing description is not a valid ADF object".into()),
    };

    if !doc.get("type").map(|v| v.is_string()).unwrap_or(false) {
        doc["type"] = json!("doc");
    }
    if !doc.get("content").map(|v| v.is_array()).unwrap_or(false) {
        doc["content"] = json!([]);
    }

    let panel = json!({
        "type": "panel",
        "attrs": { "panelType": "info" },
        "content": [
            {
                "type": "paragraph",
                "content": [{ "type": "text", "text": "📋 分析補充" }]
            },
            {
                "type": "paragraph",
                "content": [{ "type": "text", "text": text }]
            }
        ]
    });

    let content = doc
        .get_mut("content")
        .and_then(|v| v.as_array_mut())
        .ok_or("ADF content is not an array")?;
    content.push(panel);
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentConfig;
    use crate::audit::AuditLog;
    use crate::bitbucket::BitbucketClient;
    use crate::config::{AtlassianConfig, AuditConfig, BitbucketConfig, Config, McpConfig};
    use crate::confluence::ConfluenceClient;
    use crate::secrets::SecretString;
    use crate::upstream::JiraClient;
    use axum::body::Body;
    use axum::extract::Path;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, post};
    use axum::Router;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tower::ServiceExt;

    fn temp_audit_path() -> String {
        std::env::temp_dir()
            .join(format!(
                "atlapool-audit-{}-{}.jsonl",
                std::process::id(),
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            ))
            .to_string_lossy()
            .into_owned()
    }

    fn test_config(base_url: String, enable_writes: bool, audit_path: Option<String>) -> Config {
        Config {
            port: 0,
            atlassian: Some(AtlassianConfig {
                base_url: Some(base_url),

                cloud_id: None,
                token: Some(SecretString::new("test-token")),
            }),
            bitbucket: None,
            mcp: McpConfig {
                enabled: true,
                ..Default::default()
            },
            audit: AuditConfig { path: audit_path },
            agents: vec![AgentConfig {
                id: "demo".into(),
                keys: vec![SecretString::new("agent-key")],
                tools: vec![
                    "jira_get_issue".into(),
                    "jira_create_issue".into(),
                    "jira_add_comment".into(),
                    "jira_update_issue".into(),
                    "jira_get_transitions".into(),
                    "jira_transition_issue".into(),
                ],
                projects: vec!["PROJ".into()],
                spaces: vec![],
                bitbucket_workspaces: vec![],
                bitbucket_repos: vec![],
                enable_writes: Some(enable_writes),
            }],
        }
    }

    fn confluence_test_config(
        base_url: String,
        enable_writes: bool,
        audit_path: Option<String>,
        spaces: Vec<String>,
    ) -> Config {
        Config {
            port: 0,
            atlassian: Some(AtlassianConfig {
                base_url: Some(base_url),

                cloud_id: None,
                token: Some(SecretString::new("test-token")),
            }),
            bitbucket: None,
            mcp: McpConfig {
                enabled: true,
                ..Default::default()
            },
            audit: AuditConfig { path: audit_path },
            agents: vec![AgentConfig {
                id: "demo".into(),
                keys: vec![SecretString::new("agent-key")],
                tools: vec![
                    "confluence_get_page".into(),
                    "confluence_create_page".into(),
                    "confluence_update_page".into(),
                ],
                projects: vec![],
                spaces,
                bitbucket_workspaces: vec![],
                bitbucket_repos: vec![],
                enable_writes: Some(enable_writes),
            }],
        }
    }

    fn bitbucket_test_config(
        base_url: String,
        audit_path: Option<String>,
        workspaces: Vec<String>,
        repos: Vec<String>,
    ) -> Config {
        Config {
            port: 0,
            atlassian: None,
            bitbucket: Some(BitbucketConfig {
                base_url: Some(base_url),
                workspace: Some("WORK".into()),
                token: Some(SecretString::new("test-token")),
                oauth: None,
            }),
            mcp: McpConfig {
                enabled: true,
                ..Default::default()
            },
            audit: AuditConfig { path: audit_path },
            agents: vec![AgentConfig {
                id: "demo".into(),
                keys: vec![SecretString::new("agent-key")],
                tools: vec![
                    "bitbucket_get_repo".into(),
                    "bitbucket_get_pull_request".into(),
                    "bitbucket_list_branches".into(),
                    "bitbucket_list_directory".into(),
                    "bitbucket_get_file_content".into(),
                ],
                projects: vec![],
                spaces: vec![],
                bitbucket_workspaces: workspaces,
                bitbucket_repos: repos,
                enable_writes: None,
            }],
        }
    }

    fn build_request(tool: &str, args: Value, key: Option<&str>) -> Request<Body> {
        build_jsonrpc_request(
            "tools/call",
            json!({
                "name": tool,
                "arguments": args
            }),
            key,
        )
    }

    fn build_jsonrpc_request(method: &str, params: Value, key: Option<&str>) -> Request<Body> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        })
        .to_string();

        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");

        if let Some(k) = key {
            builder = builder.header("X-Atlapool-Key", k);
        }
        // A sensitive header that must never reach the upstream Jira server.
        builder = builder.header("cookie", "session=bad");
        builder.body(Body::from(body)).unwrap()
    }

    async fn mock_jira_server() -> (u16, Arc<Mutex<Vec<HeaderMap>>>) {
        #[derive(Clone)]
        struct MockState {
            headers: Arc<Mutex<Vec<HeaderMap>>>,
        }

        let state = MockState {
            headers: Arc::new(Mutex::new(Vec::new())),
        };
        let captured = state.headers.clone();

        let get_handler =
            |State(s): State<MockState>, Path(_): Path<String>, headers: HeaderMap| async move {
                s.headers.lock().unwrap().push(headers);
                Json(json!({
                    "id": "10000",
                    "key": "PROJ-123",
                    "fields": {
                        "description": {
                            "type": "doc",
                            "version": 1,
                            "content": [
                                {
                                    "type": "paragraph",
                                    "content": [{ "type": "text", "text": "original" }]
                                }
                            ]
                        }
                    }
                }))
            };

        let post_handler = |State(s): State<MockState>, headers: HeaderMap, body: Bytes| async move {
            s.headers.lock().unwrap().push(headers);
            // Echo back a minimal created issue stub plus the received project.
            let payload: Value = serde_json::from_slice(&body).unwrap_or_default();
            Json(json!({
                "id": "10000",
                "key": "PROJ-123",
                "fields": { "project": payload }
            }))
        };

        let update_handler = |State(s): State<MockState>,
                              Path(_): Path<String>,
                              headers: HeaderMap,
                              body: Bytes| async move {
            s.headers.lock().unwrap().push(headers);
            let payload: Value = serde_json::from_slice(&body).unwrap_or_default();
            Json(payload)
        };

        let get_transitions_handler =
            |State(s): State<MockState>, Path(_): Path<String>, headers: HeaderMap| async move {
                s.headers.lock().unwrap().push(headers);
                Json(json!({
                    "transitions": [
                        { "id": "1", "name": "To Do" },
                        { "id": "2", "name": "In Progress" }
                    ]
                }))
            };

        let do_transition_handler = |State(s): State<MockState>,
                                     Path(_): Path<String>,
                                     headers: HeaderMap,
                                     body: Bytes| async move {
            s.headers.lock().unwrap().push(headers);
            let payload: Value = serde_json::from_slice(&body).unwrap_or_default();
            Json(json!({
                "id": "10000",
                "key": "PROJ-123",
                "transition": payload
            }))
        };

        let comment_handler = |State(s): State<MockState>,
                               Path(_): Path<String>,
                               headers: HeaderMap,
                               body: Bytes| async move {
            s.headers.lock().unwrap().push(headers);
            let payload: Value = serde_json::from_slice(&body).unwrap_or_default();
            Json(json!({
                "id": "10001",
                "self": "/rest/api/3/comment/10001",
                "body": payload
            }))
        };

        let app = Router::new()
            .route(
                "/rest/api/3/issue/{key}",
                get(get_handler).put(update_handler),
            )
            .route("/rest/api/3/issue", post(post_handler))
            .route("/rest/api/3/issue/{key}/comment", post(comment_handler))
            .route(
                "/rest/api/3/issue/{key}/transitions",
                get(get_transitions_handler).post(do_transition_handler),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, captured)
    }

    async fn mock_confluence_server() -> (u16, Arc<Mutex<Vec<HeaderMap>>>) {
        #[derive(Clone)]
        struct MockState {
            headers: Arc<Mutex<Vec<HeaderMap>>>,
        }

        let state = MockState {
            headers: Arc::new(Mutex::new(Vec::new())),
        };
        let captured = state.headers.clone();

        let get_handler =
            |State(s): State<MockState>, Path(id): Path<String>, headers: HeaderMap| async move {
                s.headers.lock().unwrap().push(headers);
                Json(json!({
                    "id": id,
                    "title": "Demo Page",
                    "spaceId": "12345",
                    "body": { "view": { "value": "<p>Hello</p>" } }
                }))
            };

        let create_handler = |State(s): State<MockState>, headers: HeaderMap, body: Bytes| async move {
            s.headers.lock().unwrap().push(headers);
            let payload: Value = serde_json::from_slice(&body).unwrap_or_default();
            let title = payload["title"].as_str().unwrap_or("Untitled");
            Json(json!({
                "id": "67890",
                "title": title,
                "spaceId": payload["spaceId"],
                "parentId": payload["parentId"],
                "body": { "storage": payload["body"] }
            }))
        };

        let update_handler = |State(s): State<MockState>,
                              Path(id): Path<String>,
                              headers: HeaderMap,
                              body: Bytes| async move {
            s.headers.lock().unwrap().push(headers);
            let payload: Value = serde_json::from_slice(&body).unwrap_or_default();
            let title = payload["title"].as_str().unwrap_or("Untitled");
            let version = payload["version"]["number"].as_u64().unwrap_or(1);
            Json(json!({
                "id": id,
                "title": title,
                "spaceId": payload["spaceId"],
                "version": { "number": version + 1 },
                "body": { "storage": payload["body"] }
            }))
        };

        let app = Router::new()
            .route(
                "/wiki/api/v2/pages/{id}",
                get(get_handler).put(update_handler),
            )
            .route("/wiki/api/v2/pages", post(create_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, captured)
    }

    #[tokio::test]
    async fn mcp_missing_key_returns_401() {
        let state = AppState {
            start: Instant::now(),
            config: test_config("http://localhost:0".into(), false, None),
            jira: None,
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_get_issue",
                json!({"issue_key":"PROJ-123"}),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_wrong_key_returns_401() {
        let state = AppState {
            start: Instant::now(),
            config: test_config("http://localhost:0".into(), false, None),
            jira: None,
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_get_issue",
                json!({"issue_key":"PROJ-123"}),
                Some("wrong-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_disabled_returns_503() {
        let mut config = test_config("http://localhost:0".into(), false, None);
        config.mcp.enabled = false;
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_get_issue",
                json!({"issue_key":"PROJ-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("mcp endpoint disabled"));
    }

    #[tokio::test]
    async fn mcp_global_enable_writes_allows_write_when_agent_unset() {
        let (port, _captured) = mock_jira_server().await;
        let mut config = test_config(
            format!("http://127.0.0.1:{}", port),
            false, // per-agent unset
            Some(temp_audit_path()),
        );
        config.mcp.enable_writes = true;
        config.agents[0].enable_writes = None;

        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_create_issue",
                json!({"project":"PROJ","summary":"stub"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let content = std::fs::read_to_string(&audit_path).unwrap();
        assert!(content.contains("\"result\":\"attempt\""));
        assert!(content.contains("\"result\":\"success\""));
    }

    #[tokio::test]
    async fn mcp_agent_enable_writes_false_overrides_global_true() {
        let (port, _captured) = mock_jira_server().await;
        let mut config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        config.mcp.enable_writes = true;
        config.agents[0].enable_writes = Some(false);

        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_create_issue",
                json!({"project":"PROJ","summary":"stub"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_forbidden_project_returns_403() {
        let (port, _headers) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_get_issue",
                json!({"issue_key":"OTHER-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_allows_valid_request_and_strips_client_headers() {
        let (port, captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_get_issue",
                json!({"issue_key":"PROJ-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["key"], "PROJ-123");

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(!upstream_headers.contains_key("x-atlapool-key"));
        assert!(!upstream_headers.contains_key("cookie"));
    }

    #[tokio::test]
    async fn mcp_read_tool_works_when_writes_disabled() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_get_issue",
                json!({"issue_key":"PROJ-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn mcp_write_tool_denied_when_writes_disabled() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_create_issue",
                json!({"project":"PROJ","summary":"stub"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_write_tool_allowed_and_audited_when_writes_enabled() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_create_issue",
                json!({"project":"PROJ","summary":"stub"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["key"], "PROJ-123");

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));
        assert!(lines[1].contains("\"status\":200"));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_jira_create_issue_upstream_failure_logs_result() {
        // Mock an upstream that rejects the create request.
        let app = Router::new().route(
            "/rest/api/3/issue",
            post(|| async { StatusCode::BAD_REQUEST }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let config = test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_create_issue",
                json!({"project":"PROJ","summary":"stub"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"failure\""));
        assert!(lines[1].contains("\"status\":400"));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_jira_add_comment_allowed_and_strips_headers() {
        let (port, captured) = mock_jira_server().await;
        let config = test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_add_comment",
                json!({
                    "issue_key": "PROJ-123",
                    "body": { "type": "doc", "version": 1, "content": [] }
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["body"]["body"]["type"], "doc");

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(!upstream_headers.contains_key("x-atlapool-key"));
        assert!(!upstream_headers.contains_key("cookie"));

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_jira_add_comment_forbidden_project_returns_403() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_add_comment",
                json!({
                    "issue_key": "OTHER-123",
                    "body": { "type": "doc", "version": 1 }
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_jira_add_comment_write_disabled_returns_403() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_add_comment",
                json!({
                    "issue_key": "PROJ-123",
                    "body": { "type": "doc", "version": 1 }
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_jira_add_comment_rejects_missing_body() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), true, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_add_comment",
                json!({"issue_key": "PROJ-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_jira_get_issue_rejects_path_traversal_in_issue_key() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_get_issue",
                json!({"issue_key": "PROJ-1/../../user/search"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_jira_add_comment_rejects_invalid_issue_key() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), true, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_add_comment",
                json!({
                    "issue_key": "PROJ-not-a-number",
                    "body": { "type": "doc", "version": 1, "content": [] }
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_write_tool_rejected_when_audit_write_fails() {
        let (port, captured) = mock_jira_server().await;
        let unique = format!(
            "{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        );
        let bad_path = std::env::temp_dir()
            .join(format!("nonexistent-dir-{unique}"))
            .join("atlapool-audit.jsonl")
            .to_string_lossy()
            .into_owned();

        let config = test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(bad_path.clone()),
        );
        let audit = Some(AuditLog::new(bad_path));
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_create_issue",
                json!({"project":"PROJ","summary":"stub"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(captured.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mcp_unknown_tool_returns_explicit_error() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_unknown_tool",
                json!({}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_jira_update_issue_summary_only_writes_put_body() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_update_issue",
                json!({"issue_key": "PROJ-123", "summary": "updated summary"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["fields"]["summary"], "updated summary");
        assert!(json["result"]["fields"].get("assignee").is_none());
        assert!(json["result"]["fields"].get("description").is_none());

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_jira_update_issue_assignee_only_writes_put_body() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_update_issue",
                json!({"issue_key": "PROJ-123", "assignee": "account-42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["result"]["fields"]["assignee"]["accountId"],
            "account-42"
        );
        assert!(json["result"]["fields"].get("summary").is_none());
        assert!(json["result"]["fields"].get("description").is_none());

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_jira_update_issue_description_append_merges_adf() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_update_issue",
                json!({"issue_key": "PROJ-123", "description_append": "additional note"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let description = &json["result"]["fields"]["description"];
        assert_eq!(description["type"], "doc");
        let content = description["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        // Original paragraph is preserved.
        assert_eq!(content[0]["content"][0]["text"], "original");
        // New panel appended at the end.
        assert_eq!(content[1]["type"], "panel");
        assert_eq!(content[1]["attrs"]["panelType"], "info");
        assert_eq!(
            content[1]["content"][0]["content"][0]["text"],
            "📋 分析補充"
        );
        assert_eq!(
            content[1]["content"][1]["content"][0]["text"],
            "additional note"
        );

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_jira_update_issue_rejects_empty_update() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), true, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_update_issue",
                json!({"issue_key": "PROJ-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_jira_update_issue_write_disabled_returns_403() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_update_issue",
                json!({"issue_key": "PROJ-123", "summary": "updated"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_jira_update_issue_forbidden_project_returns_403() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), true, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_update_issue",
                json!({"issue_key": "OTHER-123", "summary": "updated"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_jira_get_transitions_allowed_without_writes_enabled() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_get_transitions",
                json!({"issue_key": "PROJ-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let transitions = json["result"]["transitions"].as_array().unwrap();
        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[0]["id"], "1");
        assert_eq!(transitions[0]["name"], "To Do");
    }

    #[tokio::test]
    async fn mcp_jira_transition_issue_allowed_and_audited() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_transition_issue",
                json!({"issue_key": "PROJ-123", "transition_id": "2"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["transition"]["transition"]["id"], "2");

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_jira_transition_issue_rejects_invalid_transition_id() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), true, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_transition_issue",
                json!({"issue_key": "PROJ-123", "transition_id": "abc"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_jira_transition_issue_write_disabled_returns_403() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_transition_issue",
                json!({"issue_key": "PROJ-123", "transition_id": "2"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_jira_transition_issue_forbidden_project_returns_403() {
        let (port, _captured) = mock_jira_server().await;
        let config = test_config(format!("http://127.0.0.1:{}", port), true, None);
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "jira_transition_issue",
                json!({"issue_key": "OTHER-123", "transition_id": "2"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_confluence_get_page_allows_and_strips_headers() {
        let (port, captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            false,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_get_page",
                json!({"space":"SPACE","page_id":"12345"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["title"], "Demo Page");

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(!upstream_headers.contains_key("x-atlapool-key"));
        assert!(!upstream_headers.contains_key("cookie"));
    }

    #[tokio::test]
    async fn mcp_confluence_get_page_forbidden_space_returns_403() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            false,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_get_page",
                json!({"space":"OTHER","page_id":"12345"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_confluence_upstream_not_configured_returns_503() {
        let config = confluence_test_config(
            "http://127.0.0.1:0".into(),
            false,
            None,
            vec!["SPACE".into()],
        );
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_get_page",
                json!({"space":"SPACE","page_id":"12345"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn mcp_confluence_get_page_rejects_invalid_page_id() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            false,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_get_page",
                json!({"space":"SPACE","page_id":"12345/abc"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_confluence_create_page_allowed_and_strips_headers() {
        let (port, captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
            vec!["SPACE".into()],
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_create_page",
                json!({
                    "space": "SPACE",
                    "space_id": "12345",
                    "title": "New Page",
                    "body": "<p>Hello</p>"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["id"], "67890");
        assert_eq!(json["result"]["title"], "New Page");

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(!upstream_headers.contains_key("x-atlapool-key"));
        assert!(!upstream_headers.contains_key("cookie"));

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_confluence_create_page_forbidden_space_returns_403() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_create_page",
                json!({
                    "space": "OTHER",
                    "space_id": "12345",
                    "title": "New Page",
                    "body": "<p>Hello</p>"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_confluence_create_page_write_disabled_returns_403() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            false,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_create_page",
                json!({
                    "space": "SPACE",
                    "space_id": "12345",
                    "title": "New Page",
                    "body": "<p>Hello</p>"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_confluence_create_page_rejects_missing_title() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_create_page",
                json!({
                    "space": "SPACE",
                    "space_id": "12345",
                    "body": "<p>Hello</p>"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_confluence_create_page_rejects_invalid_parent_id() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_create_page",
                json!({
                    "space": "SPACE",
                    "space_id": "12345",
                    "title": "New Page",
                    "body": "<p>Hello</p>",
                    "parent_id": "12345/abc"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_confluence_create_page_includes_parent_id_in_body() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
            vec!["SPACE".into()],
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path));
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_create_page",
                json!({
                    "space": "SPACE",
                    "space_id": "12345",
                    "title": "Child Page",
                    "body": "<p>Hello</p>",
                    "parent_id": "67890"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["id"], "67890");
        assert_eq!(json["result"]["parentId"], "67890");
    }

    #[tokio::test]
    async fn mcp_confluence_update_page_allowed_and_strips_headers() {
        let (port, captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(temp_audit_path()),
            vec!["SPACE".into()],
        );
        let audit_path = config.audit.path.clone().unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_update_page",
                json!({
                    "space": "SPACE",
                    "space_id": "12345",
                    "page_id": "67890",
                    "title": "Updated Page",
                    "version": 2,
                    "body": "<p>Updated</p>"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["id"], "67890");
        assert_eq!(json["result"]["title"], "Updated Page");

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(!upstream_headers.contains_key("x-atlapool-key"));
        assert!(!upstream_headers.contains_key("cookie"));

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_confluence_update_page_forbidden_space_returns_403() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_update_page",
                json!({
                    "space": "OTHER",
                    "space_id": "12345",
                    "page_id": "67890",
                    "title": "Updated Page",
                    "version": 2,
                    "body": "<p>Updated</p>"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_confluence_update_page_write_disabled_returns_403() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            false,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_update_page",
                json!({
                    "space": "SPACE",
                    "space_id": "12345",
                    "page_id": "67890",
                    "title": "Updated Page",
                    "version": 2,
                    "body": "<p>Updated</p>"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_confluence_update_page_rejects_missing_version() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            None,
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: Some(confluence),
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "confluence_update_page",
                json!({
                    "space": "SPACE",
                    "space_id": "12345",
                    "page_id": "67890",
                    "title": "Updated Page",
                    "body": "<p>Updated</p>"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    async fn mock_bitbucket_server() -> (u16, Arc<Mutex<Vec<HeaderMap>>>, Arc<Mutex<Vec<String>>>) {
        #[derive(Clone)]
        struct MockState {
            headers: Arc<Mutex<Vec<HeaderMap>>>,
            bodies: Arc<Mutex<Vec<String>>>,
        }

        let state = MockState {
            headers: Arc::new(Mutex::new(Vec::new())),
            bodies: Arc::new(Mutex::new(Vec::new())),
        };
        let captured_headers = state.headers.clone();
        let captured_bodies = state.bodies.clone();

        let repo_handler = |State(s): State<MockState>,
                            Path((workspace, repo)): Path<(String, String)>,
                            headers: HeaderMap| async move {
            s.headers.lock().unwrap().push(headers);
            Json(json!({
                "full_name": format!("{workspace}/{repo}"),
                "name": repo,
                "workspace": { "slug": workspace },
                "mainbranch": { "name": "main" }
            }))
        };

        let pr_handler = |State(s): State<MockState>,
                          Path((workspace, repo, pr_id)): Path<(String, String, String)>,
                          headers: HeaderMap| async move {
            s.headers.lock().unwrap().push(headers);
            Json(json!({
                "id": pr_id,
                "title": "PR title",
                "source": { "repository": { "full_name": format!("{workspace}/{repo}") } }
            }))
        };

        let create_repo_handler = |State(s): State<MockState>,
                                   Path((workspace, repo)): Path<(String, String)>,
                                   headers: HeaderMap,
                                   body: String| async move {
            s.headers.lock().unwrap().push(headers);
            s.bodies.lock().unwrap().push(body);
            Json(json!({
                "full_name": format!("{workspace}/{repo}"),
                "name": repo,
                "is_private": true
            }))
        };

        let create_branch_handler = |State(s): State<MockState>,
                                     Path((workspace, repo)): Path<(String, String)>,
                                     headers: HeaderMap,
                                     body: String| async move {
            s.headers.lock().unwrap().push(headers);
            s.bodies.lock().unwrap().push(body);
            Json(json!({
                "name": "created-branch",
                "target": { "hash": "abc" },
                "repository": { "full_name": format!("{workspace}/{repo}") }
            }))
        };

        let list_branches_handler = |State(s): State<MockState>,
                                     Path((_workspace, _repo)): Path<(String, String)>,
                                     headers: HeaderMap| async move {
            s.headers.lock().unwrap().push(headers);
            Json(json!({
                "values": [
                    {
                        "name": "main",
                        "target": { "hash": "abc123" }
                    },
                    {
                        "name": "feature/x",
                        "target": { "hash": "def456" }
                    }
                ]
            }))
        };

        let src_handler = |State(s): State<MockState>,
                           Path((workspace, repo, tail)): Path<(String, String, String)>,
                           headers: HeaderMap| async move {
            s.headers.lock().unwrap().push(headers);
            let (ref_name, sub_path) = tail.split_once('/').unwrap_or((&tail, ""));
            if sub_path.is_empty() || sub_path.ends_with('/') {
                let trimmed = sub_path.trim_end_matches('/');
                Json(json!({
                    "values": [
                        { "type": "commit_file", "path": format!("{}/README.md", trimmed).trim_start_matches('/') },
                        { "type": "commit_directory", "path": format!("{}/src", trimmed).trim_start_matches('/') }
                    ],
                    "size": 2,
                    "page": 1,
                    "pagelen": 10
                }))
            } else {
                Json(json!(format!(
                    "content of {}/{} in {}/{}",
                    ref_name, sub_path, workspace, repo
                )))
            }
        };

        let create_pr_handler = |State(s): State<MockState>,
                                 Path((workspace, repo)): Path<(String, String)>,
                                 headers: HeaderMap,
                                 body: String| async move {
            s.headers.lock().unwrap().push(headers);
            s.bodies.lock().unwrap().push(body);
            Json(json!({
                "id": "99",
                "title": "created PR",
                "source": { "repository": { "full_name": format!("{workspace}/{repo}") } }
            }))
        };

        let create_commit_handler = |State(s): State<MockState>,
                                     Path((workspace, repo)): Path<(String, String)>,
                                     headers: HeaderMap,
                                     body: String| async move {
            s.headers.lock().unwrap().push(headers);
            s.bodies.lock().unwrap().push(body);
            Json(json!({
                "hash": "commit-hash",
                "repository": { "full_name": format!("{workspace}/{repo}") }
            }))
        };

        let app = Router::new()
            .route(
                "/repositories/{workspace}/{repo}",
                get(repo_handler).post(create_repo_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/pullrequests/{pr_id}",
                get(pr_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/refs/branches",
                get(list_branches_handler).post(create_branch_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/pullrequests",
                post(create_pr_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/src",
                post(create_commit_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/src/{*tail}",
                get(src_handler),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // Give the server a moment to start listening.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        (port, captured_headers, captured_bodies)
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_repo_allowed_and_strips_headers() {
        let (port, captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_repo",
                json!({"repo_slug": "my-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["full_name"], "WORK/my-repo");

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(!upstream_headers.contains_key("x-atlapool-key"));
        assert!(!upstream_headers.contains_key("cookie"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pull_request_allowed() {
        let (port, captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_pull_request",
                json!({"repo_slug": "my-repo", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["id"], "42");

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_repo_forbidden_repo_returns_403() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_repo",
                json!({"repo_slug": "other-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_bitbucket_upstream_not_configured_returns_503() {
        let config = bitbucket_test_config(
            "http://127.0.0.1:0".into(),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_repo",
                json!({"repo_slug": "my-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_repo_rejects_invalid_pull_request_id() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_pull_request",
                json!({"repo_slug": "my-repo", "pull_request_id": "42/abc"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_repo_rejects_path_traversal_in_repo_slug() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["*".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_repo",
                json!({"repo_slug": "../../other-workspace/secret-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pull_request_rejects_path_traversal_in_repo_slug() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["*".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_pull_request",
                json!({"repo_slug": "my-repo/../other", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    fn bitbucket_write_test_config(
        base_url: String,
        tools: Vec<String>,
        enable_writes: bool,
    ) -> (Config, String) {
        let audit_path = temp_audit_path();
        let mut config = bitbucket_test_config(
            base_url,
            Some(audit_path.clone()),
            vec!["WORK".into()],
            vec!["*".into()],
        );
        config.agents[0].enable_writes = Some(enable_writes);
        config.agents[0].tools = tools;
        (config, audit_path)
    }

    #[tokio::test]
    async fn mcp_bitbucket_create_repo_defaults_to_private_and_writes_audit() {
        let (port, _captured, bodies) = mock_bitbucket_server().await;
        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_create_repo".into()],
            true,
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: Some(AuditLog::new(audit_path.clone())),
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_create_repo",
                json!({"repo_slug": "new-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = bodies.lock().unwrap().pop().unwrap();
        let upstream: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(upstream["is_private"], true);

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_bitbucket_create_repo_write_disabled_returns_403() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_create_repo".into()],
            false,
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_create_repo",
                json!({"repo_slug": "new-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mcp_bitbucket_create_repo_audit_failure_rejected() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_create_repo".into()],
            true,
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: Some(AuditLog::new("/nonexistent-dir/atlapool-audit.jsonl")),
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_create_repo",
                json!({"repo_slug": "new-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn mcp_bitbucket_create_branch_allowed() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_create_branch".into()],
            true,
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: Some(AuditLog::new(audit_path.clone())),
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_create_branch",
                json!({"repo_slug": "my-repo", "branch_name": "feature/x", "target_hash": "abc123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_bitbucket_create_commit_allowed_with_form_body() {
        let (port, _captured, bodies) = mock_bitbucket_server().await;
        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_create_commit".into()],
            true,
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: Some(AuditLog::new(audit_path.clone())),
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_create_commit",
                json!({
                    "repo_slug": "my-repo",
                    "message": "commit msg",
                    "branch": "main",
                    "files": { "README.md": "hello" }
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = bodies.lock().unwrap().pop().unwrap();
        assert!(body.contains("message=commit+msg"));
        assert!(body.contains("branch=main"));
        assert!(body.contains("README.md=hello"));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_bitbucket_create_pull_request_allowed() {
        let (port, _captured, bodies) = mock_bitbucket_server().await;
        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_create_pull_request".into()],
            true,
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: Some(AuditLog::new(audit_path.clone())),
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_create_pull_request",
                json!({
                    "repo_slug": "my-repo",
                    "title": "PR title",
                    "source_branch": "feature/x",
                    "destination_branch": "main",
                    "description": "desc"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = bodies.lock().unwrap().pop().unwrap();
        let upstream: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(upstream["title"], "PR title");

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_branches_allowed_and_strips_headers() {
        let (port, captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_list_branches",
                json!({"repo_slug": "my-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let values = json["result"]["values"].as_array().unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["name"], "main");
        assert_eq!(values[0]["target"]["hash"], "abc123");

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(!upstream_headers.contains_key("x-atlapool-key"));
        assert!(!upstream_headers.contains_key("cookie"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_directory_with_ref() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_list_directory",
                json!({"repo_slug": "my-repo", "ref": "main"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let values = json["result"]["values"].as_array().unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["path"], "README.md");
        assert_eq!(values[1]["path"], "src");
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_directory_resolves_default_branch() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_list_directory",
                json!({"repo_slug": "my-repo", "path": "src"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let values = json["result"]["values"].as_array().unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["path"], "src/README.md");
        assert_eq!(values[1]["path"], "src/src");
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_file_content_with_ref() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_file_content",
                json!({"repo_slug": "my-repo", "ref": "main", "path": "README.md"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["result"]
            .as_str()
            .unwrap()
            .contains("content of main/README.md"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_file_content_resolves_default_branch() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["my-repo".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_file_content",
                json!({"repo_slug": "my-repo", "path": "README.md"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["result"]
            .as_str()
            .unwrap()
            .contains("content of main/README.md"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_directory_rejects_path_traversal() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["*".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_list_directory",
                json!({"repo_slug": "my-repo", "path": "../secret"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_file_content_rejects_path_traversal() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["*".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_get_file_content",
                json!({"repo_slug": "my-repo", "path": "foo/../secret.txt"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_directory_rejects_ref_with_slash() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["*".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_list_directory",
                json!({"repo_slug": "my-repo", "ref": "feature/x"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_directory_rejects_ref_dotdot() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["*".into()],
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_list_directory",
                json!({"repo_slug": "my-repo", "ref": ".."}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_create_commit_handles_204_empty_response() {
        let app = Router::new().route(
            "/repositories/{workspace}/{repo}/src",
            post(|| async { StatusCode::NO_CONTENT }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_create_commit".into()],
            true,
        );
        let bitbucket = BitbucketClient::new(config.bitbucket.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: Some(bitbucket),
            audit: Some(AuditLog::new(audit_path.clone())),
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_request(
                "bitbucket_create_commit",
                json!({
                    "repo_slug": "my-repo",
                    "message": "commit msg",
                    "branch": "main",
                    "files": { "README.md": "hello" }
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"], json!({}));

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));
        assert!(lines[1].contains("\"status\":204"));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_initialize_succeeds_without_key() {
        let config = test_config("http://127.0.0.1:0".into(), false, None);
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_jsonrpc_request(
                "initialize",
                json!({ "protocolVersion": "2024-11-05" }),
                None,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(json["result"]["serverInfo"]["name"], "atlapool");
        assert!(!json["result"]["serverInfo"]["version"]
            .as_str()
            .unwrap()
            .is_empty());
        assert!(json["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn mcp_tools_list_requires_key() {
        let config = test_config("http://127.0.0.1:0".into(), false, None);
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_jsonrpc_request("tools/list", json!({}), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_tools_list_returns_allowed_tools_with_schemas() {
        let config = test_config("http://127.0.0.1:0".into(), false, None);
        let state = AppState {
            start: Instant::now(),
            config,
            jira: None,
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);
        let response = app
            .oneshot(build_jsonrpc_request(
                "tools/list",
                json!({}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let tools = json["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

        assert!(names.contains(&"jira_get_issue"));
        assert!(names.contains(&"jira_transition_issue"));
        assert!(!names.contains(&"confluence_get_page"));
        assert!(!names.contains(&"bitbucket_get_repo"));

        for tool in tools {
            assert!(tool["inputSchema"]["type"].as_str() == Some("object"));
            assert!(tool["description"].as_str().is_some());
        }
    }

    #[tokio::test]
    async fn mcp_full_flow_initialize_tools_list_call() {
        let (port, _captured) = mock_jira_server().await;
        let mut config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        // Keep only one read tool for the flow test.
        config.agents[0].tools = vec!["jira_get_issue".into()];
        let jira = JiraClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let state = AppState {
            start: Instant::now(),
            config,
            jira: Some(jira),
            confluence: None,
            bitbucket: None,
            audit: None,
        };
        let app = crate::router(state);

        // initialize
        let response = app
            .clone()
            .oneshot(build_jsonrpc_request(
                "initialize",
                json!({ "protocolVersion": "2024-11-05" }),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let init: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2024-11-05");

        // tools/list
        let response = app
            .clone()
            .oneshot(build_jsonrpc_request(
                "tools/list",
                json!({}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let list: Value = serde_json::from_slice(&bytes).unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "jira_get_issue");

        // tools/call
        let response = app
            .oneshot(build_request(
                "jira_get_issue",
                json!({"issue_key": "PROJ-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let call: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(call["result"]["key"], "PROJ-123");
    }
}
