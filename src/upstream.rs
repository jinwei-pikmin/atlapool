use crate::config::AtlassianConfig;
use reqwest::{header, Client, Method, Url};

#[derive(Debug)]
#[allow(dead_code)]
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
/// `Authorization` header only.
#[allow(dead_code)]
pub struct JiraClient {
    client: Client,
    base_url: String,
    token: crate::secrets::SecretString,
}

#[allow(dead_code)]
impl JiraClient {
    pub fn new(config: &AtlassianConfig) -> Result<Self, UpstreamError> {
        let base_url = config
            .base_url
            .clone()
            .ok_or(UpstreamError::MissingConfig("base_url"))?;
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

    /// Build a `GET /rest/api/3/myself` request for smoke-testing the proxy
    /// chain. F2-a uses Bearer injection; the exact Atlassian auth scheme
    /// (Basic/OAuth) will be aligned in F2-b.
    pub fn myself_request(&self) -> Result<reqwest::Request, UpstreamError> {
        let url = format!("{}/rest/api/3/myself", self.base_url.trim_end_matches('/'));
        let url = Url::parse(&url).map_err(|_| UpstreamError::InvalidUrl(url.clone()))?;

        self.client
            .request(Method::GET, url)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", self.token.expose_secret()),
            )
            .build()
            .map_err(UpstreamError::RequestBuild)
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
}
