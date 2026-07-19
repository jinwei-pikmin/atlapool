use crate::config::BitbucketConfig;
use crate::upstream::{RequestBody, UpstreamClient, UpstreamError};
use reqwest::{header, Client, Method, Url};
use serde_json::json;

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
    pub fn get_repo_request(&self, repo_slug: &str) -> Result<reqwest::Request, UpstreamError> {
        self.request(
            Method::GET,
            &format!("/repositories/{}/{repo_slug}", self.workspace),
            RequestBody::None,
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
            RequestBody::None,
        )
    }

    #[allow(dead_code)]
    pub fn create_branch_request(
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
    }

    #[allow(dead_code)]
    pub fn create_commit_request(
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
    }

    #[allow(dead_code)]
    pub fn create_pull_request_request(
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
    }

    #[allow(dead_code)]
    pub fn create_repo_request(
        &self,
        repo_slug: &str,
        is_private: bool,
    ) -> Result<reqwest::Request, UpstreamError> {
        self.request(
            Method::POST,
            &format!("/repositories/{}/{repo_slug}", self.workspace),
            RequestBody::json(json!({ "is_private": is_private })),
        )
    }
}

impl UpstreamClient for BitbucketClient {
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

    #[test]
    fn create_branch_request_builds_correct_path_and_body() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client
            .create_branch_request("my-repo", "feature/new", "abc123")
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

    #[test]
    fn create_commit_request_builds_correct_form_body() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client
            .create_commit_request(
                "my-repo",
                "commit msg",
                "main",
                &["parent1".into()],
                &[("README.md".into(), "hello".into())],
            )
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

    #[test]
    fn create_pull_request_request_builds_correct_path_and_body() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client
            .create_pull_request_request(
                "my-repo",
                "PR title",
                "feature/src",
                Some("main"),
                Some("desc"),
            )
            .unwrap();

        assert_eq!(request.method(), Method::POST);
        assert_eq!(
            request.url().path(),
            "/repositories/WORK/my-repo/pullrequests"
        );
    }

    #[test]
    fn create_repo_request_defaults_to_private() {
        let client = BitbucketClient::new(&test_config()).unwrap();
        let request = client.create_repo_request("new-repo", true).unwrap();

        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.url().path(), "/repositories/WORK/new-repo");
    }
}
