use serde::Deserialize;
use std::fmt;

/// A string that masks its value in `Debug` and `Display` to prevent
/// accidental credential leakage in logs or panic messages.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Access the raw secret value. Call sites should make it obvious that
    /// they are handling a credential and must not log or expose it.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SecretError {
    Unsupported,
    MissingEnv(String),
    Aws(String),
    Gcp(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::Unsupported => write!(
                f,
                "unsupported secret reference format (supported: env:VAR_NAME, aws:secretsmanager:<secret-id>, gcp:secretmanager:<project>/<secret>)"
            ),
            SecretError::MissingEnv(var) => write!(f, "env var '{}' is not set", var),
            SecretError::Aws(msg) => write!(f, "aws secrets manager error: {}", msg),
            SecretError::Gcp(msg) => write!(f, "gcp secret manager error: {}", msg),
        }
    }
}

impl std::error::Error for SecretError {}

/// Backend for fetching secrets by reference. Implemented by the real cloud
/// clients and by test mocks. The reference passed to `get_secret` includes the
/// backend prefix (e.g. `aws:secretsmanager:...` or `gcp:secretmanager:...`).
pub trait SecretBackend: Send + Sync {
    /// Fetch the plain-text value of a secret reference.
    fn get_secret(
        &self,
        reference: &str,
    ) -> impl std::future::Future<Output = Result<String, SecretError>> + Send;
}

/// AWS Secrets Manager backend using the standard AWS SDK credential chain.
#[derive(Clone)]
pub struct AwsSecretsManager {
    client: aws_sdk_secretsmanager::Client,
}

impl AwsSecretsManager {
    /// Load AWS configuration from the environment and create a Secrets Manager
    /// client. Fails if the AWS region cannot be determined, so the server
    /// exits early (fail-closed) instead of trying to call an invalid endpoint.
    pub async fn new() -> Result<Self, SecretError> {
        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;

        if sdk_config.region().is_none() {
            return Err(SecretError::Aws(
                "AWS region is not set (env:AWS_REGION or ~/.aws/config)".into(),
            ));
        }

        Ok(Self {
            client: aws_sdk_secretsmanager::Client::new(&sdk_config),
        })
    }
}

impl SecretBackend for AwsSecretsManager {
    async fn get_secret(&self, reference: &str) -> Result<String, SecretError> {
        let secret_id = reference
            .strip_prefix("aws:secretsmanager:")
            .ok_or_else(|| {
                SecretError::Aws(format!(
                    "expected aws:secretsmanager: reference, got '{}'",
                    reference
                ))
            })?;

        if secret_id.is_empty() {
            return Err(SecretError::Aws(
                "secret id is empty after 'aws:secretsmanager:'".into(),
            ));
        }

        let response = self
            .client
            .get_secret_value()
            .secret_id(secret_id)
            .send()
            .await
            .map_err(|e| {
                SecretError::Aws(format!("failed to get secret '{}': {}", secret_id, e))
            })?;

        response
            .secret_string
            .ok_or_else(|| SecretError::Aws(format!("secret '{}' has no string value", secret_id)))
    }
}

/// Lazily creates an `AwsSecretsManager` only when an `aws:secretsmanager:`
/// reference is actually encountered. This keeps `env:`-only or `gcp:`-only
/// deployments free from any AWS dependency or configuration requirement.
pub struct LazyAwsBackend {
    inner: tokio::sync::OnceCell<Result<AwsSecretsManager, SecretError>>,
}

impl LazyAwsBackend {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::OnceCell::new(),
        }
    }
}

impl SecretBackend for LazyAwsBackend {
    async fn get_secret(&self, reference: &str) -> Result<String, SecretError> {
        let secret_id = reference
            .strip_prefix("aws:secretsmanager:")
            .ok_or(SecretError::Unsupported)?;

        if secret_id.is_empty() {
            return Err(SecretError::Aws(
                "secret id is empty after 'aws:secretsmanager:'".into(),
            ));
        }

        let init = self
            .inner
            .get_or_init(|| async { AwsSecretsManager::new().await })
            .await;

        match init {
            Ok(manager) => manager.get_secret(reference).await,
            Err(err) => Err(err.clone()),
        }
    }
}

