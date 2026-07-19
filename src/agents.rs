use serde::Deserialize;

/// Per-agent identity and default-deny allowlists.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    #[serde(default)]
    pub keys: Vec<crate::secrets::SecretString>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default)]
    pub spaces: Vec<String>,
    #[serde(default)]
    pub bitbucket_workspaces: Vec<String>,
    #[serde(default)]
    pub bitbucket_repos: Vec<String>,
    #[serde(default)]
    pub enable_writes: Option<bool>,
}

/// Tool classification for write-gate decisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    Read,
    Write,
}

/// Classify a tool name into Read or Write.
///
/// Tools whose name contains a read verb (`get`, `list`, `search`, `read`)
/// as a `_`-delimited segment are treated as read operations. Everything else —
/// including unknown tool names — is conservatively treated as a write operation.
pub fn classify_tool(tool: &str) -> ToolKind {
    let is_read = tool
        .split('_')
        .any(|segment| matches!(segment, "get" | "list" | "search" | "read"));
    if is_read {
        ToolKind::Read
    } else {
        ToolKind::Write
    }
}

impl AgentConfig {
    /// Authorize a single call by checking tool and all provided allowlist dimensions.
    ///
    /// Policy:
    /// - Tool must be in `tools`.
    /// - Any provided non-empty dimension (`project`, `space`, `workspace`, `repo`)
    ///   must match the corresponding allowlist. All provided dimensions must pass.
    /// - If no dimension is provided, deny.
    pub fn authorize(
        &self,
        tool: &str,
        project: Option<&str>,
        space: Option<&str>,
        workspace: Option<&str>,
        repo: Option<&str>,
    ) -> bool {
        if !self.tools.iter().any(|t| t == tool) {
            return false;
        }

        let project = project.filter(|p| !p.is_empty());
        let space = space.filter(|s| !s.is_empty());
        let workspace = workspace.filter(|w| !w.is_empty());
        let repo = repo.filter(|r| !r.is_empty());

        if project.is_none() && space.is_none() && workspace.is_none() && repo.is_none() {
            return false;
        }

        if project.is_some_and(|p| !allowlist_match(&self.projects, p)) {
            return false;
        }
        if space.is_some_and(|s| !allowlist_match(&self.spaces, s)) {
            return false;
        }
        if workspace.is_some_and(|w| !allowlist_match(&self.bitbucket_workspaces, w)) {
            return false;
        }
        if repo.is_some_and(|r| !allowlist_match(&self.bitbucket_repos, r)) {
            return false;
        }

        true
    }
}

/// Find an agent whose configured keys contain `key`.
///
/// To avoid leaking key content through timing, the comparison is done by
/// hashing both keys with SHA-256 and comparing the fixed-size digests with
/// `subtle::ConstantTimeEq`. This prevents short-circuiting on length or on
/// the position of first differing byte.
pub fn find_agent<'a>(agents: &'a [AgentConfig], key: &str) -> Option<&'a AgentConfig> {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;

    let key_hash = Sha256::digest(key.as_bytes());
    agents.iter().find(|a| {
        a.keys.iter().any(|k| {
            let secret_hash = Sha256::digest(k.expose_secret().as_bytes());
            key_hash
                .as_slice()
                .ct_eq(secret_hash.as_slice())
                .unwrap_u8()
                == 1
        })
    })
}

/// Returns true if `value` matches at least one pattern in the allowlist.
fn allowlist_match(allowlist: &[String], value: &str) -> bool {
    allowlist.iter().any(|pat| glob_match(pat, value))
}

