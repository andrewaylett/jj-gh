//! Subcommand implementations and the context they share.

use std::io::IsTerminal;

use anyhow::Result;

use crate::auth;
use crate::github::Client;
use crate::jj::Jj;
use crate::repo::RepoRef;
use crate::stack::{self, Options, Segment};

pub mod merge;
pub mod pr;
pub mod status;
pub mod submit;
pub mod sync;

/// Settings every command needs, resolved from the global flags.
pub struct Ctx {
    pub jj: Jj,
    pub remote: String,
    pub repo_override: Option<String>,
    /// End PRs only at existing bookmarks, instead of one PR per change.
    pub by_bookmark: bool,
}

impl Ctx {
    pub fn repo(&self) -> Result<RepoRef> {
        match &self.repo_override {
            Some(slug) => RepoRef::from_slug(slug),
            None => RepoRef::from_url(&self.jj.remote_url(&self.remote)?),
        }
    }

    pub fn client(&self) -> Result<Client> {
        let repo = self.repo()?;
        let token = auth::find_token(&repo.host)?;
        Ok(Client::new(repo, token))
    }

    /// Cut the requested revset into segments. `default_branch` is excluded
    /// from stack bookmarks so that a local `main` never becomes a PR.
    pub fn segments(&self, revset: Option<&str>, default_branch: &str) -> Result<Vec<Segment>> {
        let mut exclude = vec![default_branch.to_string()];
        exclude.extend(self.jj.trunk_bookmark());
        let prefix = self.jj.push_bookmark_prefix();
        stack::load(
            &self.jj,
            &Options {
                revset: revset.unwrap_or(stack::DEFAULT_REVSET),
                per_change: !self.by_bookmark,
                prefix: &prefix,
                exclude: &exclude,
            },
        )
    }
}

/// Minimal ANSI styling, disabled when piped or when NO_COLOR is set.
pub fn paint(code: &str, text: &str) -> String {
    if std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}
