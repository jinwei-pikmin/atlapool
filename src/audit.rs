use serde::Serialize;
use std::io;
use std::path::PathBuf;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// An audit log that records write operations as newline-delimited JSON.
#[derive(Clone)]
pub struct AuditLog {
    path: PathBuf,
}

#[derive(Serialize)]
struct AuditRecord {
    agent_id: String,
    tool: String,
    target: String,
    timestamp: String,
    result: String,
}

impl AuditLog {
    /// Create an audit logger that writes to `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Record that a write operation was authorized and is about to be forwarded
    /// upstream. This is the fail-closed checkpoint: if this write fails, the
    /// caller must not proceed with the upstream request.
    pub async fn record_attempt(
        &self,
        agent_id: &str,
        tool: &str,
        target: &str,
    ) -> Result<(), io::Error> {
        let record = AuditRecord {
            agent_id: agent_id.to_string(),
            tool: tool.to_string(),
            target: target.to_string(),
            timestamp: rfc3339_now(),
            result: "attempt".to_string(),
        };

        let line = serde_json::to_vec(&record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;

        file.write_all(&line).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;

        Ok(())
    }

}

fn rfc3339_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "atlapool-audit-{}-{}.jsonl",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ))
    }

    #[tokio::test]
    async fn record_attempt_appends_jsonl() {
        let path = temp_path();
        let log = AuditLog::new(&path);

        log.record_attempt("demo", "jira_create_issue", "PROJ")
            .await
            .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let line = content.trim();
        assert!(line.contains("\"agent_id\":\"demo\""));
        assert!(line.contains("\"tool\":\"jira_create_issue\""));
        assert!(line.contains("\"target\":\"PROJ\""));
        assert!(line.contains("\"result\":\"attempt\""));
        assert!(line.contains("\"timestamp\":\""));

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn record_attempt_fails_when_parent_directory_missing() {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        );
        let path = std::env::temp_dir()
            .join(format!("nonexistent-dir-{unique}"))
            .join("atlapool-audit.jsonl");
        let log = AuditLog::new(&path);

        let result = log
            .record_attempt("demo", "jira_create_issue", "PROJ")
            .await;
        assert!(result.is_err());
    }
}
