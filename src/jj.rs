//! Thin wrapper around the `jj` binary.
//!
//! We deliberately shell out rather than link jj-lib: the CLI is jj's stable
//! interface, and the templates below keep us independent of its human output.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

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

    /// Everything after the first line, trimmed.
    pub fn body(&self) -> &str {
        self.description
            .split_once('\n')
            .map(|(_, rest)| rest.trim())
            .unwrap_or("")
    }
}

const LOG_TEMPLATE: &str = r#"'{"change":' ++ json(change_id) ++
  ',"commit":' ++ json(commit_id) ++
  ',"desc":' ++ json(description) ++
  ',"empty":' ++ json(empty) ++
  ',"conflict":' ++ json(conflict) ++
  ',"parents":[' ++ parents.map(|p| json(p.commit_id())).join(",") ++ ']' ++
  ',"bookmarks":[' ++ local_bookmarks.map(|b| json(b.name())).join(",") ++ ']}' ++ "\n""#;

/// Split `LOG_TEMPLATE` output into one JSON document per change.
///
/// jj's `json()` can emit a raw newline inside a string (seen with a
/// `Claude-Session: <url>` trailer, where the space became a newline), which is
/// invalid JSON. Every record starts with `{"change":` and quotes inside
/// strings are always escaped, so any other line is a continuation of the
/// previous record and is rejoined with an escaped newline.
fn split_records(out: &str) -> Vec<String> {
    let mut records: Vec<String> = Vec::new();
    for line in out.lines() {
        match records.last_mut() {
            Some(last) if !line.starts_with(r#"{"change":"#) => {
                last.push_str("\\n");
                last.push_str(line);
            }
            _ if line.trim().is_empty() => {}
            _ => records.push(line.to_string()),
        }
    }
    records
}

#[derive(Debug, Clone, Default)]
pub struct Jj {
    pub repository: Option<PathBuf>,
}

impl Jj {
    fn command(&self) -> Command {
        let mut cmd = Command::new(std::env::var_os("JJ_GH_JJ").unwrap_or_else(|| "jj".into()));
        cmd.args(["--no-pager", "--color", "never"]);
        if let Some(repo) = &self.repository {
            cmd.arg("-R").arg(repo);
        }
        cmd
    }

    /// Run jj, returning stdout. stderr is surfaced on failure.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .args(args)
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
        let status = self
            .command()
            .args(args)
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
        split_records(&out)
            .iter()
            .map(|l| serde_json::from_str(l).with_context(|| format!("unexpected jj output: {l}")))
            .collect()
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
}