/// GCP Secret Manager backend using Application Default Credentials.
#[derive(Clone)]
pub struct GcpSecretsManager {
    client: google_cloud_secretmanager_v1::client::SecretManagerService,
}

impl GcpSecretsManager {
    /// Create a Secret Manager client using Application Default Credentials.
    pub async fn new() -> Result<Self, SecretError> {
        let client = google_cloud_secretmanager_v1::client::SecretManagerService::builder()
            .build()
            .await
            .map_err(|e| {
                SecretError::Gcp(format!("failed to create GCP Secret Manager client: {}", e))
            })?;

        Ok(Self { client })
    }

    fn parse_name(resource: &str) -> Result<String, SecretError> {
        if resource.starts_with("projects/") {
            if resource.contains("/versions/") {
                return Ok(resource.to_string());
            }
            return Ok(format!("{}/versions/latest", resource));
        }

        if resource.matches('/').count() != 1 {
            return Err(SecretError::Gcp(format!(
                "invalid gcp:secretmanager: reference '{}': expected '<project>/<secret>' or 'projects/<project>/secrets/<secret>/versions/<version>'",
                resource
            )));
        }

        let (project, secret) = resource.split_once('/').expect("one slash");
        if project.is_empty() || secret.is_empty() {
            return Err(SecretError::Gcp(
                "empty project or secret in gcp:secretmanager: reference".into(),
            ));
        }

        Ok(format!(
            "projects/{}/secrets/{}/versions/latest",
            project, secret
        ))
    }
}

impl SecretBackend for GcpSecretsManager {
    async fn get_secret(&self, reference: &str) -> Result<String, SecretError> {
        let resource = reference
            .strip_prefix("gcp:secretmanager:")
            .ok_or_else(|| {
                SecretError::Gcp(format!(
                    "expected gcp:secretmanager: reference, got '{}'",
                    reference
                ))
            })?;

        if resource.is_empty() {
            return Err(SecretError::Gcp(
                "resource is empty after 'gcp:secretmanager:'".into(),
            ));
        }

        let name = Self::parse_name(resource)?;

        let response = self
            .client
            .access_secret_version()
            .set_name(name)
            .send()
            .await
            .map_err(|e| {
                SecretError::Gcp(format!("failed to access secret '{}': {}", resource, e))
            })?;

        let payload = response
            .payload
            .ok_or_else(|| SecretError::Gcp(format!("secret '{}' has no payload", resource)))?;

        String::from_utf8(payload.data.to_vec()).map_err(|e| {
            SecretError::Gcp(format!(
                "secret '{}' payload is not valid UTF-8: {}",
                resource, e
            ))
        })
    }
}

/// Lazily creates a `GcpSecretsManager` only when a `gcp:secretmanager:`
/// reference is actually encountered.
pub struct LazyGcpBackend {
    inner: tokio::sync::OnceCell<Result<GcpSecretsManager, SecretError>>,
}

impl LazyGcpBackend {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::OnceCell::new(),
        }
    }
}

impl SecretBackend for LazyGcpBackend {
    async fn get_secret(&self, reference: &str) -> Result<String, SecretError> {
        let resource = reference
            .strip_prefix("gcp:secretmanager:")
            .ok_or(SecretError::Unsupported)?;

        if resource.is_empty() {
            return Err(SecretError::Gcp(
                "resource is empty after 'gcp:secretmanager:'".into(),
            ));
        }

        let init = self
            .inner
            .get_or_init(|| async { GcpSecretsManager::new().await })
            .await;

        match init {
            Ok(manager) => manager.get_secret(reference).await,
            Err(err) => Err(err.clone()),
        }
    }
}

/// Dispatches `aws:secretsmanager:` references to the AWS backend and
/// `gcp:secretmanager:` references to the GCP backend. Both inner backends are
/// lazily initialized when their respective prefix is first seen.
pub struct MultiBackend<A, G> {
    aws: A,
    gcp: G,
}

