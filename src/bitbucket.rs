use crate::config::BitbucketConfig;
use crate::upstream::UpstreamClient;
use crate::upstream::UpstreamError;
use reqwest::{header, Client, Method, Url};
use serde_json::Value;

/// Bitbucket Cloud REST client. Caller headers are **not** trusted; every
/// request starts from an empty header set and receives the server-injected
/// `Authorization: Bearer <token>` header only.
#[derive(Clone)]
pub struct BitbucketClient {
    client: Client,
    base_url: String,
    #[allow(dead_code)]
    workspace: String,
    token: crate::secrets::SecretString,
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
        let token = config
            .token
            .clone()
            .ok_or(UpstreamError::MissingConfig("token"))?;
        Ok(Self {
            client: Client::new(),
            base_url,
            workspace,
            token,
        })
    }

    /// Build a request from an empty header set, injecting only the bearer token.
    pub fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Request, UpstreamError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let url = Url::parse(&url).map_err(|_| UpstreamError::InvalidUrl(url.clone()))?;

        let mut builder = self.client.request(method, url).header(
            header::AUTHORIZATION,
            format!("Bearer {}", self.token.expose_secret()),
        );

        if let Some(body) = body {
            builder = builder.json(&body);
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
    pub fn get_repo_request(&self, repo_slug: &str) -> Result<reqwest::Request, UpstreamError> {
        self.request(
            Method::GET,
            &format!("/repositories/{}/{repo_slug}", self.workspace),
            None,
        )
    }

    #[allow(dead_code)]
    pub fn get_pull_request_request(
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
            None,
        )
    }
}

impl UpstreamClient for BitbucketClient {
    fn build_request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Request, UpstreamError> {
        self.request(method, path, body)
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
        }
    }

    #[test]
    fn get_repo_request_injects_bearer_token() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client.get_repo_request("my-repo").unwrap();
        let headers = request.headers();

        let auth = headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
        assert_eq!(auth, "Bearer test-token");
    }

    #[test]
    fn get_repo_request_drops_caller_sensitive_headers() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client.get_repo_request("my-repo").unwrap();
        let headers = request.headers();

        assert!(!headers.contains_key(COOKIE));
        assert_eq!(headers.get_all(AUTHORIZATION).iter().count(), 1);
    }

    #[test]
    fn get_repo_request_builds_correct_path() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client.get_repo_request("my-repo").unwrap();

        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.url().path(), "/repositories/WORK/my-repo");
    }

    #[test]
    fn get_pull_request_request_builds_correct_path() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client.get_pull_request_request("my-repo", "42").unwrap();

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
        };
        assert!(BitbucketClient::new(&config).is_err());
    }

    #[test]
    fn new_requires_token() {
        let config = BitbucketConfig {
            base_url: None,
            workspace: Some("WORK".into()),
            token: None,
        };
        assert!(BitbucketClient::new(&config).is_err());
    }
}
