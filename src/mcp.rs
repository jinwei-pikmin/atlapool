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

use crate::upstream::UpstreamClient;
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
    body: Option<Value>,
}

pub async fn mcp_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let key = headers
        .get(MCP_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());

    let Some(key) = key else {
        return mcp_error(
            None,
            StatusCode::UNAUTHORIZED,
            "missing or empty X-Atlapool-Key",
        );
    };

    let agent = match agents::find_agent(&state.config.agents, key) {
        Some(a) => a,
        None => {
            tracing::warn!("rejected request: unknown X-Atlapool-Key");
            return mcp_error(None, StatusCode::UNAUTHORIZED, "unknown key");
        }
    };

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

    if request.method != "tools/call" {
        return mcp_error(
            request.id,
            StatusCode::NOT_IMPLEMENTED,
            "method not supported",
        );
    }

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
    let target = match resolve_target(&params.name, params.arguments.as_ref(), workspace) {
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
    // is treated as a write and requires enable_writes=true on the agent.
    let is_write = agents::classify_tool(&params.name) == agents::ToolKind::Write;
    if is_write && !agent.enable_writes {
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
    if is_write {
        let Some(audit) = state.audit.as_ref() else {
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

        let target = target
            .project
            .as_deref()
            .or(target.space.as_deref())
            .unwrap_or("unknown");

        if let Err(e) = audit.record_attempt(&agent.id, &params.name, target).await {
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
        forward(confluence, request.id.clone(), target).await
    } else if params.name.starts_with("bitbucket_") {
        let Some(bitbucket) = state.bitbucket.as_ref() else {
            return mcp_error(
                request.id,
                StatusCode::SERVICE_UNAVAILABLE,
                "bitbucket upstream not configured",
            );
        };
        forward(bitbucket, request.id.clone(), target).await
    } else {
        let Some(jira) = state.jira.as_ref() else {
            return mcp_error(
                request.id,
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream not configured",
            );
        };
        forward(jira, request.id.clone(), target).await
    };

    match forward_result {
        Ok(response) => response,
        Err((status, message)) => mcp_error(request.id, status, &message),
    }
}

async fn forward<C: UpstreamClient>(
    client: &C,
    request_id: Option<Value>,
    target: ToolTarget,
) -> Result<(StatusCode, Json<Value>), (StatusCode, String)> {
    let upstream_request = client
        .build_request(target.method, &target.path, target.body)
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let upstream_resp = client.execute(upstream_request).await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("upstream request failed: {}", e),
        )
    })?;

    let upstream_status = upstream_resp.status();
    let upstream_body = upstream_resp.json::<Value>().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to read upstream: {}", e),
        )
    })?;

    let (status, envelope) = if upstream_status.is_success() {
        (
            StatusCode::OK,
            json!({"jsonrpc":"2.0","id": request_id, "result": upstream_body}),
        )
    } else {
        (
            StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32000,
                    "message": format!("upstream returned {}", upstream_status),
                    "data": upstream_body
                }
            }),
        )
    };

    Ok((status, Json(envelope)))
}

fn mcp_error(id: Option<Value>, status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": message }
    });
    (status, Json(body))
}

