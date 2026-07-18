#[allow(dead_code)]
mod agents;
mod config;
mod secrets;
mod upstream;

use axum::{
    extract::State,
    response::Json,
    routing::get,
    Router,
};
use config::Config;
use serde_json::json;
use std::time::Instant;
use tokio::net::TcpListener;
use tracing::info;

#[derive(Clone)]
struct AppState {
    start: Instant,
    config: Config,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "atlapool=info".into());
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let config = Config::load()?;
    let port = config.port;
    let state = AppState {
        start: Instant::now(),
        config: config.clone(),
    };
    let app = router(state);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    info!("atlapool listening on 0.0.0.0:{}", port);
    axum::serve(listener, app).await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/stats", get(stats))
        .with_state(state)
}

async fn healthz() -> Json<serde_json::Value> {
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
    async fn healthz_returns_ok() {
        let app = router(AppState {
            start: Instant::now(),
            config: Config::default(),
        });
        let response = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
