//! Starting the crawl on GitHub Actions from the Worker's own cron.
//!
//! GitHub runs a `schedule:` trigger on a best-effort basis, and a 5-minute
//! one on a new repository fired once in ninety minutes. A workflow started
//! through the API (`workflow_dispatch`) runs when asked, so a Cloudflare
//! cron, which does fire on time, asks every five minutes. A run that is
//! still going simply queues the next one (the workflow's `concurrency`
//! group keeps at most one waiting).
//!
//! Off unless all are set:
//!   GITHUB_DISPATCH_TOKEN  secret  fine-grained token: this repository only,
//!                                  Actions read and write, nothing else
//!   GITHUB_REPO            var     "owner/name"
//!   GITHUB_WORKFLOW        var     workflow file name, e.g. "crawl.yml"
//!
//! Plain Rust: it shapes the request; `entry.rs` sends it.

use serde_json::{json, Value};

/// The cron expression that means "start a crawl" (wrangler.toml [triggers]).
pub const DISPATCH_CRON: &str = "*/5 * * * *";

#[derive(Debug, Clone, PartialEq)]
pub struct DispatchConfig {
    pub token: String,
    pub repo: String,
    pub workflow: String,
    /// Branch the workflow runs on.
    pub git_ref: String,
}

impl DispatchConfig {
    /// `None` unless the token, a well-formed "owner/name" and a workflow are set.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Option<DispatchConfig> {
        let val = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let repo = val("GITHUB_REPO")?;
        let (owner, name) = repo.split_once('/')?;
        // GitHub names are letters, digits, "-", "_" and "."; one starting with
        // a dot ("..") would walk the URL path instead of naming anything.
        let ok = |s: &str| {
            !s.is_empty() && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        };
        if !ok(owner) || !ok(name) {
            return None;
        }
        let workflow = val("GITHUB_WORKFLOW")?;
        if !ok(&workflow) {
            return None;
        }
        Some(DispatchConfig {
            token: val("GITHUB_DISPATCH_TOKEN")?,
            repo,
            workflow,
            git_ref: val("GITHUB_REF").unwrap_or_else(|| "main".to_string()),
        })
    }

    /// POST here to start the workflow.
    pub fn url(&self) -> String {
        format!("https://api.github.com/repos/{}/actions/workflows/{}/dispatches", self.repo, self.workflow)
    }

    /// Headers GitHub's REST API wants; it refuses requests without a User-Agent.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Authorization", format!("Bearer {}", self.token)),
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", "2022-11-28".to_string()),
            ("User-Agent", "carbuyer-api-worker".to_string()),
            ("Content-Type", "application/json".to_string()),
        ]
    }

    /// The workflow's one input, given explicitly rather than trusting its default.
    pub fn body(&self) -> Value {
        json!({ "ref": self.git_ref, "inputs": { "searches": "searches.yml" } })
    }
}

/// GitHub answers 204 No Content when the run is created.
pub fn started(status: u16, body: &str) -> Result<(), String> {
    if status == 204 || status == 200 {
        return Ok(());
    }
    let why = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect());
    Err(format!("GitHub {status}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(k: &str) -> Option<String> {
        Some(
            match k {
                "GITHUB_DISPATCH_TOKEN" => "tok",
                "GITHUB_REPO" => "manrajtoor/used-car-deal-finder",
                "GITHUB_WORKFLOW" => "crawl.yml",
                _ => "",
            }
            .to_string(),
        )
    }

    #[test]
    fn config_needs_token_repo_and_workflow() {
        let c = DispatchConfig::from_vars(vars).unwrap();
        assert_eq!(c.git_ref, "main");
        assert_eq!(c.url(), "https://api.github.com/repos/manrajtoor/used-car-deal-finder/actions/workflows/crawl.yml/dispatches");
        assert!(DispatchConfig::from_vars(|k| (k != "GITHUB_DISPATCH_TOKEN").then(|| vars(k)).flatten()).is_none());
        assert!(DispatchConfig::from_vars(|k| (k != "GITHUB_WORKFLOW").then(|| vars(k)).flatten()).is_none());
        assert!(DispatchConfig::from_vars(|_| Some("  ".into())).is_none());
    }

    #[test]
    fn a_malformed_repo_or_workflow_is_refused_not_sent() {
        for repo in ["noslash", "a/b/c", "../x", "owner/", "/name", "own er/name"] {
            let get = |k: &str| if k == "GITHUB_REPO" { Some(repo.to_string()) } else { vars(k) };
            assert!(DispatchConfig::from_vars(get).is_none(), "{repo}");
        }
        let get = |k: &str| if k == "GITHUB_WORKFLOW" { Some("../../x".to_string()) } else { vars(k) };
        assert!(DispatchConfig::from_vars(get).is_none());
    }

    #[test]
    fn request_shape() {
        let c = DispatchConfig::from_vars(vars).unwrap();
        let h = c.headers();
        assert!(h.contains(&("Authorization", "Bearer tok".to_string())));
        assert!(h.iter().any(|(k, _)| *k == "User-Agent"));
        assert_eq!(c.body(), json!({"ref": "main", "inputs": {"searches": "searches.yml"}}));
    }

    #[test]
    fn only_204_is_a_start() {
        assert!(started(204, "").is_ok());
        assert_eq!(started(404, r#"{"message":"Not Found"}"#).unwrap_err(), "GitHub 404: Not Found");
        assert!(started(403, "nope").unwrap_err().contains("403"));
    }
}
