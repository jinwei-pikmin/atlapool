use crate::config::AtlassianConfig;
use crate::upstream::{RequestBody, UpstreamClient, UpstreamError};
use reqwest::{header, Client, Method, Url};

/// Confluence Cloud REST client. Caller headers are **not** trusted; every
/// request starts from an empty header set and receives the server-injected
/// `Authorization: Bearer <token>` header only.
#[derive(Clone)]
pub struct ConfluenceClient {
    client: Client,
    base_url: String,
    token: crate::secrets::SecretString,
}

impl ConfluenceClient {
    /// Build the Confluence API base URL. Prefer the `api.atlassian.com` gateway
    /// using `cloud_id`; fall back to `base_url` for tests or legacy configs.
    fn api_base(config: &AtlassianConfig) -> Result<String, UpstreamError> {
        if let Some(cloud_id) = &config.cloud_id {
            Ok(format!(
                "https://api.atlassian.com/ex/confluence/{cloud_id}"
            ))
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
        body: RequestBody,
    ) -> Result<reqwest::Request, UpstreamError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let url = Url::parse(&url).map_err(|_| UpstreamError::InvalidUrl(url.clone()))?;

        let mut builder = self.client.request(method, url).header(
            header::AUTHORIZATION,
            format!("Bearer {}", self.token.expose_secret()),
        );

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
    pub fn get_page_request(&self, page_id: &str) -> Result<reqwest::Request, UpstreamError> {
        self.request(
            Method::GET,
            &format!("/wiki/api/v2/pages/{page_id}?body-format=view"),
            RequestBody::None,
        )
    }
}

impl UpstreamClient for ConfluenceClient {
    fn build_request(
        &self,
        method: Method,
        path: &str,
        body: RequestBody,
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

            cloud_id: None,
            token: Some(SecretString::new("test-token")),
        }
    }

    fn cloud_id_config() -> AtlassianConfig {
        AtlassianConfig {
            base_url: None,

            cloud_id: Some("test-cloud-id".into()),
            token: Some(SecretString::new("test-token")),
        }
    }

    #[test]
    fn get_page_request_injects_bearer_token() {
        let client = ConfluenceClient::new(&test_config()).unwrap();
        let request = client.get_page_request("12345").unwrap();
        let headers = request.headers();

        let auth = headers.get(AUTHORIZATION).unwrap().to_str().unwrap();
        assert_eq!(auth, "Bearer test-token");
    }

    #[test]
    fn get_page_request_drops_caller_sensitive_headers() {
        let client = ConfluenceClient::new(&test_config()).unwrap();
        let request = client.get_page_request("12345").unwrap();
        let headers = request.headers();

        assert!(!headers.contains_key(COOKIE));
        assert_eq!(headers.get_all(AUTHORIZATION).iter().count(), 1);
    }

    #[test]
    fn get_page_request_builds_correct_path() {
        let client = ConfluenceClient::new(&test_config()).unwrap();
        let request = client.get_page_request("12345").unwrap();

        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.url().path(), "/wiki/api/v2/pages/12345");
        let query: Vec<_> = request.url().query().unwrap().split('&').collect();
        assert!(query.contains(&"body-format=view"));
    }

    #[test]
    fn get_page_request_url_uses_cloud_id_gateway() {
        let client = ConfluenceClient::new(&cloud_id_config()).unwrap();
        let request = client.get_page_request("12345").unwrap();

        assert_eq!(request.url().host_str(), Some("api.atlassian.com"));
        assert_eq!(
            request.url().path(),
            "/ex/confluence/test-cloud-id/wiki/api/v2/pages/12345"
        );
    }
}
