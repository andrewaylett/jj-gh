//! Credential discovery: environment first, then the `gh` CLI.

use std::process::Command;

use anyhow::{Result, bail};

use crate::log;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub value: String,
    pub source: String,
}

/// Find a token for `host`, preferring `GITHUB_TOKEN`, then `GH_TOKEN`, then
/// whatever `gh auth token` hands back.
pub fn find_token(host: &str) -> Result<Token> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(Token {
                    value,
                    source: format!("${var}"),
                });
            }
        }
    }

    let mut gh = Command::new("gh");
    gh.args(["auth", "token", "--hostname", host]);
    let _step = log::step("Asking gh for a token");
    log::trace(log::command_line(&gh));
    match gh.output() {
        Ok(out) if out.status.success() => {
            let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if value.is_empty() {
                bail!("`gh auth token` returned nothing for {host}; try `gh auth login`");
            }
            Ok(Token {
                value,
                source: format!("gh auth token --hostname {host}"),
            })
        }
        Ok(out) => bail!(
            "no GITHUB_TOKEN set and `gh auth token` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(_) => bail!(
            "no GITHUB_TOKEN set and the `gh` CLI is not installed; \
             set GITHUB_TOKEN or install gh and run `gh auth login`"
        ),
    }
}
