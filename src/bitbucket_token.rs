use reqwest::header;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Bitbucket OAuth 2.0 token cache for the client-credentials flow.
///
/// The cache keeps a short-lived access token in memory and refreshes it before
/// it expires. A single `tokio::sync::Mutex` is used so that only one refresh
/// request is in flight at a time.
#[derive(Clone)]
pub struct BitbucketTokenCache {
    client: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String,
    state: Arc<Mutex<TokenState>>,
}

#[derive(Clone, Debug, Default)]
struct TokenState {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<time::OffsetDateTime>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

impl BitbucketTokenCache {
    pub fn new(client_id: String, client_secret: String, token_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            token_url,
            client_id,
            client_secret,
            state: Arc::new(Mutex::new(TokenState::default())),
        }
    }

    /// Return a valid access token, fetching or refreshing it if necessary.
    pub async fn get_token(&self) -> Result<String, crate::upstream::UpstreamError> {
        let mut state = self.state.lock().await;

        if let Some(token) = state.access_token.as_ref() {
            if let Some(expires) = state.expires_at {
                if time::OffsetDateTime::now_utc() < expires {
                    return Ok(token.clone());
                }
            }
        }

        // Try refresh first if we have one; fall back to client_credentials.
        if let Some(refresh) = state.refresh_token.clone() {
            match self.fetch_token("refresh_token", Some(&refresh)).await {
                Ok(resp) => {
                    self.update_state(&mut state, resp)?;
                    return Ok(state.access_token.clone().unwrap());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to refresh Bitbucket token, falling back to client_credentials");
                }
            }
        }

        let resp = self.fetch_token("client_credentials", None).await?;
        self.update_state(&mut state, resp)?;
        Ok(state.access_token.clone().unwrap())
    }

    async fn fetch_token(
        &self,
        grant_type: &str,
        refresh_token: Option<&str>,
    ) -> Result<TokenResponse, crate::upstream::UpstreamError> {
        let mut params = vec![("grant_type", grant_type)];
        if let Some(rt) = refresh_token {
            params.push(("refresh_token", rt));
        }

        let response = self
            .client
            .post(&self.token_url)
            .header(
                header::AUTHORIZATION,
                format!(
                    "Basic {}",
                    base64::encode(format!("{}:{}", self.client_id, self.client_secret))
                ),
            )
            .form(&params)
            .send()
            .await
            .map_err(|e| {
                crate::upstream::UpstreamError::TokenFetch(format!("token request failed: {}", e))
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(crate::upstream::UpstreamError::TokenFetch(format!(
                "token endpoint returned {}: {}",
                status, body
            )));
        }

        let token: TokenResponse = response.json().await.map_err(|e| {
            crate::upstream::UpstreamError::TokenFetch(format!(
                "failed to parse token response: {}",
                e
            ))
        })?;

        if token.access_token.is_empty() {
            return Err(crate::upstream::UpstreamError::TokenFetch(
                "token response missing access_token".into(),
            ));
        }

        Ok(token)
    }

    fn update_state(
        &self,
        state: &mut TokenState,
        token: TokenResponse,
    ) -> Result<(), crate::upstream::UpstreamError> {
        let expires_in = token.expires_in.unwrap_or(7200);
        let expires_in: i64 = expires_in.try_into().map_err(|_| {
            crate::upstream::UpstreamError::TokenFetch("expires_in too large".into())
        })?;

        // Refresh with a grace period, but never exceed the token lifetime.
        // For normal 2-hour tokens, refresh 10 minutes early; for very short
        // tokens, refresh after at least one second.
        let grace = (expires_in / 10).clamp(1, 600);
        let effective_lifetime = (expires_in - grace).max(1);
        let expires_at =
            time::OffsetDateTime::now_utc() + time::Duration::seconds(effective_lifetime);

        state.access_token = Some(token.access_token);
        state.refresh_token = token.refresh_token.or(state.refresh_token.clone());
        state.expires_at = Some(expires_at);
        Ok(())
    }
}

impl std::fmt::Debug for BitbucketTokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitbucketTokenCache")
            .field("token_url", &self.token_url)
            .field("client_id", &"<redacted>")
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

// Provide base64 encoding for Basic auth without adding a dependency.
mod base64 {
    pub fn encode(input: String) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = input.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = match chunk.len() {
                1 => [chunk[0], 0, 0, 0],
                2 => [chunk[0], chunk[1], 0, 0],
                3 => [chunk[0], chunk[1], chunk[2], 0],
                _ => unreachable!(),
            };
            out.push(TABLE[(b[0] >> 2) as usize]);
            out.push(TABLE[(((b[0] & 0b11) << 4) | (b[1] >> 4)) as usize]);
            out.push(TABLE[(((b[1] & 0b1111) << 2) | (b[2] >> 6)) as usize]);
            out.push(TABLE[(b[2] & 0b111111) as usize]);
        }
        let rem = bytes.len() % 3;
        if rem != 0 {
            let pad = 3 - rem;
            let len = out.len();
            for c in &mut out[len - pad..] {
                *c = b'=';
            }
        }
        String::from_utf8(out).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, extract::State, http::StatusCode, routing::post, Json, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    #[derive(Clone)]
    struct MockState {
        requests: StdArc<AtomicUsize>,
    }