fn resolve_target(
    tool: &str,
    args: Option<&Value>,
    workspace: Option<&str>,
) -> Result<ToolTarget, String> {
    match tool {
        "jira_get_issue" => {
            let args = args.ok_or("missing arguments")?;
            let issue_key = args
                .get("issue_key")
                .and_then(|v| v.as_str())
                .ok_or("missing issue_key")?;
            let project = issue_key.split_once('-').map(|(p, _)| p.to_string());
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project,
                space: None,
                method: Method::GET,
                path: format!("/rest/api/3/issue/{issue_key}"),
                body: None,
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
                body: Some(args.clone()),
            })
        }
        "jira_add_comment" => {
            let args = args.ok_or("missing arguments")?;
            let issue_key = args
                .get("issue_key")
                .and_then(|v| v.as_str())
                .ok_or("missing issue_key")?;
            let project = issue_key.split_once('-').map(|(p, _)| p.to_string());
            let body = args.get("body").cloned().ok_or("missing body")?;
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project,
                space: None,
                method: Method::POST,
                path: format!("/rest/api/3/issue/{issue_key}/comment"),
                body: Some(json!({ "body": body })),
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
                body: None,
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
            Ok(ToolTarget {
                workspace: None,
                repo: None,
                project: None,
                space: Some(space),
                method: Method::POST,
                path: "/wiki/api/v2/pages".into(),
                body: Some(json!({
                    "spaceId": space_id,
                    "status": "current",
                    "title": title,
                    "body": body_value
                })),
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
                body: Some(json!({
                    "id": page_id,
                    "spaceId": space_id,
                    "status": "current",
                    "title": title,
                    "version": { "number": version },
                    "body": body_value
                })),
            })
        }
        "bitbucket_get_repo" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            if repo_slug.is_empty() {
                return Err("repo_slug must not be empty".into());
            }
            let workspace = workspace.ok_or("missing bitbucket workspace")?;
            Ok(ToolTarget {
                workspace: Some(workspace.into()),
                repo: Some(repo_slug.into()),
                project: None,
                space: None,
                method: Method::GET,
                path: format!("/repositories/{workspace}/{repo_slug}"),
                body: None,
            })
        }
        "bitbucket_get_pull_request" => {
            let args = args.ok_or("missing arguments")?;
            let repo_slug = args
                .get("repo_slug")
                .and_then(|v| v.as_str())
                .ok_or("missing repo_slug")?;
            if repo_slug.is_empty() {
                return Err("repo_slug must not be empty".into());
            }
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
                body: None,
            })
        }
        _ => Err("unsupported tool".into()),
    }
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
                email: None,
                cloud_id: None,
                token: Some(SecretString::new("test-token")),
            }),
            bitbucket: None,
            mcp: McpConfig::default(),
            audit: AuditConfig { path: audit_path },
            agents: vec![AgentConfig {
                id: "demo".into(),
                keys: vec![SecretString::new("agent-key")],
                tools: vec![
                    "jira_get_issue".into(),
                    "jira_create_issue".into(),
                    "jira_add_comment".into(),
                ],
                projects: vec!["PROJ".into()],
                spaces: vec![],
                bitbucket_workspaces: vec![],
                bitbucket_repos: vec![],
                enable_writes,
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
                email: None,
                cloud_id: None,
                token: Some(SecretString::new("test-token")),
            }),
            bitbucket: None,
            mcp: McpConfig::default(),
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
                enable_writes,
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
            }),
            mcp: McpConfig::default(),
            audit: AuditConfig { path: audit_path },
            agents: vec![AgentConfig {
                id: "demo".into(),
                keys: vec![SecretString::new("agent-key")],
                tools: vec![
                    "bitbucket_get_repo".into(),
                    "bitbucket_get_pull_request".into(),
                ],
                projects: vec![],
                spaces: vec![],
                bitbucket_workspaces: workspaces,
                bitbucket_repos: repos,
                enable_writes: false,
            }],
        }
    }

    fn build_request(tool: &str, args: Value, key: Option<&str>) -> Request<Body> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": args
            }
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
                Json(json!({"id":"PROJ-123","key":"PROJ-123"}))
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
            .route("/rest/api/3/issue/{key}", get(get_handler))
            .route("/rest/api/3/issue", post(post_handler))
            .route("/rest/api/3/issue/{key}/comment", post(comment_handler))
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
        let line = log.trim();
        assert!(line.contains("\"agent_id\":\"demo\""));
        assert!(line.contains("\"tool\":\"jira_create_issue\""));
        assert!(line.contains("\"target\":\"PROJ\""));
        assert!(line.contains("\"result\":\"attempt\""));

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
        let line = log.trim();
        assert!(line.contains("\"agent_id\":\"demo\""));
        assert!(line.contains("\"tool\":\"jira_add_comment\""));
        assert!(line.contains("\"target\":\"PROJ\""));
        assert!(line.contains("\"result\":\"attempt\""));

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
        let line = log.trim();
        assert!(line.contains("\"agent_id\":\"demo\""));
        assert!(line.contains("\"tool\":\"confluence_create_page\""));
        assert!(line.contains("\"target\":\"SPACE\""));
        assert!(line.contains("\"result\":\"attempt\""));

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
        let line = log.trim();
        assert!(line.contains("\"agent_id\":\"demo\""));
        assert!(line.contains("\"tool\":\"confluence_update_page\""));
        assert!(line.contains("\"target\":\"SPACE\""));
        assert!(line.contains("\"result\":\"attempt\""));

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

    async fn mock_bitbucket_server() -> (u16, Arc<Mutex<Vec<HeaderMap>>>) {
        #[derive(Clone)]
        struct MockState {
            headers: Arc<Mutex<Vec<HeaderMap>>>,
        }

        let state = MockState {
            headers: Arc::new(Mutex::new(Vec::new())),
        };
        let captured = state.headers.clone();

        let repo_handler = |State(s): State<MockState>,
                            Path((workspace, repo)): Path<(String, String)>,
                            headers: HeaderMap| async move {
            s.headers.lock().unwrap().push(headers);
            Json(json!({
                "full_name": format!("{workspace}/{repo}"),
                "name": repo,
                "workspace": { "slug": workspace }
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

        let app = Router::new()
            .route("/repositories/{workspace}/{repo}", get(repo_handler))
            .route(
                "/repositories/{workspace}/{repo}/pullrequests/{pr_id}",
                get(pr_handler),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // Give the server a moment to start listening.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        (port, captured)
    }

    #[tokio::test]
    async fn mcp_bitbucket_get_repo_allowed_and_strips_headers() {
        let (port, captured) = mock_bitbucket_server().await;
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
        let (port, captured) = mock_bitbucket_server().await;
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
        let (port, _captured) = mock_bitbucket_server().await;
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
        let (port, _captured) = mock_bitbucket_server().await;
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
}
