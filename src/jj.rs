//! Thin wrapper around the `jj` binary.
//!
//! We deliberately shell out rather than link jj-lib: the CLI is jj's stable
//! interface, and the templates below keep us independent of its human output.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::log;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Change {
    #[serde(rename = "change")]
    pub change_id: String,
    #[serde(rename = "commit")]
    pub commit_id: String,
    #[serde(rename = "desc")]
    pub description: String,
    pub empty: bool,
    pub conflict: bool,
    pub parents: Vec<String>,
    pub bookmarks: Vec<String>,
}

impl Change {
    pub fn short_id(&self) -> &str {
        &self.change_id[..self.change_id.len().min(8)]
    }

    pub fn title(&self) -> &str {
        self.description.lines().next().unwrap_or("").trim()
    }

    /// Everything after the first line, trimmed, without a closing block of
    /// `Co-Authored-By:` trailers.
    ///
    /// GitHub strips that block when it creates a PR, so keeping it would make
    /// every description look edited.
    pub fn body(&self) -> &str {
        let body = self
            .description
            .split_once('\n')
            .map(|(_, rest)| rest.trim())
            .unwrap_or("");
        let (rest, last) = body.rsplit_once("\n\n").unwrap_or(("", body));
        if last.lines().all(is_co_author) {
            rest.trim_end()
        } else {
            body
        }
    }
}

fn is_co_author(line: &str) -> bool {
    line.split_once(':')
        .is_some_and(|(key, _)| key.eq_ignore_ascii_case("co-authored-by"))
}

/// "Running jj git fetch": the subcommand, without its flags and their values.
fn running(args: &[&str]) -> String {
    let words: Vec<&str> = args
        .iter()
        .copied()
        .take_while(|a| !a.starts_with('-'))
        .take(3)
        .collect();
    format!("Running jj {}", words.join(" "))
}

const LOG_TEMPLATE: &str = r#"'{"change":' ++ json(change_id) ++
  ',"commit":' ++ json(commit_id) ++
  ',"desc":' ++ json(description) ++
  ',"empty":' ++ json(empty) ++
  ',"conflict":' ++ json(conflict) ++
  ',"parents":[' ++ parents.map(|p| json(p.commit_id())).join(",") ++ ']' ++
  ',"bookmarks":[' ++ local_bookmarks.map(|b| json(b.name())).join(",") ++ ']}' ++ "\n""#;

#[derive(Debug, Clone, Default)]
pub struct Jj {
    pub repository: Option<PathBuf>,
}

impl Jj {
    fn command(&self) -> Command {
        let mut cmd = Command::new(std::env::var_os("JJ_GH_JJ").unwrap_or_else(|| "jj".into()));
        // A user's `ui.log-word-wrap = true` re-wraps template output, which
        // injects raw newlines into the JSON we parse.
        cmd.args([
            "--no-pager",
            "--color",
            "never",
            "--config",
            "ui.log-word-wrap=false",
        ]);
        if let Some(repo) = &self.repository {
            cmd.arg("-R").arg(repo);
        }
        cmd
    }

    /// Run jj, returning stdout. stderr is surfaced on failure.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let mut cmd = self.command();
        cmd.args(args);
        let _step = log::step(running(args));
        log::trace(log::command_line(&cmd));
        let out = cmd
            .output()
            .context("failed to run `jj` — is it installed and on PATH?")?;
        if !out.status.success() {
            bail!(
                "`jj {}` failed:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run jj for its side effects, letting its progress output through.
    pub fn run_visible(&self, args: &[&str]) -> Result<()> {
        let mut cmd = self.command();
        cmd.args(args);
        log::note(running(args));
        log::trace(log::command_line(&cmd));
        let status = cmd
            .status()
            .context("failed to run `jj` — is it installed and on PATH?")?;
        if !status.success() {
            bail!("`jj {}` failed", args.join(" "));
        }
        Ok(())
    }

    /// Changes matching `revset`, oldest first.
    pub fn changes(&self, revset: &str) -> Result<Vec<Change>> {
        let out = self.run(&[
            "log",
            "--no-graph",
            "--reversed",
            "-r",
            revset,
            "-T",
            LOG_TEMPLATE,
        ])?;
        out.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).with_context(|| format!("unexpected jj output: {l}")))
            .collect()
    }

