use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use regex::Regex;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::OnceLock;

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
    /// Optional `max_lines` for `bitbucket_get_pull_request_diff` truncation.
    max_lines: Option<usize>,
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

    if request.method == "initialize" {
        return initialize_handler(request.id).await;
    }

    if request.method.starts_with("notifications/") {
        return (StatusCode::ACCEPTED, Json(json!({})));
    }

    if request.method != "tools/list" && request.method != "tools/call" {
        return mcp_error(
            request.id,
            StatusCode::NOT_IMPLEMENTED,
            "method not supported",
        );
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
        return mcp_policy_error(request.id, "not permitted by agent policy");
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
        return mcp_policy_error(request.id, "write tools not enabled for agent");
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

        // confluence_get_page by title needs a title-to-page-id lookup before
        // fetching the full page body.
        if params.name == "confluence_get_page" && target.path.starts_with("/wiki/api/v2/spaces/") {
            match resolve_confluence_page_by_title(confluence, &target.path).await {
                Ok(ConfluenceTitleResult::Found(page_id)) => {
                    target.path = format!("/wiki/api/v2/pages/{page_id}?body-format=view");
                }
                Ok(ConfluenceTitleResult::NotFound(message))
                | Ok(ConfluenceTitleResult::Multiple(message)) => {
                    return mcp_policy_error(request.id, &message);
                }
                Err((status, message)) => return mcp_error(request.id, status, &message),
            }
        }

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

    // Raw diff is plain text (possibly redirecting to a CDN). Read bytes so we
    // can detect binary content and truncate/redact before wrapping.
    if tool == "bitbucket_get_pull_request_diff" {
        if !upstream_status.is_success() {
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
            return Err((StatusCode::BAD_GATEWAY, message));
        }

        let bytes = match upstream_resp.bytes().await {
            Ok(b) => b,
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

        let max_lines = target.max_lines.unwrap_or(2000);
        let upstream_body = process_diff_body(bytes, max_lines);
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
        let result = wrap_calltool_result(&upstream_body, false);
        return Ok((
            StatusCode::OK,
            Json(json!({"jsonrpc":"2.0","id": request_id, "result": result})),
        ));
    }

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
    let mut upstream_body: Value = if upstream_text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&upstream_text).unwrap_or_else(|_| json!(upstream_text))
    };

    // Add a pagination hint for list-style upstream responses.
    if let Some(obj) = upstream_body.as_object_mut() {
        if obj.get("values").is_some() || obj.get("results").is_some() {
            let has_more = obj.get("next").is_some();
            obj.insert("has_more".to_string(), json!(has_more));
        }
    }

    // For pipeline status, a 404 or empty result means pipelines are not configured
    // or have never run for the branch; normalize to a non-error status instead of
    // returning an error envelope.
    let (treat_as_success, upstream_body) = if tool == "bitbucket_get_pipeline_status" {
        let is_missing = upstream_status == StatusCode::NOT_FOUND
            || upstream_body
                .get("values")
                .and_then(|v| v.as_array())
                .map(|v| v.is_empty())
                .unwrap_or(true);
        let success = upstream_status.is_success() || upstream_status == StatusCode::NOT_FOUND;
        let body = if is_missing && success {
            normalize_pipeline_status(json!({
                "values": [],
                "message": "Pipelines are not configured or have not run for the requested branch"
            }))
        } else if upstream_status.is_success() {
            normalize_pipeline_status(upstream_body)
        } else {
            upstream_body
        };
        (success, body)
    } else {
        (upstream_status.is_success(), upstream_body)
    };

    let (status, envelope) = if treat_as_success {
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
        let result = wrap_calltool_result(&upstream_body, false);
        (
            StatusCode::OK,
            json!({"jsonrpc":"2.0","id": request_id, "result": result}),
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
        let result = wrap_calltool_result(&upstream_body, true);
        (
            StatusCode::OK,
            json!({"jsonrpc":"2.0","id": request_id, "result": result}),
        )
    };

    Ok((status, Json(envelope)))
}

/// Normalize a Bitbucket Pipelines response into a single `normalized_status`.
///
/// Bitbucket uses two fields: `state` (PENDING/IN_PROGRESS/RUNNING/PAUSED/COMPLETED)
/// and `result` (SUCCESSFUL/FAILED/ERROR/STOPPED/EXPIRED) when completed.
/// This function maps those to `passed`/`failed`/`running`/`unknown`.
fn normalize_pipeline_status(mut upstream_body: Value) -> Value {
    let values = upstream_body
        .get("values")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let (normalized, explanation, pipeline) = if let Some(first) = values.first() {
        let state = first
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        let result = first
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_uppercase();

        let normalized = match state.as_str() {
            "PENDING" | "IN_PROGRESS" | "RUNNING" | "PAUSED" => "running",
            "COMPLETED" => match result.as_str() {
                "SUCCESSFUL" => "passed",
                "FAILED" | "ERROR" | "STOPPED" | "EXPIRED" => "failed",
                _ => "unknown",
            },
            _ => "unknown",
        };

        let explanation = format!("pipeline state is {state}, result is {result}");
        (normalized, explanation, first.clone())
    } else {
        let explanation = upstream_body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("no pipeline runs found for the requested branch")
            .to_string();
        ("unknown", explanation, Value::Null)
    };

    if let Some(obj) = upstream_body.as_object_mut() {
        obj.insert("normalized_status".to_string(), json!(normalized));
        obj.insert("status_message".to_string(), json!(explanation));
        if pipeline.is_null() {
            obj.insert("pipeline".to_string(), json!(null));
        } else {
            obj.insert("pipeline".to_string(), pipeline);
        }
    }
    upstream_body
}

fn secret_patterns() -> &'static [Regex; 3] {
    static PATTERNS: OnceLock<[Regex; 3]> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
            Regex::new(r"sk-[a-zA-Z0-9]{20,}").unwrap(),
            Regex::new(r"Bearer [a-zA-Z0-9._-]{20,}").unwrap(),
        ]
    })
}

fn redact_secrets(text: &str) -> String {
    let mut redacted = text.to_string();
    for pattern in secret_patterns() {
        redacted = pattern.replace_all(&redacted, "[REDACTED]").into_owned();
    }
    redacted
}

fn process_diff_body(bytes: Bytes, max_lines: usize) -> Value {
    let text = match String::from_utf8(bytes.to_vec()) {
        Ok(t) => t,
        Err(_) => {
            return json!({
                "diff": "[binary file, diff not shown]",
                "binary": true
            });
        }
    };

    let redacted = redact_secrets(&text);
    let lines: Vec<&str> = redacted.lines().collect();
    let total_lines = lines.len();
    let (diff, truncated) = if total_lines > max_lines {
        let mut truncated_lines: Vec<&str> = lines.into_iter().take(max_lines).collect();
        truncated_lines.push("... (truncated; use a full git client for the complete diff)");
        (truncated_lines.join("\n"), true)
    } else {
        (redacted, false)
    };

    let mut result = json!({
        "diff": diff,
        "total_lines": total_lines,
    });
    if truncated {
        result["truncated"] = json!(true);
        result["message"] = json!("diff exceeded max_lines and was truncated");
    }
    result
}

fn jql_project_override_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(project|projectkey)\b").unwrap())
}

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

