//! Minimal GitHub client: GraphQL for reads, REST for writes.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::Token;
use crate::log;
use crate::repo::RepoRef;

/// What GitHub assumes when no `X-GitHub-Api-Version` header is sent.
const DEFAULT_API_VERSION: &str = "2022-11-28";

/// The stacked pull request endpoints are only available from this version.
const STACKS_API_VERSION: &str = "2026-03-10";

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
    #[serde(default)]
    pub body: String,
    pub url: String,
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

const PR_FIELDS: &str = "number title body url state isDraft baseRefName headRefName \
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

fn trace_response(resp: &ureq::http::Response<ureq::Body>, started: std::time::Instant) {
    log::trace(format!(
        "→ {} after {}ms",
        resp.status(),
        started.elapsed().as_millis()
    ));
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
        self.rest_versioned(method, path, body, DEFAULT_API_VERSION)
    }

    fn rest_versioned(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        version: &str,
    ) -> Result<Value> {
        let url = format!("{}{}", self.api, path);
        let what = format!("{method} {path}");
        let auth = format!("Bearer {}", self.token.value);
        log::trace(format!("{method} {url} (API version {version})"));
        let started = std::time::Instant::now();
        let resp = match (method, body) {
            ("GET", _) => self
                .agent
                .get(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", version)
                .call(),
            ("POST", Some(b)) => self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", version)
                .send_json(b),
            ("PATCH", Some(b)) => self
                .agent
                .patch(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", version)
                .send_json(b),
            ("PUT", Some(b)) => self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", version)
                .send_json(b),
            ("PUT", None) => self
                .agent
                .put(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", version)
                .send_empty(),
            _ => bail!("unsupported request {what}"),
        }
        .with_context(|| format!("{what}: request failed"))?;
        trace_response(&resp, started);
        self.finish(&what, resp)
    }

    fn graphql(&self, query: &str) -> Result<Value> {
        let auth = format!("Bearer {}", self.token.value);
        log::trace(format!("POST {} (GraphQL)", self.graphql));
        let started = std::time::Instant::now();
        let resp = self
            .agent
            .post(&self.graphql)
            .header("Authorization", &auth)
            .send_json(json!({ "query": query }))
            .context("GraphQL request failed")?;
        trace_response(&resp, started);
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
        let _step = log::step("Asking GitHub who we are");
        let data = self.graphql("{ viewer { login } }")?;
        data["viewer"]["login"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no viewer in response"))
    }

    pub fn default_branch(&self) -> Result<String> {
        let _step = log::step(format!("Loading {}'s default branch", self.repo.slug()));
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

    /// The merge methods the repository permits, as the names the merge endpoint takes.
    pub fn allowed_merge_methods(&self) -> Result<Vec<&'static str>> {
        let _step = log::step("Loading the merge methods the repository allows");
        let q = format!(
            "{{ repository(owner: {}, name: {}) {{ \
               rebaseMergeAllowed squashMergeAllowed mergeCommitAllowed }} }}",
            json!(self.repo.owner),
            json!(self.repo.name)
        );
        let repo = &self.graphql(&q)?["repository"];
        Ok([
            ("rebaseMergeAllowed", "rebase"),
            ("squashMergeAllowed", "squash"),
            ("mergeCommitAllowed", "merge"),
        ]
        .into_iter()
        .filter(|(field, _)| repo[field].as_bool() == Some(true))
        .map(|(_, name)| name)
        .collect())
    }

    /// For each head branch, the most recent PR from it (open preferred).
    pub fn pull_requests_for(&self, branches: &[String]) -> Result<Vec<Option<PullRequest>>> {
        if branches.is_empty() {
            return Ok(vec![]);
        }
        let _step = log::step(format!(
            "Loading pull requests for {}",
            plural(branches.len(), "branch", "branches")
        ));
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
        let _step = log::step(format!("Loading #{number}"));
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
        let _step = log::step(format!("Loading where #{number} comes from"));
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
        let _step = log::step(format!("Creating a pull request for {head}"));
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
        let _step = log::step(format!("Updating #{number}"));
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
        let _step = log::step(format!("Requesting review on #{number}"));
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

    /// Merge `number` and, if it's in a native stack, every PR below it, in one atomic request.
    /// The synchronous merge endpoint refuses stacked PRs, so this always goes through
    /// `merge-async`. `sha` makes GitHub cancel the merge if the branch has moved.
    pub fn merge_pull_request(&self, number: u64, method: &str, sha: &str) -> Result<MergeOutcome> {
        let path = format!("/repos/{}/pulls/{number}/merge-async", self.repo.slug());
        let _step = log::step(format!("Merging #{number}"));
        let mut v = self.rest_versioned(
            "PUT",
            &path,
            Some(json!({ "merge_method": method, "sha": sha })),
            STACKS_API_VERSION,
        )?;
        let deadline = std::time::Instant::now() + MERGE_TIMEOUT;
        let mut waiting = None;
        loop {
            match v["status"].as_str() {
                Some("merged") => return Ok(MergeOutcome::Merged),
                Some("enqueued") => return Ok(MergeOutcome::Enqueued),
                Some("failed") => bail!(
                    "merging #{number} failed: {}",
                    v["details"]["message"]
                        .as_str()
                        .unwrap_or("no reason given")
                ),
                _ => {}
            }
            let uuid = v["details"]["uuid"]
                .as_str()
                .ok_or_else(|| anyhow!("merge of #{number} returned no request id"))?
                .to_string();
            if std::time::Instant::now() > deadline {
                bail!("timed out waiting for #{number} to merge; check GitHub for the outcome");
            }
            // Once, so a slow merge doesn't fill a log with one line a second.
            if waiting.is_none() {
                waiting = Some(log::step(format!("Waiting for GitHub to merge #{number}")));
            }
            std::thread::sleep(Duration::from_secs(1));
            v = self.rest_versioned("GET", &format!("{path}/{uuid}"), None, STACKS_API_VERSION)?;
        }
    }
}

/// How long to wait for GitHub to finish a merge it accepted.
const MERGE_TIMEOUT: Duration = Duration::from_secs(300);

/// What became of an accepted merge request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged,
    /// Handed to a merge queue: nothing has landed yet.
    Enqueued,
}

/// A native GitHub stack, as far as we care: its identifier and the PRs in it, bottom first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    pub number: u64,
    /// Open PRs only; merged ones stay in GitHub's stack but aren't ours to manage.
    pub open: Vec<u64>,
}

impl Stack {
    fn from_rest(v: &Value) -> Result<Self> {
        let number = v["number"]
            .as_u64()
            .ok_or_else(|| anyhow!("no stack number in response"))?;
        let open = v["pull_requests"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|p| p["merged_at"].is_null())
                    .filter_map(|p| p["number"].as_u64())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Stack { number, open })
    }
}

impl Client {
    fn stacks_path(&self, rest: &str) -> String {
        format!("/repos/{}/stacks{rest}", self.repo.slug())
    }

    /// The native stack containing `pr`, if there is one.
    pub fn stack_containing(&self, pr: u64) -> Result<Option<Stack>> {
        let _step = log::step(format!("Looking for the stack containing #{pr}"));
        let v = self.rest_versioned(
            "GET",
            &self.stacks_path(&format!("?pull_request={pr}")),
            None,
            STACKS_API_VERSION,
        )?;
        v.as_array()
            .and_then(|a| a.first())
            .map(Stack::from_rest)
            .transpose()
    }

    /// Create a stack from PRs ordered bottom to top.
    pub fn create_stack(&self, pull_requests: &[u64]) -> Result<()> {
        let _step = log::step("Creating a stack");
        self.rest_versioned(
            "POST",
            &self.stacks_path(""),
            Some(json!({ "pull_requests": pull_requests })),
            STACKS_API_VERSION,
        )
        .map(|_| ())
    }

    /// Append PRs, ordered upward from the current top of the stack.
    pub fn add_to_stack(&self, stack: u64, pull_requests: &[u64]) -> Result<()> {
        let _step = log::step(format!("Adding to stack {stack}"));
        self.rest_versioned(
            "POST",
            &self.stacks_path(&format!("/{stack}/add")),
            Some(json!({ "pull_requests": pull_requests })),
            STACKS_API_VERSION,
        )
        .map(|_| ())
    }

    /// Remove every unmerged PR from the stack, dissolving it.
    pub fn unstack(&self, stack: u64) -> Result<()> {
        let _step = log::step(format!("Dissolving stack {stack}"));
        self.rest_versioned(
            "POST",
            &self.stacks_path(&format!("/{stack}/unstack")),
            Some(json!({})),
            STACKS_API_VERSION,
        )
        .map(|_| ())
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
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