    async fn token_handler(
        State(s): State<MockState>,
        body: Bytes,
    ) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
        s.requests.fetch_add(1, Ordering::SeqCst);
        let text = String::from_utf8_lossy(&body);
        if text.contains("grant_type=client_credentials") {
            Ok((
                StatusCode::OK,
                Json(json!({
                    "access_token": "access-1",
                    "refresh_token": "refresh-1",
                    "expires_in": 2
                })),
            ))
        } else if text.contains("grant_type=refresh_token") {
            Ok((
                StatusCode::OK,
                Json(json!({
                    "access_token": "access-2",
                    "refresh_token": "refresh-2",
                    "expires_in": 2
                })),
            ))
        } else {
            Err(StatusCode::BAD_REQUEST)
        }
    }

    async fn mock_token_server() -> (u16, StdArc<AtomicUsize>) {
        let counter = StdArc::new(AtomicUsize::new(0));
        let state = MockState {
            requests: counter.clone(),
        };
        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, counter)
    }

    #[tokio::test]
    async fn get_token_fetches_and_caches() {
        let (port, counter) = mock_token_server().await;
        let cache = BitbucketTokenCache::new(
            "client-id".into(),
            "client-secret".into(),
            format!("http://127.0.0.1:{}/token", port),
        );

        let t1 = cache.get_token().await.unwrap();
        let t2 = cache.get_token().await.unwrap();
        assert_eq!(t1, "access-1");
        assert_eq!(t1, t2);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn get_token_refreshes_after_expiry() {
        let (port, counter) = mock_token_server().await;
        let cache = BitbucketTokenCache::new(
            "client-id".into(),
            "client-secret".into(),
            format!("http://127.0.0.1:{}/token", port),
        );

        let t1 = cache.get_token().await.unwrap();
        // Mock returns expires_in=2; the cache uses a 10% grace (0.2s) but clamps
        // to at least 1s, so the token is considered expired after ~1.8s.
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        let t2 = cache.get_token().await.unwrap();

        assert_eq!(t1, "access-1");
        assert_eq!(t2, "access-2");
        assert!(counter.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn get_token_is_concurrent_safe() {
        let (port, counter) = mock_token_server().await;
        let cache = BitbucketTokenCache::new(
            "client-id".into(),
            "client-secret".into(),
            format!("http://127.0.0.1:{}/token", port),
        );

        let mut handles = Vec::new();
        for _ in 0..10 {
            let c = cache.clone();
            handles.push(tokio::spawn(async move { c.get_token().await.unwrap() }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }

        assert!(results.iter().all(|r| r == "access-1"));
        // With the mutex, the mock should see only one token request.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn get_token_returns_error_on_invalid_credentials() {
        let (port, _counter) = mock_token_server_error().await;
        let cache = BitbucketTokenCache::new(
            "bad-id".into(),
            "bad-secret".into(),
            format!("http://127.0.0.1:{}/token", port),
        );

        let result = cache.get_token().await;
        assert!(result.is_err());
    }

    async fn mock_token_server_error() -> (u16, StdArc<AtomicUsize>) {
        use axum::{http::StatusCode, routing::post, Router};

        let counter = StdArc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new().route(
            "/token",
            post(move || async move {
                c.fetch_add(1, Ordering::SeqCst);
                StatusCode::UNAUTHORIZED
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, counter)
    }
}