fn wrap_calltool_result(upstream_body: &Value, is_error: bool) -> Value {
    let mut result = json!({
        "content": [{"type": "text", "text": ""}],
        "isError": is_error
    });
    match upstream_body {
        Value::String(text) => {
            result["content"][0]["text"] = json!(text);
            result
        }
        _ => {
            result["content"][0]["text"] = json!(upstream_body.to_string());
            result["structuredContent"] = upstream_body.clone();
            result
        }
    }
}

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

fn mcp_policy_error(id: Option<Value>, message: &str) -> (StatusCode, Json<Value>) {
    let result = json!({
        "content": [{"type": "text", "text": message}],
        "isError": true,
        "structuredContent": { "message": message }
    });
    mcp_success(id, result)
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
            name: "jira_search_issues",
            description: "Search Jira issues within a project using JQL.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Jira project key (required, must be in allowlist)" },
                    "jql_filter": { "type": "string", "description": "Optional additional JQL condition; cannot contain project/projectKey tokens" },
                    "max_results": { "type": "number", "description": "Maximum results (default 50, capped at 100)" }
                },
                "required": ["project"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "jira_list_comments",
            description: "List comments on a Jira issue.",
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
            name: "confluence_get_page",
            description: "Fetch a Confluence page by ID or by space + title.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "space": { "type": "string", "description": "Space key for allowlist" },
                    "page_id": { "type": "string", "description": "Numeric page ID (use either page_id or title)" },
                    "space_id": { "type": "string", "description": "Numeric space ID (required with title)" },
                    "title": { "type": "string", "description": "Page title to look up in the space (use with space_id)" }
                },
                "required": ["space"],
                "minProperties": 2,
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
            name: "confluence_list_pages",
            description: "List pages in a Confluence space.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "space": { "type": "string", "description": "Space key for allowlist" },
                    "space_id": { "type": "string", "description": "Numeric space ID" }
                },
                "required": ["space", "space_id"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "confluence_delete_page",
            description: "Delete a Confluence page.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "space": { "type": "string", "description": "Space key for allowlist" },
                    "page_id": { "type": "string", "description": "Numeric page ID" }
                },
                "required": ["space", "page_id"],
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
            name: "bitbucket_list_pull_requests",
            description: "List pull requests in a Bitbucket repository.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "state": { "type": "string", "description": "OPEN, MERGED, DECLINED, or SUPERSEDED (default OPEN)" }
                },
                "required": ["repo_slug"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_list_pull_request_changes",
            description: "List changed files and line statistics for a pull request (diffstat, not raw diff).",
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
            name: "bitbucket_get_pipeline_status",
            description: "Get the latest Bitbucket Pipelines status for a branch.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "branch": { "type": "string", "description": "Branch name" }
                },
                "required": ["repo_slug", "branch"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_get_pull_request_diff",
            description: "Get the raw diff of a Bitbucket pull request with secret redaction and optional line truncation.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "pull_request_id": { "type": "string", "description": "Numeric pull request ID" },
                    "max_lines": { "type": "number", "description": "Maximum diff lines to return (default 2000)" }
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
        Tool {
            name: "bitbucket_merge_pull_request",
            description: "Merge a Bitbucket pull request using the merge_commit strategy.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "pull_request_id": { "type": "string", "description": "Numeric pull request ID" },
                    "close_source_branch": { "type": "boolean", "description": "Close source branch after merge (default true)" }
                },
                "required": ["repo_slug", "pull_request_id"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_decline_pull_request",
            description: "Decline a Bitbucket pull request without merging.",
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
            name: "bitbucket_delete_branch",
            description: "Delete a branch in a Bitbucket repository.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "branch_name": { "type": "string", "description": "Branch name; may contain '/' (e.g. 'feature/x')" }
                },
                "required": ["repo_slug", "branch_name"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "bitbucket_add_pull_request_comment",
            description: "Add a comment to a Bitbucket pull request.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_slug": { "type": "string", "description": "Repository slug" },
                    "pull_request_id": { "type": "string", "description": "Numeric pull request ID" },
                    "content": { "type": "string", "description": "Comment content in raw markdown" }
                },
                "required": ["repo_slug", "pull_request_id", "content"],
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
    let valid_branch_name = |s: &str| -> Result<(), String> {
        if s.is_empty() {
            return Err("branch_name must not be empty".into());
        }
        if s == "." || s == ".." {
            return Err("branch_name must not be '.' or '..'".into());
        }
        if s.starts_with('/') {
            return Err("branch_name must not start with '/'".into());
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
                max_lines: None,
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
                max_lines: None,
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
                max_lines: None,
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

            let page_id = args.get("page_id").and_then(|v| v.as_str());
            let title = args.get("title").and_then(|v| v.as_str());
            let space_id = args.get("space_id").and_then(|v| v.as_str());

            if let Some(page_id) = page_id {
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
                    max_lines: None,
                })
            } else if let Some(title) = title {
                let space_id = space_id.ok_or("missing space_id for title lookup")?;
                if space_id.is_empty() || !space_id.chars().all(|c| c.is_ascii_digit()) {
                    return Err("space_id must be a non-empty numeric id".into());
                }
                if title.is_empty() {
                    return Err("title must not be empty".into());
                }
                let encoded_title = urlencoding::encode(title);
                Ok(ToolTarget {
                    workspace: None,
                    repo: None,
                    project: None,
                    space: Some(space),
                    method: Method::GET,
                    path: format!("/wiki/api/v2/spaces/{space_id}/pages?title={encoded_title}"),
                    body: RequestBody::None,
                    update: None,
                    resolve_ref: None,
                    max_lines: None,
                })
            } else {
                Err("either page_id or title (with space_id) must be provided".into())
            }
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
                max_lines: None,
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
                max_lines: None,
            })
        }
        "confluence_list_pages" => {
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
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project: None,
                space: Some(space),
                method: Method::GET,
                path: format!("/wiki/api/v2/spaces/{space_id}/pages"),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
                max_lines: None,
            })
        }
        "confluence_delete_page" => {
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
                method: Method::DELETE,
                path: format!("/wiki/api/v2/pages/{page_id}"),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
                max_lines: None,
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
                max_lines: None,
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
                    max_lines: None,
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
                    max_lines: None,
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
                    max_lines: None,
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
                    max_lines: None,
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
                max_lines: None,
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
                max_lines: None,
            })
        }
        "bitbucket_list_pull_requests" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let state_str = args.get("state").and_then(|v| v.as_str()).unwrap_or("OPEN");
            if state_str.is_empty() {
                return Err("state must not be empty".into());
            }
            const VALID_STATES: &[&str] = &["OPEN", "MERGED", "DECLINED", "SUPERSEDED"];
            if !VALID_STATES.contains(&state_str) {
                return Err("state must be OPEN, MERGED, DECLINED, or SUPERSEDED".into());
            }
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::GET,
                path: format!(
                    "/repositories/{workspace}/{repo_slug}/pullrequests?state={state_str}"
                ),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
                max_lines: None,
            })
        }
        "bitbucket_list_pull_request_changes" => {
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
                    "/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/diffstat"
                ),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
                max_lines: None,
            })
        }
        "bitbucket_get_pipeline_status" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let branch = args
                .get("branch")
                .and_then(|v| v.as_str())
                .ok_or("missing branch")?;
            valid_branch_name(branch)?;
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            let encoded_branch = encode_path_segment(branch);
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::GET,
                path: format!("/repositories/{workspace}/{repo_slug}/pipelines/?sort=-created_on&target.branch={encoded_branch}&pagelen=1"),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
                max_lines: None,
            })
        }
        "bitbucket_get_pull_request_diff" => {
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
            let max_lines = args
                .get("max_lines")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(2000);
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::GET,
                path: format!(
                    "/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/diff"
                ),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
                max_lines: Some(max_lines),
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
                max_lines: None,
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
                max_lines: None,
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
                max_lines: None,
            })
        }
        "bitbucket_merge_pull_request" => {
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
            let close_source_branch = args
                .get("close_source_branch")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            let body = json!({
                "merge_strategy": "merge_commit",
                "close_source_branch": close_source_branch
            });
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::POST,
                path: format!(
                    "/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/merge"
                ),
                body: RequestBody::json(body),
                update: None,
                resolve_ref: None,
                max_lines: None,
            })
        }
        "bitbucket_decline_pull_request" => {
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
                method: Method::POST,
                path: format!(
                    "/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/decline"
                ),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
                max_lines: None,
            })
        }
        "bitbucket_delete_branch" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            valid_repo_slug(repo_slug)?;
            let branch_name = args
                .get("branch_name")
                .and_then(|v| v.as_str())
                .ok_or("missing branch_name")?;
            valid_branch_name(branch_name)?;
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            let encoded_branch = encode_path_segment(branch_name);
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::DELETE,
                path: format!(
                    "/repositories/{workspace}/{repo_slug}/refs/branches/{encoded_branch}"
                ),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
                max_lines: None,
            })
        }
        "bitbucket_add_pull_request_comment" => {
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
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or("missing content")?;
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            let body = json!({
                "content": { "raw": content }
            });
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::POST,
                path: format!(
                    "/repositories/{workspace}/{repo_slug}/pullrequests/{pull_request_id}/comments"
                ),
                body: RequestBody::json(body),
                update: None,
                resolve_ref: None,
                max_lines: None,
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
                max_lines: None,
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
                max_lines: None,
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
                max_lines: None,
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
                max_lines: None,
            })
        }
        "jira_search_issues" => {
            let args = args.ok_or("missing arguments")?;
            let project = args
                .get("project")
                .and_then(|v| v.as_str())
                .ok_or("missing project")?;
            if project.is_empty() {
                return Err("project must not be empty".into());
            }

            let jql_filter = args.get("jql_filter").and_then(|v| v.as_str());
            if let Some(filter) = jql_filter {
                if jql_project_override_regex().is_match(filter) {
                    return Err("jql_filter must not contain project or projectKey tokens".into());
                }
            }

            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(50)
                .min(100);

            let jql = if let Some(filter) = jql_filter {
                if filter.trim().is_empty() {
                    format!("project = \"{project}\"")
                } else {
                    format!("project = \"{project}\" AND ({filter})")
                }
            } else {
                format!("project = \"{project}\"")
            };

            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project: Some(project.to_string()),
                space: None,
                method: Method::POST,
                path: "/rest/api/3/search".into(),
                body: RequestBody::json(json!({
                    "jql": jql,
                    "maxResults": max_results,
                })),
                update: None,
                resolve_ref: None,
                max_lines: None,
            })
        }
        "jira_list_comments" => {
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
                path: format!("/rest/api/3/issue/{issue_key}/comment"),
                body: RequestBody::None,
                update: None,
                resolve_ref: None,
                max_lines: None,
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

enum ConfluenceTitleResult {
    Found(String),
    NotFound(String),
    Multiple(String),
}

/// Resolve a Confluence page title to a single page ID.
///
/// `list_path` must be `/wiki/api/v2/spaces/{space_id}/pages?title={encoded_title}`.
async fn resolve_confluence_page_by_title<C: UpstreamClient>(
    client: &C,
    list_path: &str,
) -> Result<ConfluenceTitleResult, (StatusCode, String)> {
    let request = client
        .build_request(Method::GET, list_path, RequestBody::None)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let response = client.execute(request).await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("upstream GET failed: {}", e),
        )
    })?;
    if !response.status().is_success() {
        return Err((
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!("upstream GET returned {}", response.status()),
        ));
    }
    let body: Value = response.json().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to parse upstream GET response: {}", e),
        )
    })?;
    let results = body
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                "upstream response did not contain a results array".into(),
            )
        })?;
    match results.len() {
        0 => Ok(ConfluenceTitleResult::NotFound(
            "no Confluence page found with the given title".into(),
        )),
        1 => {
            let page_id = results[0]
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    (
                        StatusCode::BAD_GATEWAY,
                        "upstream page result missing id".into(),
                    )
                })?;
            Ok(ConfluenceTitleResult::Found(page_id.to_string()))
        }
        _ => Ok(ConfluenceTitleResult::Multiple(
            "multiple Confluence pages found with the given title; please use page_id".into(),
        )),
    }
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
    use axum::extract::{Path, Query};
    use axum::http::{Request, StatusCode};
    use axum::routing::{delete, get, post};
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
                    "jira_search_issues".into(),
                    "jira_list_comments".into(),
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
                    "confluence_list_pages".into(),
                    "confluence_delete_page".into(),
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
                    "bitbucket_list_pull_requests".into(),
                    "bitbucket_list_pull_request_changes".into(),
                    "bitbucket_get_pipeline_status".into(),
                    "bitbucket_get_pull_request_diff".into(),
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
        build_jsonrpc_request_with_headers(method, params, key, &[])
    }

    fn build_jsonrpc_request_with_headers(
        method: &str,
        params: Value,
        key: Option<&str>,
        extra_headers: &[(&str, &str)],
    ) -> Request<Body> {
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

        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
        }

        if let Some(k) = key {
            builder = builder.header("X-Atlapool-Key", k);
        }
        // A sensitive header that must never reach the upstream Jira server.
        builder = builder.header("cookie", "session=bad");
        builder.body(Body::from(body)).unwrap()
    }

    fn build_request_with_protocol_version(
        tool: &str,
        args: Value,
        key: Option<&str>,
    ) -> Request<Body> {
        build_jsonrpc_request_with_headers(
            "tools/call",
            json!({
                "name": tool,
                "arguments": args
            }),
            key,
            &[("mcp-protocol-version", "2024-11-05")],
        )
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

        let search_handler = |State(s): State<MockState>, headers: HeaderMap, body: Bytes| async move {
            s.headers.lock().unwrap().push(headers);
            let payload: Value = serde_json::from_slice(&body).unwrap_or_default();
            Json(json!({
                "expand": "names,schema",
                "startAt": 0,
                "maxResults": payload["maxResults"],
                "total": 1,
                "issues": [
                    {
                        "id": "10000",
                        "key": "PROJ-123",
                        "fields": { "summary": "Test issue" }
                    }
                ],
                "jql": payload["jql"]
            }))
        };

        let list_comments_handler =
            |State(s): State<MockState>, Path(_): Path<String>, headers: HeaderMap| async move {
                s.headers.lock().unwrap().push(headers);
                Json(json!({
                    "startAt": 0,
                    "maxResults": 10,
                    "total": 1,
                    "comments": [
                        {
                            "id": "10001",
                            "author": { "displayName": "Alice" },
                            "body": { "type": "doc", "version": 1, "content": [] },
                            "created": "2026-01-01T00:00:00.000+0000"
                        }
                    ]
                }))
            };

        let app = Router::new()
            .route(
                "/rest/api/3/issue/{key}",
                get(get_handler).put(update_handler),
            )
            .route("/rest/api/3/issue", post(post_handler))
            .route(
                "/rest/api/3/issue/{key}/comment",
                get(list_comments_handler).post(comment_handler),
            )
            .route(
                "/rest/api/3/issue/{key}/transitions",
                get(get_transitions_handler).post(do_transition_handler),
            )
            .route("/rest/api/3/search", post(search_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, captured)
    }

    async fn mock_jira_server_404() -> u16 {
        let get_handler = |Path(_): Path<String>, headers: HeaderMap| async move {
            let _ = headers; // silence unused variable warning in tests
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "errorMessages": ["Issue does not exist"],
                    "errors": {}
                })),
            )
        };

        let app = Router::new().route("/rest/api/3/issue/{key}", get(get_handler));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        port
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

        let list_handler = |State(s): State<MockState>,
                            Path(space_id): Path<String>,
                            Query(query): Query<std::collections::HashMap<String, String>>,
                            headers: HeaderMap| async move {
            s.headers.lock().unwrap().push(headers);
            let title = query.get("title").cloned().unwrap_or_default();
            let response = if title == "Unique Page" {
                json!({
                    "results": [
                        { "id": "456", "title": "Unique Page", "spaceId": space_id }
                    ],
                    "_links": { "base": "https://example.atlassian.net/wiki" }
                })
            } else if title == "No Such Page" {
                json!({
                    "results": [],
                    "_links": { "base": "https://example.atlassian.net/wiki" }
                })
            } else if title == "Ambiguous" {
                json!({
                    "results": [
                        { "id": "456", "title": "Ambiguous", "spaceId": space_id },
                        { "id": "457", "title": "Ambiguous", "spaceId": space_id }
                    ],
                    "_links": { "base": "https://example.atlassian.net/wiki" }
                })
            } else {
                let has_next = space_id == "999";
                let mut r = json!({
                    "results": [
                        { "id": "123", "title": "Page 1", "spaceId": space_id },
                        { "id": "124", "title": "Page 2", "spaceId": space_id }
                    ],
                    "_links": { "base": "https://example.atlassian.net/wiki" }
                });
                if has_next {
                    r["next"] = json!("/wiki/api/v2/spaces/999/pages?cursor=abc");
                }
                r
            };
            Json(response)
        };

        let delete_handler =
            |State(s): State<MockState>, Path(_id): Path<String>, headers: HeaderMap| async move {
                s.headers.lock().unwrap().push(headers);
                StatusCode::NO_CONTENT
            };

        let app = Router::new()
            .route(
                "/wiki/api/v2/pages/{id}",
                get(get_handler).put(update_handler).delete(delete_handler),
            )
            .route("/wiki/api/v2/pages", post(create_handler))
            .route("/wiki/api/v2/spaces/{space_id}/pages", get(list_handler))
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(json["result"]["structuredContent"]["key"], "PROJ-123");

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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(json["result"]["structuredContent"]["key"], "PROJ-123");

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

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);

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
        assert_eq!(
            json["result"]["structuredContent"]["body"]["body"]["type"],
            "doc"
        );

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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
    async fn mcp_jira_get_issue_with_protocol_version_upstream_404_returns_calltool_error() {
        let port = mock_jira_server_404().await;
        let mut config = test_config(format!("http://127.0.0.1:{}", port), false, None);
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
        let response = app
            .oneshot(build_request_with_protocol_version(
                "jira_get_issue",
                json!({"issue_key": "PROJ-999"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        assert_eq!(
            json["result"]["structuredContent"]["errorMessages"][0],
            "Issue does not exist"
        );
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
        assert_eq!(
            json["result"]["structuredContent"]["fields"]["summary"],
            "updated summary"
        );
        assert!(json["result"]["structuredContent"]["fields"]
            .get("assignee")
            .is_none());
        assert!(json["result"]["structuredContent"]["fields"]
            .get("description")
            .is_none());

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
            json["result"]["structuredContent"]["fields"]["assignee"]["accountId"],
            "account-42"
        );
        assert!(json["result"]["structuredContent"]["fields"]
            .get("summary")
            .is_none());
        assert!(json["result"]["structuredContent"]["fields"]
            .get("description")
            .is_none());

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
        let description = &json["result"]["structuredContent"]["fields"]["description"];
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        let transitions = json["result"]["structuredContent"]["transitions"]
            .as_array()
            .unwrap();
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
        assert_eq!(
            json["result"]["structuredContent"]["transition"]["transition"]["id"],
            "2"
        );

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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
    }

    #[tokio::test]
    async fn mcp_jira_search_issues_uses_forced_project_prefix() {
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
                "jira_search_issues",
                json!({"project": "PROJ", "jql_filter": "status = \"In Progress\"", "max_results": 10}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        let jql = json["result"]["structuredContent"]["jql"].as_str().unwrap();
        assert!(jql.starts_with("project = \"PROJ\" AND"));
        assert!(jql.contains("status = \"In Progress\""));
        assert_eq!(json["result"]["structuredContent"]["maxResults"], 10);
    }

    #[tokio::test]
    async fn mcp_jira_search_issues_rejects_project_override_in_filter() {
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
                "jira_search_issues",
                json!({"project": "PROJ", "jql_filter": "project = \"OTHER\""}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_jira_search_issues_clamps_max_results() {
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
                "jira_search_issues",
                json!({"project": "PROJ", "max_results": 500}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(json["result"]["structuredContent"]["maxResults"], 100);
    }

    #[tokio::test]
    async fn mcp_jira_search_issues_forbidden_project_returns_policy_error() {
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
                "jira_search_issues",
                json!({"project": "OTHER"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
    }

    #[tokio::test]
    async fn mcp_jira_list_comments_allowed_and_strips_headers() {
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
                "jira_list_comments",
                json!({"issue_key": "PROJ-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        let comments = json["result"]["structuredContent"]["comments"]
            .as_array()
            .unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["author"]["displayName"], "Alice");

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(upstream_headers.get("cookie").is_none());
    }

    #[tokio::test]
    async fn mcp_jira_list_comments_forbidden_project_returns_policy_error() {
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
                "jira_list_comments",
                json!({"issue_key": "OTHER-123"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
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
        assert_eq!(json["result"]["structuredContent"]["title"], "Demo Page");

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

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(json["result"]["structuredContent"]["id"], "67890");
        assert_eq!(json["result"]["structuredContent"]["title"], "New Page");

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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(json["result"]["structuredContent"]["id"], "67890");
        assert_eq!(json["result"]["structuredContent"]["parentId"], "67890");
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
        assert_eq!(json["result"]["structuredContent"]["id"], "67890");
        assert_eq!(json["result"]["structuredContent"]["title"], "Updated Page");

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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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

    #[tokio::test]
    async fn mcp_confluence_list_pages_allowed_and_has_pagination_hint() {
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
                "confluence_list_pages",
                json!({"space": "SPACE", "space_id": "12345"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        let results = json["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(json["result"]["structuredContent"]["has_more"], false);

        let upstream_headers = captured.lock().unwrap().pop().unwrap();
        assert_eq!(
            upstream_headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(upstream_headers.get("cookie").is_none());
    }

    #[tokio::test]
    async fn mcp_confluence_list_pages_has_more_when_next_present() {
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
                "confluence_list_pages",
                json!({"space": "SPACE", "space_id": "999"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(json["result"]["structuredContent"]["has_more"], true);
    }

    #[tokio::test]
    async fn mcp_confluence_list_pages_forbidden_space_returns_policy_error() {
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            false,
            None,
            vec!["OTHER".into()],
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
                "confluence_list_pages",
                json!({"space": "SPACE", "space_id": "12345"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
    }

    #[tokio::test]
    async fn mcp_confluence_delete_page_allowed_and_audited() {
        let audit_path = temp_audit_path();
        let (port, _captured) = mock_confluence_server().await;
        let config = confluence_test_config(
            format!("http://127.0.0.1:{}", port),
            true,
            Some(audit_path.clone()),
            vec!["SPACE".into()],
        );
        let confluence = ConfluenceClient::new(config.atlassian.as_ref().unwrap()).unwrap();
        let audit = Some(AuditLog::new(audit_path.clone()));
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
                "confluence_delete_page",
                json!({"space": "SPACE", "page_id": "67890"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);

        let content = std::fs::read_to_string(&audit_path).unwrap();
        assert!(content.contains("\"result\":\"attempt\""));
        assert!(content.contains("\"result\":\"success\""));
    }

    #[tokio::test]
    async fn mcp_confluence_delete_page_write_disabled_returns_policy_error() {
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
                "confluence_delete_page",
                json!({"space": "SPACE", "page_id": "67890"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("write tools not enabled for agent"));
    }

    #[tokio::test]
    async fn mcp_confluence_delete_page_rejects_invalid_page_id() {
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
                "confluence_delete_page",
                json!({"space": "SPACE", "page_id": "abc"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_confluence_get_page_by_title_resolves_and_returns_body() {
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
                json!({"space": "SPACE", "space_id": "12345", "title": "Unique Page"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(json["result"]["structuredContent"]["id"], "456");
        assert!(json["result"]["structuredContent"]["body"]["view"]["value"]
            .as_str()
            .is_some());
    }

    #[tokio::test]
    async fn mcp_confluence_get_page_by_title_not_found_returns_calltool_error() {
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
                json!({"space": "SPACE", "space_id": "12345", "title": "No Such Page"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no Confluence page found"));
    }

    #[tokio::test]
    async fn mcp_confluence_get_page_by_title_ambiguous_returns_calltool_error() {
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
                json!({"space": "SPACE", "space_id": "12345", "title": "Ambiguous"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("multiple Confluence pages found"));
    }

    #[tokio::test]
    async fn mcp_confluence_get_page_by_page_id_still_works() {
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
                json!({"space": "SPACE", "page_id": "67890"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(json["result"]["structuredContent"]["id"], "67890");
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

        let diffstat_handler = |State(s): State<MockState>,
                                Path((workspace, repo, pr_id)): Path<(String, String, String)>,
                                headers: HeaderMap| async move {
            s.headers.lock().unwrap().push(headers);
            Json(json!({
                "values": [
                    {
                        "status": "added",
                        "old": null,
                        "new": { "path": "src/new.rs", "type": "commit_file" },
                        "lines_added": 42,
                        "lines_removed": 0
                    },
                    {
                        "status": "modified",
                        "old": { "path": "src/main.rs", "type": "commit_file" },
                        "new": { "path": "src/main.rs", "type": "commit_file" },
                        "lines_added": 10,
                        "lines_removed": 3
                    },
                    {
                        "status": "removed",
                        "old": { "path": "README.old", "type": "commit_file" },
                        "new": null,
                        "lines_added": 0,
                        "lines_removed": 5
                    }
                ],
                "size": 3,
                "page": 1,
                "pagelen": 10,
                "pullrequest": { "id": pr_id, "title": "PR title" },
                "repository": { "full_name": format!("{workspace}/{repo}") }
            }))
        };

        let diff_handler = |State(s): State<MockState>,
                            Path((_workspace, _repo, pr_id)): Path<(String, String, String)>,
                            headers: HeaderMap| async move {
            s.headers.lock().unwrap().push(headers);
            if pr_id == "43" {
                // Return invalid UTF-8 bytes to simulate a binary diff.
                return (StatusCode::OK, vec![0xffu8, 0xfe, 0xfd, 0xfc]).into_response();
            }

            if pr_id == "44" {
                let body = (0..2500)
                    .map(|i| format!("+line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                return (StatusCode::OK, body).into_response();
            }

            let body = format!(
                "diff --git a/src/main.rs b/src/main.rs\n\
index abc..def 100644\n\
--- a/src/main.rs\n\
+++ b/src/main.rs\n\
@@ -1,3 +1,6 @@\n\
+aws = \"AKIA0123456789ABCDEF\"\n\
+openai = \"sk-abcdefghijklmnopqrstuvwxyz\"\n\
+token = \"Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature\"\n\
 fn main() {{\n\
     println!(\"hello\");\n\
 }}\n"
            );
            (StatusCode::OK, body).into_response()
        };

        let pipeline_status_handler = |State(s): State<MockState>,
                                       Path((workspace, repo)): Path<(String, String)>,
                                       Query(params): Query<
            std::collections::HashMap<String, String>,
        >,
                                       headers: HeaderMap| async move {
            s.headers.lock().unwrap().push(headers);
            let branch = params
                .get("target.branch")
                .map(|s| s.as_str())
                .unwrap_or("main");

            // Simulate "no pipelines" for the literal sentinel branch.
            if branch == "no-pipelines" {
                return (
                    StatusCode::NOT_FOUND,
                    Json(
                        json!({"type": "error", "error": { "message": "There are no pipelines configured for this repository." } }),
                    ),
                );
            }

            let response = if branch == "feature/in-progress" {
                json!({
                    "values": [
                        {
                            "uuid": "{pipeline-uuid}",
                            "state": "IN_PROGRESS",
                            "result": null,
                            "created_on": "2026-07-22T00:00:00Z",
                            "target": { "type": "pipeline_target_branch", "ref_name": branch },
                            "repository": { "full_name": format!("{workspace}/{repo}") }
                        }
                    ],
                    "size": 1,
                    "page": 1,
                    "pagelen": 1
                })
            } else if branch == "feature/failed" {
                json!({
                    "values": [
                        {
                            "uuid": "{pipeline-uuid}",
                            "state": "COMPLETED",
                            "result": "FAILED",
                            "created_on": "2026-07-22T00:00:00Z",
                            "target": { "type": "pipeline_target_branch", "ref_name": branch },
                            "repository": { "full_name": format!("{workspace}/{repo}") }
                        }
                    ],
                    "size": 1,
                    "page": 1,
                    "pagelen": 1
                })
            } else {
                json!({
                    "values": [
                        {
                            "uuid": "{pipeline-uuid}",
                            "state": "COMPLETED",
                            "result": "SUCCESSFUL",
                            "created_on": "2026-07-22T00:00:00Z",
                            "target": { "type": "pipeline_target_branch", "ref_name": branch },
                            "repository": { "full_name": format!("{workspace}/{repo}") }
                        }
                    ],
                    "size": 1,
                    "page": 1,
                    "pagelen": 1
                })
            };

            (StatusCode::OK, Json(response))
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

        let list_pull_requests_handler =
            |State(s): State<MockState>,
             Path((workspace, repo)): Path<(String, String)>,
             Query(params): Query<std::collections::HashMap<String, String>>,
             headers: HeaderMap| async move {
                s.headers.lock().unwrap().push(headers);
                let state = params.get("state").map(|s| s.as_str()).unwrap_or("OPEN");
                let values = match state {
                    "MERGED" => vec![json!({
                        "id": 1,
                        "title": "merged PR",
                        "state": "MERGED",
                        "source": { "branch": { "name": "feature/old" } },
                        "destination": { "branch": { "name": "main" } }
                    })],
                    "DECLINED" => vec![json!({
                        "id": 3,
                        "title": "declined PR",
                        "state": "DECLINED",
                        "source": { "branch": { "name": "feature/nope" } },
                        "destination": { "branch": { "name": "main" } }
                    })],
                    _ => vec![
                        json!({
                            "id": 2,
                            "title": "open PR one",
                            "state": "OPEN",
                            "source": { "branch": { "name": "feature/a" } },
                            "destination": { "branch": { "name": "main" } }
                        }),
                        json!({
                            "id": 4,
                            "title": "open PR two",
                            "state": "OPEN",
                            "source": { "branch": { "name": "feature/b" } },
                            "destination": { "branch": { "name": "main" } }
                        }),
                    ],
                };
                let mut response = json!({
                    "values": values,
                    "size": values.len(),
                    "page": 1,
                    "pagelen": 10
                });
                if state == "OPEN" {
                    response["next"] = json!(format!("http://127.0.0.1/repositories/{workspace}/{repo}/pullrequests?state=OPEN&page=2"));
                }
                Json(response)
            };

        let merge_pr_handler = |State(s): State<MockState>,
                                Path((workspace, repo, pr_id)): Path<(String, String, String)>,
                                headers: HeaderMap,
                                body: String| async move {
            s.headers.lock().unwrap().push(headers);
            s.bodies.lock().unwrap().push(body);
            Json(json!({
                "id": pr_id,
                "title": "merged PR",
                "state": "MERGED",
                "source": { "repository": { "full_name": format!("{workspace}/{repo}") } }
            }))
        };

        let decline_pr_handler =
            |State(s): State<MockState>,
             Path((workspace, repo, pr_id)): Path<(String, String, String)>,
             headers: HeaderMap| async move {
                s.headers.lock().unwrap().push(headers);
                Json(json!({
                    "id": pr_id,
                    "title": "declined PR",
                    "state": "DECLINED",
                    "source": { "repository": { "full_name": format!("{workspace}/{repo}") } }
                }))
            };

        let delete_branch_handler =
            |State(s): State<MockState>,
             Path((workspace, repo, branch_name)): Path<(String, String, String)>,
             headers: HeaderMap| async move {
                s.headers.lock().unwrap().push(headers);
                Json(json!({
                    "name": branch_name,
                    "type": "branch",
                    "repository": { "full_name": format!("{workspace}/{repo}") }
                }))
            };

        let add_pr_comment_handler =
            |State(s): State<MockState>,
             Path((workspace, repo, pr_id)): Path<(String, String, String)>,
             headers: HeaderMap,
             body: String| async move {
                s.headers.lock().unwrap().push(headers);
                s.bodies.lock().unwrap().push(body);
                Json(json!({
                    "id": 123,
                    "pullrequest": { "id": pr_id, "title": "PR title" },
                    "content": { "raw": "comment body" },
                    "user": { "nickname": "atlapool-bot" },
                    "repository": { "full_name": format!("{workspace}/{repo}") }
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
                "/repositories/{workspace}/{repo}/pullrequests/{pr_id}/diffstat",
                get(diffstat_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/pullrequests/{pr_id}/diff",
                get(diff_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/pipelines/",
                get(pipeline_status_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/refs/branches",
                get(list_branches_handler).post(create_branch_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/pullrequests",
                get(list_pull_requests_handler).post(create_pr_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/pullrequests/{pr_id}/merge",
                post(merge_pr_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/pullrequests/{pr_id}/decline",
                post(decline_pr_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/pullrequests/{pr_id}/comments",
                post(add_pr_comment_handler),
            )
            .route(
                "/repositories/{workspace}/{repo}/refs/branches/{*branch_name}",
                delete(delete_branch_handler),
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
        assert_eq!(
            json["result"]["structuredContent"]["full_name"],
            "WORK/my-repo"
        );

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
        assert_eq!(json["result"]["structuredContent"]["id"], "42");

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
    async fn mcp_bitbucket_list_pull_request_changes_allowed_and_has_no_code() {
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
                "bitbucket_list_pull_request_changes",
                json!({"repo_slug": "my-repo", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let values = json["result"]["structuredContent"]["values"]
            .as_array()
            .unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0]["status"], "added");
        assert_eq!(values[0]["new"]["path"], "src/new.rs");
        assert_eq!(values[0]["lines_added"], 42);
        assert_eq!(values[1]["status"], "modified");
        assert!(!values[1].as_object().unwrap().contains_key("diff"));
        assert!(!json["result"]["structuredContent"]
            .to_string()
            .contains("fn main"));

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
    async fn mcp_bitbucket_list_pull_request_changes_invalid_id_returns_400() {
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
                "bitbucket_list_pull_request_changes",
                json!({"repo_slug": "my-repo", "pull_request_id": "42abc"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_pull_request_changes_allowlist_blocks_repo() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["allowed-repo".into()],
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
                "bitbucket_list_pull_request_changes",
                json!({"repo_slug": "my-repo", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pipeline_status_passed() {
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
                "bitbucket_get_pipeline_status",
                json!({"repo_slug": "my-repo", "branch": "main"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(
            json["result"]["structuredContent"]["normalized_status"],
            "passed"
        );
        assert_eq!(
            json["result"]["structuredContent"]["pipeline"]["state"],
            "COMPLETED"
        );
        assert_eq!(
            json["result"]["structuredContent"]["pipeline"]["result"],
            "SUCCESSFUL"
        );
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pipeline_status_running() {
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
                "bitbucket_get_pipeline_status",
                json!({"repo_slug": "my-repo", "branch": "feature/in-progress"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(
            json["result"]["structuredContent"]["normalized_status"],
            "running"
        );
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pipeline_status_failed() {
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
                "bitbucket_get_pipeline_status",
                json!({"repo_slug": "my-repo", "branch": "feature/failed"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(
            json["result"]["structuredContent"]["normalized_status"],
            "failed"
        );
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pipeline_status_no_pipelines_returns_unknown_not_error() {
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
                "bitbucket_get_pipeline_status",
                json!({"repo_slug": "my-repo", "branch": "no-pipelines"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(
            json["result"]["structuredContent"]["normalized_status"],
            "unknown"
        );
        assert!(json["result"]["structuredContent"]["status_message"]
            .as_str()
            .unwrap()
            .contains("not configured"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pipeline_status_invalid_branch_returns_400() {
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
                "bitbucket_get_pipeline_status",
                json!({"repo_slug": "my-repo", "branch": ".."}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pipeline_status_allowlist_blocks_repo() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["allowed-repo".into()],
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
                "bitbucket_get_pipeline_status",
                json!({"repo_slug": "my-repo", "branch": "main"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pull_request_diff_returns_redacted_raw_diff() {
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
                "bitbucket_get_pull_request_diff",
                json!({"repo_slug": "my-repo", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        let diff = json["result"]["structuredContent"]["diff"]
            .as_str()
            .unwrap();
        assert!(diff.contains("diff --git"));
        assert!(diff.contains("[REDACTED]"));
        assert!(!diff.contains("AKIA0123456789ABCDEF"));
        assert!(!diff.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(!diff.contains("Bearer eyJhbGciOiJIUzI1NiJ9"));
        assert_eq!(json["result"]["structuredContent"]["total_lines"], 11);
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pull_request_diff_truncates_with_max_lines() {
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
                "bitbucket_get_pull_request_diff",
                json!({"repo_slug": "my-repo", "pull_request_id": "44", "max_lines": 100}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(json["result"]["structuredContent"]["total_lines"], 2500);
        assert_eq!(json["result"]["structuredContent"]["truncated"], true);
        let diff = json["result"]["structuredContent"]["diff"]
            .as_str()
            .unwrap();
        let diff_lines: Vec<&str> = diff.lines().collect();
        // 100 content lines + the truncation marker line.
        assert_eq!(diff_lines.len(), 101);
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pull_request_diff_binary_marked() {
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
                "bitbucket_get_pull_request_diff",
                json!({"repo_slug": "my-repo", "pull_request_id": "43"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(
            json["result"]["structuredContent"]["diff"],
            "[binary file, diff not shown]"
        );
        assert_eq!(json["result"]["structuredContent"]["binary"], true);
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pull_request_diff_invalid_pr_id_returns_400() {
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
                "bitbucket_get_pull_request_diff",
                json!({"repo_slug": "my-repo", "pull_request_id": "42abc"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_pull_request_diff_allowlist_blocks_repo() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["allowed-repo".into()],
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
                "bitbucket_get_pull_request_diff",
                json!({"repo_slug": "my-repo", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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

    #[tokio::test]
    async fn mcp_bitbucket_list_pull_requests_defaults_open_and_has_more() {
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
                "bitbucket_list_pull_requests",
                json!({"repo_slug": "my-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let values = json["result"]["structuredContent"]["values"]
            .as_array()
            .unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["state"], "OPEN");
        assert_eq!(values[0]["source"]["branch"]["name"], "feature/a");
        assert_eq!(values[0]["destination"]["branch"]["name"], "main");
        assert_eq!(json["result"]["structuredContent"]["has_more"], true);
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_pull_requests_with_state_merged_and_no_more() {
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
                "bitbucket_list_pull_requests",
                json!({"repo_slug": "my-repo", "state": "MERGED"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        let values = json["result"]["structuredContent"]["values"]
            .as_array()
            .unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["state"], "MERGED");
        assert_eq!(json["result"]["structuredContent"]["has_more"], false);
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_pull_requests_invalid_state_returns_400() {
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
                "bitbucket_list_pull_requests",
                json!({"repo_slug": "my-repo", "state": "BOGUS"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_list_pull_requests_allowlist_blocks_repo() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["allowed-repo".into()],
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
                "bitbucket_list_pull_requests",
                json!({"repo_slug": "my-repo"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text == "not permitted by agent policy" || text == "write tools not enabled for agent",
            "unexpected policy denial message: {}",
            text
        );
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
    async fn mcp_bitbucket_merge_pull_request_allowed_and_audited() {
        let (port, _captured, bodies) = mock_bitbucket_server().await;
        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_merge_pull_request".into()],
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
                "bitbucket_merge_pull_request",
                json!({
                    "repo_slug": "my-repo",
                    "pull_request_id": "42",
                    "close_source_branch": false
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(json["result"]["structuredContent"]["state"], "MERGED");

        let upstream_body = bodies.lock().unwrap().pop().unwrap();
        let upstream: Value = serde_json::from_str(&upstream_body).unwrap();
        assert_eq!(upstream["merge_strategy"], "merge_commit");
        assert_eq!(upstream["close_source_branch"], false);

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_bitbucket_merge_pull_request_defaults_close_source_branch_to_true() {
        let (port, _captured, bodies) = mock_bitbucket_server().await;
        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_merge_pull_request".into()],
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
                "bitbucket_merge_pull_request",
                json!({"repo_slug": "my-repo", "pull_request_id": "7"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let upstream_body = bodies.lock().unwrap().pop().unwrap();
        let upstream: Value = serde_json::from_str(&upstream_body).unwrap();
        assert_eq!(upstream["merge_strategy"], "merge_commit");
        assert_eq!(upstream["close_source_branch"], true);

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_bitbucket_merge_pull_request_write_disabled_returns_calltool_error() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_merge_pull_request".into()],
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
                "bitbucket_merge_pull_request",
                json!({"repo_slug": "my-repo", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("write tools not enabled for agent"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_merge_pull_request_invalid_id_returns_400() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_merge_pull_request".into()],
            true,
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
                "bitbucket_merge_pull_request",
                json!({"repo_slug": "my-repo", "pull_request_id": "42abc"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_merge_pull_request_allowlist_blocks_repo() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let mut config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["allowed-repo".into()],
        );
        config.agents[0].enable_writes = Some(true);
        config.agents[0].tools = vec!["bitbucket_merge_pull_request".into()];
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
                "bitbucket_merge_pull_request",
                json!({"repo_slug": "my-repo", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_decline_pull_request_allowed_and_audited() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_decline_pull_request".into()],
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
                "bitbucket_decline_pull_request",
                json!({"repo_slug": "my-repo", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(json["result"]["structuredContent"]["state"], "DECLINED");

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_bitbucket_decline_pull_request_write_disabled_returns_calltool_error() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_decline_pull_request".into()],
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
                "bitbucket_decline_pull_request",
                json!({"repo_slug": "my-repo", "pull_request_id": "42"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("write tools not enabled for agent"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_decline_pull_request_invalid_id_returns_400() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_decline_pull_request".into()],
            true,
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
                "bitbucket_decline_pull_request",
                json!({"repo_slug": "my-repo", "pull_request_id": "42abc"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_delete_branch_allowed_and_audited_with_slash_encoding() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_delete_branch".into()],
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
                "bitbucket_delete_branch",
                json!({"repo_slug": "my-repo", "branch_name": "feature/x"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(json["result"]["structuredContent"]["name"], "feature/x");

        let log = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"result\":\"attempt\""));
        assert!(lines[1].contains("\"result\":\"success\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_bitbucket_delete_branch_write_disabled_returns_calltool_error() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_delete_branch".into()],
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
                "bitbucket_delete_branch",
                json!({"repo_slug": "my-repo", "branch_name": "feature/x"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("write tools not enabled for agent"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_delete_branch_rejects_dotdot() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_delete_branch".into()],
            true,
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
                "bitbucket_delete_branch",
                json!({"repo_slug": "my-repo", "branch_name": ".."}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_delete_branch_allowlist_blocks_repo() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let mut config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["allowed-repo".into()],
        );
        config.agents[0].enable_writes = Some(true);
        config.agents[0].tools = vec!["bitbucket_delete_branch".into()];
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
                "bitbucket_delete_branch",
                json!({"repo_slug": "my-repo", "branch_name": "feature/x"}),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_add_pull_request_comment_allowed_and_audited_without_content() {
        let (port, _captured, bodies) = mock_bitbucket_server().await;
        let (config, audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_add_pull_request_comment".into()],
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
                "bitbucket_add_pull_request_comment",
                json!({
                    "repo_slug": "my-repo",
                    "pull_request_id": "42",
                    "content": "LGTM, please merge"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(
            json["result"]["structuredContent"]["pullrequest"]["id"],
            "42"
        );

        let upstream_body = bodies.lock().unwrap().pop().unwrap();
        let upstream: Value = serde_json::from_str(&upstream_body).unwrap();
        assert_eq!(upstream["content"]["raw"], "LGTM, please merge");

        let log = std::fs::read_to_string(&audit_path).unwrap();
        assert!(!log.contains("LGTM, please merge"));
        assert!(log.contains("\"target\":\"WORK\""));

        std::fs::remove_file(&audit_path).ok();
    }

    #[tokio::test]
    async fn mcp_bitbucket_add_pull_request_comment_write_disabled_returns_calltool_error() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_add_pull_request_comment".into()],
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
                "bitbucket_add_pull_request_comment",
                json!({
                    "repo_slug": "my-repo",
                    "pull_request_id": "42",
                    "content": "nope"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("write tools not enabled for agent"));
    }

    #[tokio::test]
    async fn mcp_bitbucket_add_pull_request_comment_invalid_id_returns_400() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let (config, _audit_path) = bitbucket_write_test_config(
            format!("http://127.0.0.1:{}", port),
            vec!["bitbucket_add_pull_request_comment".into()],
            true,
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
                "bitbucket_add_pull_request_comment",
                json!({
                    "repo_slug": "my-repo",
                    "pull_request_id": "42abc",
                    "content": "hi"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_bitbucket_add_pull_request_comment_allowlist_blocks_repo() {
        let (port, _captured, _bodies) = mock_bitbucket_server().await;
        let mut config = bitbucket_test_config(
            format!("http://127.0.0.1:{}", port),
            None,
            vec!["WORK".into()],
            vec!["allowed-repo".into()],
        );
        config.agents[0].enable_writes = Some(true);
        config.agents[0].tools = vec!["bitbucket_add_pull_request_comment".into()];
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
                "bitbucket_add_pull_request_comment",
                json!({
                    "repo_slug": "my-repo",
                    "pull_request_id": "42",
                    "content": "hi"
                }),
                Some("agent-key"),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["result"]["isError"], true);
        let text = json["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted by agent policy"));
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
        let values = json["result"]["structuredContent"]["values"]
            .as_array()
            .unwrap();
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
        let values = json["result"]["structuredContent"]["values"]
            .as_array()
            .unwrap();
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
        let values = json["result"]["structuredContent"]["values"]
            .as_array()
            .unwrap();
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
        assert!(json["result"]["content"][0]["text"]
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
        assert!(json["result"]["content"][0]["text"]
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
        assert_eq!(json["result"]["isError"], false);
        assert_eq!(json["result"]["structuredContent"], json!({}));

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
        assert_eq!(call["result"]["structuredContent"]["key"], "PROJ-123");
    }

    #[tokio::test]
    async fn mcp_real_client_initialize_list_call_flow() {
        use axum::http::{HeaderName, HeaderValue};
        use rmcp::model::CallToolRequestParams;
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        use rmcp::transport::StreamableHttpClientTransport;
        use rmcp::ServiceExt;

        let (port, _captured) = mock_jira_server().await;
        let mut config = test_config(format!("http://127.0.0.1:{}", port), false, None);
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut custom_headers = std::collections::HashMap::new();
        custom_headers.insert(
            HeaderName::from_static("x-atlapool-key"),
            HeaderValue::from_static("agent-key"),
        );

        let transport_config = StreamableHttpClientTransportConfig::with_uri(format!(
            "http://127.0.0.1:{}/mcp",
            server_port
        ))
        .custom_headers(custom_headers);

        let transport = StreamableHttpClientTransport::from_config(transport_config);
        let client = ().serve(transport).await.unwrap();

        let tools = client.peer().list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_ref(), "jira_get_issue");

        let args = serde_json::json!({"issue_key": "PROJ-123"})
            .as_object()
            .cloned()
            .unwrap();
        let result = client
            .peer()
            .call_tool(CallToolRequestParams::new("jira_get_issue").with_arguments(args))
            .await
            .unwrap();

        let structured = result
            .structured_content
            .expect("expected structured content");
        assert_eq!(structured["key"], "PROJ-123");
    }

    #[tokio::test]
    async fn mcp_real_client_policy_denials_return_calltool_error() {
        use axum::http::{HeaderName, HeaderValue};
        use rmcp::model::CallToolRequestParams;
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        use rmcp::transport::StreamableHttpClientTransport;
        use rmcp::ServiceExt;

        let (port, _captured) = mock_jira_server().await;
        let mut config = test_config(format!("http://127.0.0.1:{}", port), false, None);
        config.agents[0].tools = vec!["jira_get_issue".into(), "jira_add_comment".into()];
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut custom_headers = std::collections::HashMap::new();
        custom_headers.insert(
            HeaderName::from_static("x-atlapool-key"),
            HeaderValue::from_static("agent-key"),
        );

        let transport_config = StreamableHttpClientTransportConfig::with_uri(format!(
            "http://127.0.0.1:{}/mcp",
            server_port
        ))
        .custom_headers(custom_headers);

        let transport = StreamableHttpClientTransport::from_config(transport_config);
        let client = ().serve(transport).await.unwrap();

        // allowlist denial: jira_create_issue is not in the agent's tool list.
        let create_args = serde_json::json!({"project": "PROJ", "summary": "New issue"})
            .as_object()
            .cloned()
            .unwrap();
        let create_result = client
            .peer()
            .call_tool(CallToolRequestParams::new("jira_create_issue").with_arguments(create_args))
            .await
            .unwrap();
        assert_eq!(create_result.is_error, Some(true));
        let create_text = &create_result.content[0].as_text().unwrap().text;
        assert!(create_text.contains("not permitted by agent policy"));

        // write-gate denial: writes are disabled for the agent.
        let comment_args = serde_json::json!({
            "issue_key": "PROJ-123",
            "body": {
                "type": "doc",
                "version": 1,
                "content": [{"type": "paragraph", "content": [{"type": "text", "text": "hi"}]}]
            }
        })
        .as_object()
        .cloned()
        .unwrap();
        let comment_result = client
            .peer()
            .call_tool(CallToolRequestParams::new("jira_add_comment").with_arguments(comment_args))
            .await
            .unwrap();
        assert_eq!(comment_result.is_error, Some(true));
        let comment_text = &comment_result.content[0].as_text().unwrap().text;
        assert!(comment_text.contains("write tools not enabled for agent"));
    }

    #[tokio::test]
    async fn mcp_real_client_bitbucket_get_repo_success() {
        use axum::http::{HeaderName, HeaderValue};
        use rmcp::model::CallToolRequestParams;
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        use rmcp::transport::StreamableHttpClientTransport;
        use rmcp::ServiceExt;

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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut custom_headers = std::collections::HashMap::new();
        custom_headers.insert(
            HeaderName::from_static("x-atlapool-key"),
            HeaderValue::from_static("agent-key"),
        );

        let transport_config = StreamableHttpClientTransportConfig::with_uri(format!(
            "http://127.0.0.1:{}/mcp",
            server_port
        ))
        .custom_headers(custom_headers);

        let transport = StreamableHttpClientTransport::from_config(transport_config);
        let client = ().serve(transport).await.unwrap();

        let args = serde_json::json!({"workspace": "WORK", "repo_slug": "my-repo"})
            .as_object()
            .cloned()
            .unwrap();
        let result = client
            .peer()
            .call_tool(CallToolRequestParams::new("bitbucket_get_repo").with_arguments(args))
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(false));
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.contains("WORK/my-repo"));

        let structured = result
            .structured_content
            .expect("expected structured content");
        assert_eq!(structured["full_name"], "WORK/my-repo");
    }
}
