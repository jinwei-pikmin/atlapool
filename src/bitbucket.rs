use crate::bitbucket_token::BitbucketTokenCache;
use crate::config::BitbucketConfig;
use crate::upstream::{RequestBody, UpstreamClient, UpstreamError};
use reqwest::{header, Client, Method, Url};
use serde_json::json;
use std::sync::Arc;

/// Provider for the bearer token used on outgoing Bitbucket API requests.
#[derive(Clone)]
enum TokenProvider {
    /// A long-lived token configured directly in `[bitbucket].token`.
    Static(crate::secrets::SecretString),
    /// A short-lived OAuth 2.0 token maintained by the token cache.
    OAuth(Arc<BitbucketTokenCache>),
}

/// Bitbucket Cloud REST client. Caller headers are **not** trusted; every
/// request starts from an empty header set and receives the server-injected
/// `Authorization: Bearer <token>` header only.
#[derive(Clone)]
pub struct BitbucketClient {
    client: Client,
    base_url: String,
    #[allow(dead_code)]
    workspace: String,
    token_provider: TokenProvider,
}

impl BitbucketClient {
    fn api_base(config: &BitbucketConfig) -> Result<String, UpstreamError> {
        if let Some(base_url) = &config.base_url {
            Ok(base_url.trim_end_matches('/').to_string())
        } else {
            Ok("https://api.bitbucket.org/2.0".to_string())
        }
    }

    pub fn new(config: &BitbucketConfig) -> Result<Self, UpstreamError> {
        let base_url = Self::api_base(config)?;
        let workspace = config
            .workspace
            .clone()
            .ok_or(UpstreamError::MissingConfig("workspace"))?;

        let token_provider = if let Some(ref oauth) = config.oauth {
            let client_id = oauth
                .client_id
                .as_ref()
                .map(|s| s.expose_secret().to_string())
                .ok_or(UpstreamError::MissingConfig("bitbucket.oauth.client_id"))?;
            let client_secret = oauth
                .client_secret
                .as_ref()
                .map(|s| s.expose_secret().to_string())
                .ok_or(UpstreamError::MissingConfig(
                    "bitbucket.oauth.client_secret",
                ))?;
            TokenProvider::OAuth(Arc::new(BitbucketTokenCache::new(
                client_id,
                client_secret,
                oauth.token_url.clone(),
            )))
        } else {
            let token = config
                .token
                .clone()
                .ok_or(UpstreamError::MissingConfig("token"))?;
            TokenProvider::Static(token)
        };

        // Use the default reqwest client. Its default redirect policy strips
        // sensitive headers such as `Authorization` when following cross-host
        // redirects (e.g. Bitbucket diff endpoints that redirect to a CDN).
        // This security property is pinned by
        // `mcp_bitbucket_get_pull_request_diff_strips_auth_on_cross_host_redirect`.
        Ok(Self {
            client: Client::new(),
            base_url,
            workspace,
            token_provider,
        })
    }

    async fn auth_token(&self) -> Result<String, UpstreamError> {
        match &self.token_provider {
            TokenProvider::Static(token) => Ok(token.expose_secret().to_string()),
            TokenProvider::OAuth(cache) => cache.get_token().await,
        }
    }

