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
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::Unsupported => write!(
                f,
                "unsupported secret reference format (supported: env:VAR_NAME, aws:secretsmanager:<secret-id>)"
            ),
            SecretError::MissingEnv(var) => write!(f, "env var '{}' is not set", var),
            SecretError::Aws(msg) => write!(f, "aws secrets manager error: {}", msg),
        }
    }
}

impl std::error::Error for SecretError {}

/// Backend for fetching secrets by id. Implemented by the real AWS Secrets
/// Manager client and by test mocks.
pub trait SecretBackend: Send + Sync {
    /// Fetch the plain-text value of a secret.
    fn get_secret(
        &self,
        secret_id: &str,
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
    async fn get_secret(&self, secret_id: &str) -> Result<String, SecretError> {
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

/// Resolve a secret reference string.
///
/// Supported formats:
///   - `env:VAR_NAME`
///   - `aws:secretsmanager:<secret-id>` (secret name or ARN)
pub async fn resolve<B: SecretBackend>(
    backend: &B,
    reference: &str,
) -> Result<String, SecretError> {
    if let Some(var) = reference.strip_prefix("env:") {
        return std::env::var(var).map_err(|_| SecretError::MissingEnv(var.to_string()));
    }

    if let Some(secret_id) = reference.strip_prefix("aws:secretsmanager:") {
        if secret_id.is_empty() {
            return Err(SecretError::Aws(
                "secret id is empty after 'aws:secretsmanager:'".into(),
            ));
        }
        return backend.get_secret(secret_id).await;
    }

    Err(SecretError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockBackend {
        result: Result<String, SecretError>,
    }

    impl SecretBackend for MockBackend {
        async fn get_secret(&self, _secret_id: &str) -> Result<String, SecretError> {
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
            result: Ok("unused".into()),
        };
        let result = resolve(&backend, "env:ATLAPOOL_TEST_MISSING_VAR").await;
        assert!(matches!(result, Err(SecretError::MissingEnv(_))));
    }

    #[tokio::test]
    async fn resolve_aws_ok() {
        let backend = MockBackend {
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
        let backend = MockBackend {
            result: Ok("unused".into()),
        };
        let result = resolve(&backend, "aws:secretsmanager:").await;
        assert!(matches!(result, Err(SecretError::Aws(_))));
    }

    #[tokio::test]
    async fn resolve_unsupported_prefix() {
        let backend = MockBackend {
            result: Ok("unused".into()),
        };
        let result = resolve(&backend, "gcp:secretmanager:foo").await;
        assert_eq!(result, Err(SecretError::Unsupported));
    }
}
