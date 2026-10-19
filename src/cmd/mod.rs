//! Subcommand implementations and the context they share.

use std::io::IsTerminal;

use anyhow::Result;

use crate::auth;
use crate::github::{Client, PrState};
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
    ///
    /// Without an explicit revset, GitHub's native stack is asked first, so
    /// that we see the whole stack even when `@` sits part-way through it.
    pub fn segments(
        &self,
        client: &Client,
        revset: Option<&str>,
        default_branch: &str,
    ) -> Result<Vec<Segment>> {
        let mut exclude = vec![default_branch.to_string()];
        exclude.extend(self.jj.trunk_bookmark());
        let prefix = self.jj.push_bookmark_prefix();
        let native = match revset {
            Some(_) => None,
            None => self
                .native_stack_revset(client, &exclude)
                .unwrap_or_else(|err| {
                    eprintln!(
                        "{} couldn't look up a native stack, using the working copy: {err:#}",
                        paint("33", "warning:")
                    );
                    None
                }),
        };
        let fallback;
        let revset = match revset.or(native.as_deref()) {
            Some(r) => r,
            None => {
                fallback = stack::default_revset(&self.jj);
                &fallback
            }
        };
        stack::load(
            &self.jj,
            &Options {
                revset,
                per_change: !self.by_bookmark,
                prefix: &prefix,
                exclude: &exclude,
            },
        )
    }
}

impl Ctx {
    /// A revset covering the native GitHub stack that `@` (or, failing that,
    /// its parents) belongs to, plus `@` itself when it is pushable. `None`
    /// when no bookmark there has an open PR in a stack.
    fn native_stack_revset(&self, client: &Client, exclude: &[String]) -> Result<Option<String>> {
        let unpushable = self.jj.unpushable_revset();
        let pushable_head = format!("(@ ~ {unpushable})");
        // `@` first, so that the stack we are sitting on wins over its parent's.
        for revset in [pushable_head.as_str(), "@-"] {
            let mut names: Vec<String> = self
                .jj
                .changes(revset)?
                .into_iter()
                .flat_map(|c| c.bookmarks)
                .filter(|b| !exclude.contains(b))
                .collect();
            names.sort();
            names.dedup();
            for pr in client.pull_requests_for(&names)?.into_iter().flatten() {
                if pr.state != PrState::Open {
                    continue;
                }
                let Some(found) = client.stack_containing(pr.number)? else {
                    continue;
                };
                let mut heads = Vec::new();
                for n in found.open {
                    heads.push(client.pull_request(n)?.head_ref_name);
                }
                let marks = heads
                    .iter()
                    .map(|h| format!("present(bookmarks(exact:{}))", serde_json::json!(h)))
                    .collect::<Vec<_>>()
                    .join(" | ");
                return Ok(Some(format!("trunk()..heads({marks} | {pushable_head})")));
            }
        }
        Ok(None)
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