    /// Build a request from an empty header set, injecting only the bearer token.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: RequestBody,
    ) -> Result<reqwest::Request, UpstreamError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let url = Url::parse(&url).map_err(|_| UpstreamError::InvalidUrl(url.clone()))?;

        let token = self.auth_token().await?;
        let mut builder = self
            .client
            .request(method, url)
            .header(header::AUTHORIZATION, format!("Bearer {}", token));

        match body {
            RequestBody::None => {}
            RequestBody::Json(v) => builder = builder.json(&v),
            RequestBody::Form(fields) => builder = builder.form(&fields),
        }

        builder.build().map_err(UpstreamError::RequestBuild)
    }

    pub async fn send(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client.execute(request).await
    }

    #[allow(dead_code)]
    pub async fn get_repo_request(
        &self,
        repo_slug: &str,
    ) -> Result<reqwest::Request, UpstreamError> {
        self.request(
            Method::GET,
            &format!("/repositories/{}/{repo_slug}", self.workspace),
            RequestBody::None,
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn get_pull_request_request(
        &self,
        repo_slug: &str,
        pull_request_id: &str,
    ) -> Result<reqwest::Request, UpstreamError> {
        self.request(
            Method::GET,
            &format!(
                "/repositories/{}/{repo_slug}/pullrequests/{pull_request_id}",
                self.workspace
            ),
            RequestBody::None,
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn create_branch_request(
        &self,
        repo_slug: &str,
        branch_name: &str,
        target_hash: &str,
    ) -> Result<reqwest::Request, UpstreamError> {
        self.request(
            Method::POST,
            &format!("/repositories/{}/{repo_slug}/refs/branches", self.workspace),
            RequestBody::json(json!({
                "name": branch_name,
                "target": { "hash": target_hash }
            })),
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn create_commit_request(
        &self,
        repo_slug: &str,
        message: &str,
        branch: &str,
        parents: &[String],
        files: &[(String, String)],
    ) -> Result<reqwest::Request, UpstreamError> {
        let mut fields: Vec<(String, String)> = Vec::new();
        fields.push(("message".into(), message.into()));
        fields.push(("branch".into(), branch.into()));
        for parent in parents {
            fields.push(("parents".into(), parent.clone()));
        }
        for (path, content) in files {
            fields.push((path.clone(), content.clone()));
        }
        self.request(
            Method::POST,
            &format!("/repositories/{}/{repo_slug}/src", self.workspace),
            RequestBody::form(fields),
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn create_pull_request_request(
        &self,
        repo_slug: &str,
        title: &str,
        source_branch: &str,
        destination_branch: Option<&str>,
        description: Option<&str>,
    ) -> Result<reqwest::Request, UpstreamError> {
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
        self.request(
            Method::POST,
            &format!("/repositories/{}/{repo_slug}/pullrequests", self.workspace),
            RequestBody::json(body),
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn create_repo_request(
        &self,
        repo_slug: &str,
        is_private: bool,
    ) -> Result<reqwest::Request, UpstreamError> {
        self.request(
            Method::POST,
            &format!("/repositories/{}/{repo_slug}", self.workspace),
            RequestBody::json(json!({ "is_private": is_private })),
        )
        .await
    }
}

impl UpstreamClient for BitbucketClient {
    async fn build_request(
        &self,
        method: Method,
        path: &str,
        body: RequestBody,
    ) -> Result<reqwest::Request, UpstreamError> {
        self.request(method, path, body).await
    }

    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.send(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretString;
    use reqwest::header::{AUTHORIZATION, COOKIE};

    fn test_config() -> BitbucketConfig {
        BitbucketConfig {
            base_url: Some("https://example.bitbucket.org".into()),
            workspace: Some("WORK".into()),
            token: Some(SecretString::new("test-token")),
            oauth: None,
        }
    }

    fn oauth_test_config(token_url: &str) -> BitbucketConfig {
        BitbucketConfig {
            base_url: Some("https://example.bitbucket.org".into()),
            workspace: Some("WORK".into()),
            token: None,
            oauth: Some(crate::config::BitbucketOAuthConfig {
                client_id: Some(SecretString::new("client-id")),
                client_secret: Some(SecretString::new("client-secret")),
                token_url: token_url.into(),
            }),
        }
    }

    async fn mock_token_server() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::{body::Bytes, http::StatusCode, routing::post, Json, Router};
        use serde_json::json;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let app = Router::new().route(
            "/token",
            post(move |body: Bytes| async move {
                c.fetch_add(1, Ordering::SeqCst);
                let text = String::from_utf8_lossy(&body);
                if text.contains("grant_type=client_credentials") {
                    Ok::<_, StatusCode>((
                        StatusCode::OK,
                        Json(json!({
                            "access_token": "oauth-access-1",
                            "refresh_token": "refresh-1",
                            "expires_in": 2
                        })),
                    ))
                } else if text.contains("grant_type=refresh_token") {
                    Ok((
                        StatusCode::OK,
                        Json(json!({
                            "access_token": "oauth-access-2",
                            "refresh_token": "refresh-2",
                            "expires_in": 2
                        })),
                    ))
                } else {
                    Err(StatusCode::BAD_REQUEST)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, counter)
    }

    #[tokio::test]
    async fn get_repo_request_injects_bearer_token() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client.get_repo_request("my-repo").await.unwrap();
        let headers = request.headers();

        let auth = headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
        assert_eq!(auth, "Bearer test-token");
    }

    #[tokio::test]
    async fn get_repo_request_drops_caller_sensitive_headers() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client.get_repo_request("my-repo").await.unwrap();
        let headers = request.headers();

        assert!(!headers.contains_key(COOKIE));
        assert_eq!(headers.get_all(AUTHORIZATION).iter().count(), 1);
    }

    #[tokio::test]
    async fn get_repo_request_builds_correct_path() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client.get_repo_request("my-repo").await.unwrap();

        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.url().path(), "/repositories/WORK/my-repo");
    }

    #[tokio::test]
    async fn get_pull_request_request_builds_correct_path() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client
            .get_pull_request_request("my-repo", "42")
            .await
            .unwrap();

        assert_eq!(request.method(), Method::GET);
        assert_eq!(
            request.url().path(),
            "/repositories/WORK/my-repo/pullrequests/42"
        );
    }

    #[test]
    fn new_requires_workspace() {
        let config = BitbucketConfig {
            base_url: None,
            workspace: None,
            token: Some(SecretString::new("test-token")),
            oauth: None,
        };
        assert!(BitbucketClient::new(&config).is_err());
    }

    #[test]
    fn new_requires_token_when_oauth_missing() {
        let config = BitbucketConfig {
            base_url: None,
            workspace: Some("WORK".into()),
            token: None,
            oauth: None,
        };
        assert!(BitbucketClient::new(&config).is_err());
    }

    #[tokio::test]
    async fn oauth_request_fetches_and_injects_bearer_token() {
        let (port, counter) = mock_token_server().await;
        let token_url = format!("http://127.0.0.1:{}/token", port);
        let client = BitbucketClient::new(&oauth_test_config(&token_url)).unwrap();

        let request = client.get_repo_request("my-repo").await.unwrap();
        let auth = request
            .headers()
            .get(AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer oauth-access-1");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn oauth_request_reuses_cached_token() {
        let (port, counter) = mock_token_server().await;
        let token_url = format!("http://127.0.0.1:{}/token", port);
        let client = BitbucketClient::new(&oauth_test_config(&token_url)).unwrap();

        let _ = client.get_repo_request("my-repo").await.unwrap();
        let _ = client.get_repo_request("my-repo").await.unwrap();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn create_branch_request_builds_correct_path_and_body() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client
            .create_branch_request("my-repo", "feature/new", "abc123")
            .await
            .unwrap();

        assert_eq!(request.method(), Method::POST);
        assert_eq!(
            request.url().path(),
            "/repositories/WORK/my-repo/refs/branches"
        );
        assert_eq!(
            request.headers().get("content-type").unwrap(),
            "application/json"
        );
    }

    #[tokio::test]
    async fn create_commit_request_builds_correct_form_body() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client
            .create_commit_request(
                "my-repo",
                "commit msg",
                "main",
                &["parent1".into()],
                &[("README.md".into(), "hello".into())],
            )
            .await
            .unwrap();

        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.url().path(), "/repositories/WORK/my-repo/src");
        assert!(request
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/x-www-form-urlencoded"));
    }

    #[tokio::test]
    async fn create_pull_request_request_builds_correct_path_and_body() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client
            .create_pull_request_request(
                "my-repo",
                "PR title",
                "feature/src",
                Some("main"),
                Some("desc"),
            )
            .await
            .unwrap();

        assert_eq!(request.method(), Method::POST);
        assert_eq!(
            request.url().path(),
            "/repositories/WORK/my-repo/pullrequests"
        );
    }

    #[tokio::test]
    async fn create_repo_request_defaults_to_private() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client.create_repo_request("new-repo", true).await.unwrap();

        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.url().path(), "/repositories/WORK/new-repo");
    }
}
