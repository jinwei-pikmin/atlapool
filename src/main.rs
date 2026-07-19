mod agents;
mod audit;
mod bitbucket;
mod config;
mod confluence;
mod mcp;
mod secrets;
mod upstream;

use axum::{
    extract::State,
    response::Json,
    routing::{get, post},
    Router,
};
use config::Config;
use serde_json::json;
use std::time::Instant;
use tokio::net::TcpListener;
use tracing::info;
use upstream::JiraClient;

#[derive(Clone)]
struct AppState {
    start: Instant,
    config: Config,
    jira: Option<JiraClient>,
    confluence: Option<crate::confluence::ConfluenceClient>,
    bitbucket: Option<crate::bitbucket::BitbucketClient>,
    audit: Option<crate::audit::AuditLog>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "atlapool=info".into());
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let config = Config::load().await?;
    let port = config.port;
    let jira = config.atlassian.as_ref().map(JiraClient::new).transpose()?;
    let confluence = config
        .atlassian
        .as_ref()
        .map(crate::confluence::ConfluenceClient::new)
        .transpose()?;
    let bitbucket = config
        .bitbucket
        .as_ref()
        .map(crate::bitbucket::BitbucketClient::new)
        .transpose()?;
    let audit_path = config
        .audit
        .path
        .clone()
        .unwrap_or_else(|| "atlapool-audit.jsonl".into());
    let audit = Some(crate::audit::AuditLog::new(audit_path));
    let state = AppState {
        start: Instant::now(),
        config: config.clone(),
        jira,
        confluence,
        bitbucket,
        audit,
    };
    let app = router(state);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    info!("atlapool listening on 0.0.0.0:{}", port);
    axum::serve(listener, app).await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/mcp", post(mcp::mcp_handler))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

async fn stats(State(state): State<AppState>) -> Json<serde_json::Value> {
    let uptime_secs = state.start.elapsed().as_secs();
    Json(json!({"uptime_secs": uptime_secs, "port": state.config.port}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_returns_ok_without_external_checks() {
        // /health must stay alive even when no upstream (Jira) or audit is configured.
        let app = router(AppState {
            start: Instant::now(),
            config: Config::default(),
            jira: None,
            confluence: None,
            bitbucket: None,
            audit: None,
        });
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json, serde_json::json!({"status": "ok"}));
    }
}
