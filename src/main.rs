use axum::{
    extract::State,
    response::Json,
    routing::get,
    Router,
};
use serde_json::json;
use std::env;
use std::time::Instant;
use tokio::net::TcpListener;
use tracing::info;

#[derive(Clone)]
struct AppState {
    start: Instant,
}

#[tokio::main]
async fn main() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "atlapool=info".into());
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let port: u16 = port.parse().expect("PORT must be a valid u16");

    let state = AppState {
        start: Instant::now(),
    };
    let app = router(state);

    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind");
    info!("atlapool listening on 0.0.0.0:{}", port);
    axum::serve(listener, app).await.unwrap();
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
    Json(json!({"uptime_secs": uptime_secs}))
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
        });
        let response = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
