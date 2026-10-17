//! Minimal GitHub client: GraphQL for reads, REST for writes.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::Token;
use crate::repo::RepoRef;

pub struct Client {
    agent: ureq::Agent,
    token: Token,
    pub repo: RepoRef,
    api: String,
    graphql: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub body: String,
    pub state: PrState,
    pub is_draft: bool,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub review_decision: Option<String>,
    pub mergeable: Option<String>,
    /// SUCCESS, FAILURE, ERROR, PENDING, EXPECTED, or None when no checks ran.
    #[serde(default)]
    pub checks: Option<String>,
}

const PR_FIELDS: &str = "number title url body state isDraft baseRefName headRefName \
    headRefOid reviewDecision mergeable \
    commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }";

/// Raw shape of a PR node, before flattening the check rollup.
#[derive(Deserialize)]
struct RawPr {
    #[serde(flatten)]
    pr: PullRequest,
    commits: Option<RawCommits>,
}

#[derive(Deserialize)]
struct RawCommits {
    nodes: Vec<RawCommitNode>,
}

#[derive(Deserialize)]
struct RawCommitNode {
    commit: RawCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
    status_check_rollup: Option<RawRollup>,
}

#[derive(Deserialize)]
struct RawRollup {
    state: String,
}

impl RawPr {
    fn flatten(mut self) -> PullRequest {
        self.pr.checks = self
            .commits
            .and_then(|c| c.nodes.into_iter().next())
            .and_then(|n| n.commit.status_check_rollup)
            .map(|r| r.state);
        self.pr
    }
}

pub fn api_urls(host: &str) -> (String, String) {
    let api = std::env::var("JJ_GH_API_URL").unwrap_or_else(|_| {
        if host == "github.com" {
            "https://api.github.com".to_string()
        } else {
            format!("https://{host}/api/v3")
        }
    });
    let api = api.trim_end_matches('/').to_string();
    let graphql =
        std::env::var("JJ_GH_GRAPHQL_URL").unwrap_or_else(|_| match api.strip_suffix("/api/v3") {
            Some(base) => format!("{base}/api/graphql"),
            None => format!("{api}/graphql"),
        });
    (api, graphql)
}

