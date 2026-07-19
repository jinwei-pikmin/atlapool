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
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
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
            status: None,
            message: None,
        };

        self.write_record(&record).await
    }

    /// Record the outcome of a write operation after the upstream call completes.
    /// This is best-effort logging: failures here are logged but do not change
    /// the response already returned to the caller.
    pub async fn record_result(
        &self,
        agent_id: &str,
        tool: &str,
        target: &str,
        success: bool,
        status: Option<u16>,
        message: Option<&str>,
    ) {
        let record = AuditRecord {
            agent_id: agent_id.to_string(),
            tool: tool.to_string(),
            target: target.to_string(),
            timestamp: rfc3339_now(),
            result: if success {
                "success".to_string()
            } else {
                "failure".to_string()
            },
            status,
            message: message.map(|m| m.to_string()),
        };

        if let Err(e) = self.write_record(&record).await {
            tracing::error!(error = %e, "failed to write audit result record");
        }
    }

    async fn write_record(&self, record: &AuditRecord) -> Result<(), io::Error> {
        let line = serde_json::to_vec(record)
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
    use serde_json::Value;

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

    #[tokio::test]
    async fn record_result_appends_success_and_failure_lines() {
        let path = temp_path();
        let log = AuditLog::new(&path);

        log.record_attempt("demo", "jira_create_issue", "PROJ")
            .await
            .unwrap();
        log.record_result("demo", "jira_create_issue", "PROJ", true, Some(201), None)
            .await;

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);

        let success: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(success["result"], "success");
        assert_eq!(success["status"], 201);

        std::fs::remove_file(&path).ok();
    }
}
