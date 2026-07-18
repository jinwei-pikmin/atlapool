use std::fmt;

#[derive(Debug, PartialEq)]
pub enum SecretError {
    Unsupported,
    MissingEnv(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::Unsupported => {
                write!(f, "unsupported secret reference format (F5-a only supports env:VAR_NAME)")
            }
            SecretError::MissingEnv(var) => write!(f, "env var '{}' is not set", var),
        }
    }
}

impl std::error::Error for SecretError {}

/// Resolve a secret reference string.
///
/// Supported formats:
///   - `env:VAR_NAME`
pub fn resolve(reference: &str) -> Result<String, SecretError> {
    if let Some(var) = reference.strip_prefix("env:") {
        return std::env::var(var).map_err(|_| SecretError::MissingEnv(var.to_string()));
    }
    Err(SecretError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_env_ok() {
        std::env::set_var("ATLAPOOL_TEST_SECRET", "s3cr3t");
        assert_eq!(resolve("env:ATLAPOOL_TEST_SECRET").unwrap(), "s3cr3t");
        std::env::remove_var("ATLAPOOL_TEST_SECRET");
    }

    #[test]
    fn resolve_env_missing() {
        let result = resolve("env:ATLAPOOL_TEST_MISSING_VAR");
        assert!(matches!(result, Err(SecretError::MissingEnv(_))));
    }

    #[test]
    fn resolve_unsupported_prefix() {
        let result = resolve("aws:secretsmanager:foo:bar");
        assert_eq!(result, Err(SecretError::Unsupported));
    }
}
