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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            atlassian: None,
            mcp: McpConfig::default(),
            audit: AuditConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct AtlassianConfig {
    pub base_url: Option<String>,
    pub token: Option<String>,
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
                let resolved = crate::secrets::resolve(token)?;
                atlassian.token = Some(resolved);
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_default_port() {
        let config = Config::default();
        assert_eq!(config.port, 8080);
    }
}