    /// A revset for the changes `jj git push` refuses: undescribed or
    /// conflicted ones, plus whatever `git.private-commits` selects.
    pub fn unpushable_revset(&self) -> String {
        let private = self
            .run(&["config", "get", "git.private-commits"])
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "none()".to_string());
        format!("(description(exact:\"\") | conflicts() | ({private}))")
    }

    pub fn remote_url(&self, remote: &str) -> Result<String> {
        let out = self.run(&["git", "remote", "list"])?;
        out.lines()
            .filter_map(|l| l.split_once(' '))
            .find(|(name, _)| *name == remote)
            .map(|(_, url)| url.trim().to_string())
            .ok_or_else(|| anyhow!("no git remote named `{remote}` (see `jj git remote list`)"))
    }

    /// The prefix jj itself uses for generated bookmarks (`push-` by default).
    pub fn push_bookmark_prefix(&self) -> String {
        self.run(&["config", "get", "git.push-bookmark-prefix"])
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "push-".to_string())
    }

    pub fn create_bookmark(&self, name: &str, rev: &str) -> Result<()> {
        self.run(&["bookmark", "create", name, "-r", rev])
            .map(|_| ())
    }

    pub fn fetch(&self, remote: &str) -> Result<()> {
        self.run_visible(&["git", "fetch", "--remote", remote])
    }

    pub fn push(&self, remote: &str, bookmarks: &[String]) -> Result<()> {
        let mut args: Vec<&str> = vec!["git", "push", "--remote", remote];
        for b in bookmarks {
            args.push("--bookmark");
            args.push(b);
        }
        self.run_visible(&args)
    }

    /// The nearest default-branch name according to jj's `trunk()` alias, if
    /// it resolves to a remote bookmark.
    pub fn trunk_bookmark(&self) -> Option<String> {
        let out = self
            .run(&[
                "log",
                "--no-graph",
                "-r",
                "trunk()",
                "-T",
                r#"remote_bookmarks.map(|b| b.name()).join("\n")"#,
            ])
            .ok()?;
        out.lines().next().map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(desc: &str) -> Change {
        Change {
            change_id: "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk".into(),
            commit_id: "abc".into(),
            description: desc.into(),
            empty: false,
            conflict: false,
            parents: vec![],
            bookmarks: vec![],
        }
    }

    #[test]
    fn splits_title_and_body() {
        let c = change("Add a thing\n\nIt does stuff.\nMore stuff.\n");
        assert_eq!(c.title(), "Add a thing");
        assert_eq!(c.body(), "It does stuff.\nMore stuff.");
        assert_eq!(change("Just a title\n").body(), "");
        assert_eq!(c.short_id(), "kkkkkkkk");
    }

    #[test]
    fn body_drops_a_closing_block_of_co_authors() {
        let c = change("Title\n\nBody.\n\nCo-Authored-By: A <a@x>\nco-authored-by: B <b@x>\n");
        assert_eq!(c.body(), "Body.");
        assert_eq!(change("Title\n\nCo-Authored-By: A <a@x>\n").body(), "");
    }

    #[test]
    fn body_keeps_mixed_or_inline_trailers() {
        let mixed = "Title\n\nBody.\n\nCo-Authored-By: A <a@x>\nSigned-off-by: B <b@x>";
        assert_eq!(change(mixed).body(), mixed.split_once("\n\n").unwrap().1);
        let inline = "Title\n\nBody.\nCo-Authored-By: A <a@x>";
        assert_eq!(change(inline).body(), "Body.\nCo-Authored-By: A <a@x>");
    }
}
