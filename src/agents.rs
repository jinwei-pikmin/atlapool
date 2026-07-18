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
}

impl AgentConfig {
    /// Authorize a single call by checking tool and project/space allowlists.
    ///
    /// Policy:
    /// - Tool must be in `tools`.
    /// - `project` and/or `space` must be resolvable (non-empty).
    /// - At least one resolved value must match the corresponding allowlist.
    /// - If both are resolvable but neither matches, deny.
    pub fn authorize(&self, tool: &str, project: Option<&str>, space: Option<&str>) -> bool {
        if !self.tools.iter().any(|t| t == tool) {
            return false;
        }

        let project = project.filter(|p| !p.is_empty());
        let space = space.filter(|s| !s.is_empty());

        let project_ok = project.is_some_and(|p| allowlist_match(&self.projects, p));
        let space_ok = space.is_some_and(|s| allowlist_match(&self.spaces, s));

        if !project_ok && !space_ok {
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
            key_hash.as_slice().ct_eq(secret_hash.as_slice()).unwrap_u8() == 1
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
        assert!(agent.authorize("jira_search_issues", Some("PROJ"), None));
    }

    #[test]
    fn authorize_tool_denied() {
        let agent = demo_agent();
        assert!(!agent.authorize("jira_delete_issue", Some("PROJ"), None));
    }

    #[test]
    fn authorize_project_allowed() {
        let agent = demo_agent();
        assert!(agent.authorize("jira_search_issues", Some("PROJ"), None));
    }

    #[test]
    fn authorize_project_denied() {
        let agent = demo_agent();
        assert!(!agent.authorize("jira_search_issues", Some("OTHER"), None));
    }

    #[test]
    fn authorize_project_wildcard() {
        let agent = demo_agent();
        assert!(agent.authorize("jira_search_issues", Some("PROJ/123"), None));
        assert!(!agent.authorize("jira_search_issues", Some("OTHER/123"), None));
    }

    #[test]
    fn authorize_space_wildcard() {
        let agent = demo_agent();
        assert!(agent.authorize("confluence_read_page", None, Some("SPACE/HOME")));
        assert!(!agent.authorize("confluence_read_page", None, Some("OTHER/HOME")));
    }

    #[test]
    fn authorize_unresolvable_project_and_space() {
        let agent = demo_agent();
        assert!(!agent.authorize("jira_search_issues", None, None));
    }

    #[test]
    fn authorize_project_and_space_one_matches() {
        let agent = AgentConfig {
            id: "mixed".into(),
            keys: vec![],
            tools: vec!["cross_tool".into()],
            projects: vec!["PROJ".into()],
            spaces: vec![],
        };
        // Project resolves and matches, even though space is unresolved.
        assert!(agent.authorize("cross_tool", Some("PROJ"), None));
    }

    #[test]
    fn authorize_deny_when_both_resolved_but_unlisted() {
        let agent = demo_agent();
        assert!(!agent.authorize("jira_search_issues", Some("OTHER"), Some("OTHER")));
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
}