impl<A, G> MultiBackend<A, G>
where
    A: SecretBackend,
    G: SecretBackend,
{
    pub fn with_backends(aws: A, gcp: G) -> Self {
        Self { aws, gcp }
    }
}

impl<A, G> SecretBackend for MultiBackend<A, G>
where
    A: SecretBackend,
    G: SecretBackend,
{
    async fn get_secret(&self, reference: &str) -> Result<String, SecretError> {
        if reference.starts_with("aws:secretsmanager:") {
            return self.aws.get_secret(reference).await;
        }
        if reference.starts_with("gcp:secretmanager:") {
            return self.gcp.get_secret(reference).await;
        }
        Err(SecretError::Unsupported)
    }
}

/// Convenience alias for the production multi-cloud backend with lazy
/// initialization.
pub type LazyMultiBackend = MultiBackend<LazyAwsBackend, LazyGcpBackend>;

impl LazyMultiBackend {
    pub fn new() -> Self {
        Self::with_backends(LazyAwsBackend::new(), LazyGcpBackend::new())
    }
}

/// Returns `true` if the value is a secret reference that should be resolved
/// rather than used as a literal credential.
pub fn is_secret_reference(value: &str) -> bool {
    value.starts_with("env:")
        || value.starts_with("aws:secretsmanager:")
        || value.starts_with("gcp:secretmanager:")
}

