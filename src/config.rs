use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    pub atlassian: Option<AtlassianConfig>,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub audit: AuditConfig,
    #[serde(default)]
    pub agents: Vec<crate::agents::AgentConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            atlassian: None,
            mcp: McpConfig::default(),
            audit: AuditConfig::default(),
            agents: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct AtlassianConfig {
    pub base_url: Option<String>,
    pub token: Option<crate::secrets::SecretString>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct McpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub enable_writes: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct AuditConfig {
    pub path: Option<String>,
}

fn default_port() -> u16 {
    8080
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if let Ok(path) = std::env::var("ATLAPOOL_CONFIG") {
            let content = fs::read_to_string(&path)?;
            return Self::from_toml(&content);
        }
        if Path::new("config.toml").exists() {
            let content = fs::read_to_string("config.toml")?;
            return Self::from_toml(&content);
        }
        Ok(Self::default())
    }

    fn from_toml(content: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut config: Config = toml::from_str(content)?;
        if let Some(ref mut atlassian) = config.atlassian {
            if let Some(token) = atlassian.token.as_ref() {
                let resolved = crate::secrets::resolve(token.expose_secret())?;
                atlassian.token = Some(crate::secrets::SecretString::new(resolved));
            }
        }
        for agent in &mut config.agents {
            agent.keys = agent
                .keys
                .iter()
                .map(|k| {
                    let s = k.expose_secret();
                    if s.starts_with("env:") {
                        Ok(crate::secrets::SecretString::new(crate::secrets::resolve(
                            s,
                        )?))
                    } else {
                        Ok(k.clone())
                    }
                })
                .collect::<Result<Vec<_>, crate::secrets::SecretError>>()?;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_config_has_default_port() {
        let config = Config::default();
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn config_debug_redacts_secret_token() {
        let config = Config {
            atlassian: Some(AtlassianConfig {
                base_url: Some("https://example.atlassian.net".into()),
                token: Some(crate::secrets::SecretString::new("env:SOME_VAR")),
            }),
            ..Config::default()
        };
        let debug = format!("{:?}", config);
        assert!(debug.contains("example.atlassian.net"));
        assert!(!debug.contains("env:SOME_VAR"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn load_fails_when_env_secret_missing() {
        let var = "ATLAPOOL_TEST_CONFIG_MISSING_TOKEN";
        let path = std::env::temp_dir().join(format!(
            "atlapool-config-missing-{}.toml",
            std::process::id()
        ));

        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "port = 8080").unwrap();
        writeln!(file, "\n[atlassian]").unwrap();
        writeln!(file, "token = \"env:{}\"", var).unwrap();

        std::env::remove_var(var);
        std::env::set_var("ATLAPOOL_CONFIG", &path);

        let result = Config::load();

        std::env::remove_var("ATLAPOOL_CONFIG");
        fs::remove_file(&path).ok();

        assert!(
            result.is_err(),
            "missing env var should cause Config::load to fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("env var"),
            "error should mention env var: {}",
            err
        );
    }
}