/// Simple glob match. `*` matches any character sequence (including empty and
/// path separators). No other metacharacters are supported.
fn glob_match(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let m = p.len();
    let n = v.len();

    // dp[i][j] = pattern[0..i] matches value[0..j]
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[0][0] = true;

    // A leading `*` in the pattern can match an empty value prefix.
    for i in 1..=m {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }

    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = match p[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1] || dp[i - 1][j - 1],
                c => c == v[j - 1] && dp[i - 1][j - 1],
            };
        }
    }

    dp[m][n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretString;

    fn demo_agent() -> AgentConfig {
        AgentConfig {
            id: "demo".into(),
            keys: vec![SecretString::new("key1"), SecretString::new("key2")],
            tools: vec!["jira_search_issues".into(), "confluence_read_page".into()],
            projects: vec!["PROJ".into(), "PROJ/*".into()],
            spaces: vec!["SPACE".into(), "SPACE/*".into()],
            bitbucket_workspaces: vec!["WORK".into(), "WORK/*".into()],
            bitbucket_repos: vec!["REPO".into(), "REPO/*".into()],
            enable_writes: None,
        }
    }

    #[test]
    fn find_agent_by_key_ok() {
        let agents = vec![demo_agent()];
        let agent = find_agent(&agents, "key1");
        assert!(agent.is_some());
        assert_eq!(agent.unwrap().id, "demo");
    }

    #[test]
    fn find_agent_by_key_not_found() {
        let agents = vec![demo_agent()];
        assert!(find_agent(&agents, "wrong-key").is_none());
    }

    #[test]
    fn authorize_tool_allowed() {
        let agent = demo_agent();
        assert!(agent.authorize("jira_search_issues", Some("PROJ"), None, None, None));
    }

    #[test]
    fn authorize_tool_denied() {
        let agent = demo_agent();
        assert!(!agent.authorize("jira_delete_issue", Some("PROJ"), None, None, None));
    }

    #[test]
    fn authorize_project_allowed() {
        let agent = demo_agent();
        assert!(agent.authorize("jira_search_issues", Some("PROJ"), None, None, None));
    }

    #[test]
    fn authorize_project_denied() {
        let agent = demo_agent();
        assert!(!agent.authorize("jira_search_issues", Some("OTHER"), None, None, None));
    }

    #[test]
    fn authorize_project_wildcard() {
        let agent = demo_agent();
        assert!(agent.authorize("jira_search_issues", Some("PROJ/123"), None, None, None));
        assert!(!agent.authorize("jira_search_issues", Some("OTHER/123"), None, None, None));
    }

    #[test]
    fn authorize_space_wildcard() {
        let agent = demo_agent();
        assert!(agent.authorize("confluence_read_page", None, Some("SPACE/HOME"), None, None));
        assert!(!agent.authorize("confluence_read_page", None, Some("OTHER/HOME"), None, None));
    }

    #[test]
    fn authorize_unresolvable_project_and_space() {
        let agent = demo_agent();
        assert!(!agent.authorize("jira_search_issues", None, None, None, None));
    }

    #[test]
    fn authorize_project_and_space_one_matches() {
        let agent = AgentConfig {
            id: "mixed".into(),
            keys: vec![],
            tools: vec!["cross_tool".into()],
            projects: vec!["PROJ".into()],
            spaces: vec![],
            bitbucket_workspaces: vec![],
            bitbucket_repos: vec![],
            enable_writes: None,
        };
        // Project resolves and matches, even though space is unresolved.
        assert!(agent.authorize("cross_tool", Some("PROJ"), None, None, None));
    }

    #[test]
    fn authorize_bitbucket_workspace_and_repo_allowed() {
        let agent = AgentConfig {
            id: "bb".into(),
            keys: vec![],
            tools: vec!["bitbucket_get_repo".into()],
            projects: vec![],
            spaces: vec![],
            bitbucket_workspaces: vec!["WORK".into()],
            bitbucket_repos: vec!["REPO".into()],
            enable_writes: None,
        };
        assert!(agent.authorize("bitbucket_get_repo", None, None, Some("WORK"), Some("REPO")));
    }

    #[test]
    fn authorize_bitbucket_repo_denied() {
        let agent = AgentConfig {
            id: "bb".into(),
            keys: vec![],
            tools: vec!["bitbucket_get_repo".into()],
            projects: vec![],
            spaces: vec![],
            bitbucket_workspaces: vec!["WORK".into()],
            bitbucket_repos: vec!["REPO".into()],
            enable_writes: None,
        };
        assert!(!agent.authorize(
            "bitbucket_get_repo",
            None,
            None,
            Some("WORK"),
            Some("OTHER")
        ));
    }

    #[test]
    fn authorize_deny_when_both_resolved_but_unlisted() {
        let agent = demo_agent();
        assert!(!agent.authorize(
            "jira_search_issues",
            Some("OTHER"),
            Some("OTHER"),
            None,
            None
        ));
    }

    #[test]
    fn glob_match_empty_star() {
        assert!(glob_match("PROJ*", "PROJ"));
        assert!(glob_match("PROJ*", "PROJ-123"));
        assert!(!glob_match("PROJ*", "OTHER"));
    }

    #[test]
    fn glob_match_star_across_slash() {
        assert!(glob_match("PROJ/*", "PROJ/123"));
        assert!(glob_match("PROJ/*", "PROJ/123/sub"));
        assert!(!glob_match("PROJ/*", "PROJX"));
    }

    #[test]
    fn classify_tool_read_prefixes() {
        assert_eq!(classify_tool("jira_get_issue"), ToolKind::Read);
        assert_eq!(classify_tool("confluence_list_comments"), ToolKind::Read);
        assert_eq!(classify_tool("jira_search_issues"), ToolKind::Read);
        assert_eq!(classify_tool("confluence_read_page"), ToolKind::Read);
    }

    #[test]
    fn classify_tool_write_or_unknown() {
        assert_eq!(classify_tool("jira_create_issue"), ToolKind::Write);
        assert_eq!(classify_tool("confluence_delete_page"), ToolKind::Write);
        assert_eq!(classify_tool("jira_unknown_tool"), ToolKind::Write);
    }
}