impl Client {
    pub fn new(repo: RepoRef, token: Token) -> Self {
        let (api, graphql) = api_urls(&repo.host);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent(concat!("jj-gh/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Client {
            agent,
            token,
            repo,
            api,
            graphql,
        }
    }

    pub fn token_source(&self) -> &str {
        &self.token.source
    }

    fn finish(&self, what: &str, mut resp: ureq::http::Response<ureq::Body>) -> Result<Value> {
        let status = resp.status();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = body
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| text.trim().chars().take(300).collect());
            let detail = body
                .get("errors")
                .filter(|e| !e.is_null())
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            let hint = match status.as_u16() {
                401 => " — is the token valid? try `gh auth login` or refresh GITHUB_TOKEN",
                403 | 404 => " — check the token has access to this repository",
                _ => "",
            };
            bail!("{what}: HTTP {status}: {msg}{detail}{hint}");
        }
        Ok(body)
    }

    fn rest(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{}{}", self.api, path);
        let what = format!("{method} {path}");
        let auth = format!("Bearer {}", self.token.value);
        let resp = match (method, body) {
            ("GET", _) => self
                .agent
                .get(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .call(),
            ("POST", Some(b)) => self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .send_json(b),
            ("PATCH", Some(b)) => self
                .agent
                .patch(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .send_json(b),
            ("PUT", Some(b)) => self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .send_json(b),
            ("PUT", None) => self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .send_empty(),
            _ => bail!("unsupported request {what}"),
        }
        .with_context(|| format!("{what}: request failed"))?;
        self.finish(&what, resp)
    }

    fn graphql(&self, query: &str) -> Result<Value> {
        let auth = format!("Bearer {}", self.token.value);
        let resp = self
            .agent
            .post(&self.graphql)
            .header("Authorization", &auth)
            .send_json(json!({ "query": query }))
            .context("GraphQL request failed")?;
        let body = self.finish("GraphQL", resp)?;
        if let Some(errors) = body.get("errors").filter(|e| !e.is_null()) {
            let msg = errors
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.get("message").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_else(|| errors.to_string());
            bail!("GraphQL: {msg}");
        }
        body.get("data")
            .cloned()
            .ok_or_else(|| anyhow!("GraphQL response had no data"))
    }

    pub fn viewer_login(&self) -> Result<String> {
        let data = self.graphql("{ viewer { login } }")?;
        data["viewer"]["login"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no viewer in response"))
    }

    pub fn default_branch(&self) -> Result<String> {
        let q = format!(
            "{{ repository(owner: {}, name: {}) {{ defaultBranchRef {{ name }} }} }}",
            json!(self.repo.owner),
            json!(self.repo.name)
        );
        let data = self.graphql(&q)?;
        data["repository"]["defaultBranchRef"]["name"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("repository {} has no default branch", self.repo.slug()))
    }

    /// For each head branch, the most recent PR from it (open preferred).
    pub fn pull_requests_for(&self, branches: &[String]) -> Result<Vec<Option<PullRequest>>> {
        if branches.is_empty() {
            return Ok(vec![]);
        }
        let mut selections = String::new();
        for (i, b) in branches.iter().enumerate() {
            selections.push_str(&format!(
                "p{i}: pullRequests(headRefName: {}, first: 5, \
                 orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ {PR_FIELDS} }} }}\n",
                json!(b)
            ));
        }
        let q = format!(
            "{{ repository(owner: {}, name: {}) {{ {selections} }} }}",
            json!(self.repo.owner),
            json!(self.repo.name)
        );
        let data = self.graphql(&q)?;
        let repo = &data["repository"];
        let mut out = Vec::with_capacity(branches.len());
        for i in 0..branches.len() {
            let nodes = repo[format!("p{i}")]["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut prs = nodes
                .into_iter()
                .map(|n| serde_json::from_value::<RawPr>(n).map(RawPr::flatten))
                .collect::<Result<Vec<_>, _>>()
                .context("unexpected pull request shape from GitHub")?;
            // Prefer an open PR; otherwise the newest (already first).
            let pick = prs
                .iter()
                .position(|p| p.state == PrState::Open)
                .unwrap_or(0);
            out.push(if prs.is_empty() {
                None
            } else {
                Some(prs.swap_remove(pick))
            });
        }
        Ok(out)
    }

    pub fn pull_request(&self, number: u64) -> Result<PullRequest> {
        let q = format!(
            "{{ repository(owner: {}, name: {}) {{ pullRequest(number: {number}) {{ {PR_FIELDS} }} }} }}",
            json!(self.repo.owner),
            json!(self.repo.name)
        );
        let data = self.graphql(&q)?;
        let node = data["repository"]["pullRequest"].clone();
        if node.is_null() {
            bail!("no pull request #{number} in {}", self.repo.slug());
        }
        Ok(serde_json::from_value::<RawPr>(node)?.flatten())
    }

    /// `owner/name` of the repo a PR's head branch lives in.
    pub fn head_repository(&self, number: u64) -> Result<Option<String>> {
        let q = format!(
            "{{ repository(owner: {}, name: {}) {{ pullRequest(number: {number}) {{ \
             headRepository {{ nameWithOwner }} }} }} }}",
            json!(self.repo.owner),
            json!(self.repo.name)
        );
        let data = self.graphql(&q)?;
        Ok(
            data["repository"]["pullRequest"]["headRepository"]["nameWithOwner"]
                .as_str()
                .map(str::to_string),
        )
    }

    pub fn create_pull_request(
        &self,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
        draft: bool,
    ) -> Result<PullRequestRef> {
        let v = self.rest(
            "POST",
            &format!("/repos/{}/pulls", self.repo.slug()),
            Some(json!({
                "title": title, "body": body, "head": head, "base": base, "draft": draft,
            })),
        )?;
        PullRequestRef::from_rest(&v)
    }

    pub fn update_pull_request(&self, number: u64, fields: Value) -> Result<()> {
        self.rest(
            "PATCH",
            &format!("/repos/{}/pulls/{number}", self.repo.slug()),
            Some(fields),
        )
        .map(|_| ())
    }

    pub fn request_reviewers(&self, number: u64, reviewers: &[String]) -> Result<()> {
        if reviewers.is_empty() {
            return Ok(());
        }
        self.rest(
            "POST",
            &format!(
                "/repos/{}/pulls/{number}/requested_reviewers",
                self.repo.slug()
            ),
            Some(json!({ "reviewers": reviewers })),
        )
        .map(|_| ())
    }

    pub fn merge_pull_request(&self, number: u64, method: &str) -> Result<()> {
        self.rest(
            "PUT",
            &format!("/repos/{}/pulls/{number}/merge", self.repo.slug()),
            Some(json!({ "merge_method": method })),
        )
        .map(|_| ())
    }
}

#[derive(Debug, Clone)]
pub struct PullRequestRef {
    pub number: u64,
    pub url: String,
}

impl PullRequestRef {
    fn from_rest(v: &Value) -> Result<Self> {
        Ok(PullRequestRef {
            number: v["number"]
                .as_u64()
                .ok_or_else(|| anyhow!("no PR number in response"))?,
            url: v["html_url"].as_str().unwrap_or_default().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_urls_for_dotcom_and_enterprise() {
        // Only exercised when the override variables are absent.
        if std::env::var_os("JJ_GH_API_URL").is_some() {
            return;
        }
        assert_eq!(
            api_urls("github.com"),
            (
                "https://api.github.com".into(),
                "https://api.github.com/graphql".into()
            )
        );
        assert_eq!(
            api_urls("ghe.example.com"),
            (
                "https://ghe.example.com/api/v3".into(),
                "https://ghe.example.com/api/graphql".into()
            )
        );
    }
}
