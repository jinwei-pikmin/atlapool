use crate::config::AtlassianConfig;
use reqwest::{header, Client, Method, Url};
use serde_json::Value;

#[derive(Debug)]
pub enum UpstreamError {
    MissingConfig(&'static str),
    InvalidUrl(String),
    RequestBuild(reqwest::Error),
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::MissingConfig(field) => {
                write!(f, "missing atlassian config: {}", field)
            }
            UpstreamError::InvalidUrl(url) => write!(f, "invalid upstream url: {}", url),
            UpstreamError::RequestBuild(err) => write!(f, "failed to build request: {}", err),
        }
    }
}

impl std::error::Error for UpstreamError {}

/// Upstream Jira client. Caller headers are **not** trusted; every request
/// starts from an empty header set and receives the server-injected
/// `Authorization: Bearer <token>` header only.
#[derive(Clone)]
pub struct JiraClient {
    client: Client,
    base_url: String,
    token: crate::secrets::SecretString,
}

impl JiraClient {
    /// Build the Jira API base URL. Prefer the `api.atlassian.com` gateway using
    /// `cloud_id`; fall back to `base_url` for tests or legacy configs.
    fn api_base(config: &AtlassianConfig) -> Result<String, UpstreamError> {
        if let Some(cloud_id) = &config.cloud_id {
            Ok(format!("https://api.atlassian.com/ex/jira/{cloud_id}"))
        } else if let Some(base_url) = &config.base_url {
            Ok(base_url.trim_end_matches('/').to_string())
        } else {
            Err(UpstreamError::MissingConfig("cloud_id or base_url"))
        }
    }

    pub fn new(config: &AtlassianConfig) -> Result<Self, UpstreamError> {
        let base_url = Self::api_base(config)?;
        let token = config
            .token
            .clone()
            .ok_or(UpstreamError::MissingConfig("token"))?;
        Ok(Self {
            client: Client::new(),
            base_url,
            token,
        })
    }

    /// Build a request from an empty header set, injecting only the bearer token.
    pub fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
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
    pub fn myself_request(&self) -> Result<reqwest::Request, UpstreamError> {
        self.request(Method::GET, "/rest/api/3/myself", None)
    }

    #[allow(dead_code)]
    pub fn get_issue_request(&self, issue_key: &str) -> Result<reqwest::Request, UpstreamError> {
        self.request(Method::GET, &format!("/rest/api/3/issue/{issue_key}"), None)
    }
}

/// Abstraction over Jira and Confluence clients so `mcp_handler` can forward
/// requests without knowing the concrete upstream type.
pub trait UpstreamClient: Send + Sync {
    fn build_request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Request, UpstreamError>;

    async fn execute(&self, request: reqwest::Request)
        -> Result<reqwest::Response, reqwest::Error>;
}

impl UpstreamClient for JiraClient {
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

    fn test_config() -> AtlassianConfig {
        AtlassianConfig {
            base_url: Some("https://example.atlassian.net".into()),
            email: None,
            cloud_id: None,
            token: Some(SecretString::new("test-token")),
        }
    }

    fn cloud_id_config() -> AtlassianConfig {
        AtlassianConfig {
            base_url: None,
            email: None,
            cloud_id: Some("test-cloud-id".into()),
            token: Some(SecretString::new("test-token")),
        }
    }

    #[test]
    fn myself_request_injects_bearer_token() {
        let client = JiraClient::new(&test_config()).unwrap();
        let request = client.myself_request().unwrap();
        let headers = request.headers();

        let auth = headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
        assert_eq!(auth, "Bearer test-token");
    }

    #[test]
    fn myself_request_drops_caller_sensitive_headers() {
        let client = JiraClient::new(&test_config()).unwrap();
        let request = client.myself_request().unwrap();
        let headers = request.headers();

        // The request must not carry any cookie from the caller.
        assert!(!headers.contains_key(COOKIE));
        // The only Authorization header is the one injected by the server.
        assert_eq!(headers.get_all(AUTHORIZATION).iter().count(), 1);
    }

    #[test]
    fn myself_request_url_points_to_jira_myself() {
        let client = JiraClient::new(&test_config()).unwrap();
        let request = client.myself_request().unwrap();

        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.url().path(), "/rest/api/3/myself");
    }

    #[test]
    fn myself_request_url_uses_cloud_id_gateway() {
        let client = JiraClient::new(&cloud_id_config()).unwrap();
        let request = client.myself_request().unwrap();

        assert_eq!(request.url().host_str(), Some("api.atlassian.com"));
        assert_eq!(
            request.url().path(),
            "/ex/jira/test-cloud-id/rest/api/3/myself"
        );
    }

    #[test]
    fn get_issue_request_builds_correct_path() {
        let client = JiraClient::new(&test_config()).unwrap();
        let request = client.get_issue_request("PROJ-123").unwrap();

        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.url().path(), "/rest/api/3/issue/PROJ-123");
        let auth = request
            .headers()
            .get(AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer test-token");
    }
}