/// Resolve a secret reference string.
///
/// Supported formats:
///   - `env:VAR_NAME`
///   - `aws:secretsmanager:<secret-id>` (secret name or ARN)
///   - `gcp:secretmanager:<project>/<secret>` (latest version)
///   - `gcp:secretmanager:projects/<project>/secrets/<secret>/versions/<version>`
pub async fn resolve<B: SecretBackend>(
    backend: &B,
    reference: &str,
) -> Result<String, SecretError> {
    if let Some(var) = reference.strip_prefix("env:") {
        return std::env::var(var).map_err(|_| SecretError::MissingEnv(var.to_string()));
    }

    backend.get_secret(reference).await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockBackend {
        prefix: &'static str,
        result: Result<String, SecretError>,
    }

    impl SecretBackend for MockBackend {
        async fn get_secret(&self, reference: &str) -> Result<String, SecretError> {
            if !reference.starts_with(self.prefix) {
                return Err(SecretError::Unsupported);
            }
            self.result.clone()
        }
    }

    #[test]
    fn secret_string_debug_and_display_redact() {
        let secret = SecretString::new("top-secret");
        assert_eq!(format!("{:?}", secret), "<redacted>");
        assert_eq!(format!("{}", secret), "<redacted>");
        assert_eq!(secret.expose_secret(), "top-secret");
    }

    #[tokio::test]
    async fn resolve_env_ok() {
        std::env::set_var("ATLAPOOL_TEST_SECRET", "s3cr3t");
        let backend = MockBackend {
            prefix: "",
            result: Ok("unused".into()),
        };
        assert_eq!(
            resolve(&backend, "env:ATLAPOOL_TEST_SECRET").await.unwrap(),
            "s3cr3t"
        );
        std::env::remove_var("ATLAPOOL_TEST_SECRET");
    }

    #[tokio::test]
    async fn resolve_env_missing() {
        let backend = MockBackend {
            prefix: "",
            result: Ok("unused".into()),
        };
        let result = resolve(&backend, "env:ATLAPOOL_TEST_MISSING_VAR").await;
        assert!(matches!(result, Err(SecretError::MissingEnv(_))));
    }

    #[tokio::test]
    async fn resolve_aws_ok() {
        let backend = MockBackend {
            prefix: "aws:secretsmanager:",
            result: Ok("aws-secret-value".into()),
        };
        assert_eq!(
            resolve(&backend, "aws:secretsmanager:prod/atlassian/token")
                .await
                .unwrap(),
            "aws-secret-value"
        );
    }

    #[tokio::test]
    async fn resolve_aws_arn_with_colons() {
        let backend = MockBackend {
            prefix: "aws:secretsmanager:",
            result: Ok("arn-secret".into()),
        };
        assert_eq!(
            resolve(
                &backend,
                "aws:secretsmanager:arn:aws:secretsmanager:us-east-1:123456789:secret:my-secret"
            )
            .await
            .unwrap(),
            "arn-secret"
        );
    }

    #[tokio::test]
    async fn resolve_aws_missing_secret() {
        let backend = MockBackend {
            prefix: "aws:secretsmanager:",
            result: Err(SecretError::Aws("ResourceNotFoundException".into())),
        };
        let result = resolve(&backend, "aws:secretsmanager:missing-secret").await;
        assert_eq!(
            result,
            Err(SecretError::Aws("ResourceNotFoundException".into()))
        );
    }

    #[tokio::test]
    async fn resolve_aws_empty_id() {
        let backend = LazyAwsBackend::new();
        let result = resolve(&backend, "aws:secretsmanager:").await;
        assert!(matches!(result, Err(SecretError::Aws(_))));
    }

    #[tokio::test]
    async fn resolve_gcp_ok() {
        let backend = MockBackend {
            prefix: "gcp:secretmanager:",
            result: Ok("gcp-secret-value".into()),
        };
        assert_eq!(
            resolve(&backend, "gcp:secretmanager:my-project/my-secret")
                .await
                .unwrap(),
            "gcp-secret-value"
        );
    }

    #[tokio::test]
    async fn resolve_gcp_full_resource_name() {
        let backend = MockBackend {
            prefix: "gcp:secretmanager:",
            result: Ok("gcp-full-secret".into()),
        };
        assert_eq!(
            resolve(
                &backend,
                "gcp:secretmanager:projects/my-project/secrets/my-secret/versions/1"
            )
            .await
            .unwrap(),
            "gcp-full-secret"
        );
    }

    #[tokio::test]
    async fn resolve_gcp_missing_secret() {
        let backend = MockBackend {
            prefix: "gcp:secretmanager:",
            result: Err(SecretError::Gcp("NotFound".into())),
        };
        let result = resolve(&backend, "gcp:secretmanager:my-project/missing-secret").await;
        assert_eq!(result, Err(SecretError::Gcp("NotFound".into())));
    }

    #[tokio::test]
    async fn resolve_gcp_empty_reference() {
        let backend = LazyGcpBackend::new();
        let result = resolve(&backend, "gcp:secretmanager:").await;
        assert!(matches!(result, Err(SecretError::Gcp(_))));
    }

    #[tokio::test]
    async fn resolve_unsupported_prefix() {
        let backend = MultiBackend::with_backends(
            MockBackend {
                prefix: "aws:secretsmanager:",
                result: Err(SecretError::Unsupported),
            },
            MockBackend {
                prefix: "gcp:secretmanager:",
                result: Err(SecretError::Unsupported),
            },
        );
        let result = resolve(&backend, "vault:foo").await;
        assert_eq!(result, Err(SecretError::Unsupported));
    }

    #[test]
    fn gcp_parse_name_project_secret() {
        let name = GcpSecretsManager::parse_name("my-project/my-secret").unwrap();
        assert_eq!(
            name,
            "projects/my-project/secrets/my-secret/versions/latest"
        );
    }

    #[test]
    fn gcp_parse_name_full_resource_without_version() {
        let name = GcpSecretsManager::parse_name("projects/my-project/secrets/my-secret").unwrap();
        assert_eq!(
            name,
            "projects/my-project/secrets/my-secret/versions/latest"
        );
    }

    #[test]
    fn gcp_parse_name_full_resource_with_version() {
        let name =
            GcpSecretsManager::parse_name("projects/my-project/secrets/my-secret/versions/3")
                .unwrap();
        assert_eq!(name, "projects/my-project/secrets/my-secret/versions/3");
    }

    #[test]
    fn gcp_parse_name_invalid() {
        assert!(GcpSecretsManager::parse_name("too/many/parts").is_err());
        assert!(GcpSecretsManager::parse_name("missing-slash").is_err());
        assert!(GcpSecretsManager::parse_name("/empty-project").is_err());
        assert!(GcpSecretsManager::parse_name("empty-secret/").is_err());
    }
}
