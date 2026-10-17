use std::process::Command;

use anyhow::{Result, bail};

use super::Ctx;

/// Print (and optionally open) the URL of the PR covering `rev`.
pub fn open(ctx: &Ctx, rev: Option<&str>, print_only: bool) -> Result<()> {
    let client = ctx.client()?;
    let default_branch = client.default_branch()?;
    let segments = ctx.segments(None, &default_branch)?;

    let segment = match rev {
        None => segments.iter().rev().find(|s| !s.new_bookmark),
        Some(rev) => {
            let target = ctx.jj.changes(rev)?;
            let Some(target) = target.first() else {
                bail!("`{rev}` matched no change");
            };
            segments
                .iter()
                .find(|s| s.changes.iter().any(|c| c.change_id == target.change_id))
        }
    };
    let Some(segment) = segment else {
        bail!("that change isn't part of a submitted stack");
    };
    let Some(pr) = client
        .pull_requests_for(std::slice::from_ref(&segment.bookmark))?
        .remove(0)
    else {
        bail!(
            "{} has no pull request yet; run `jj-gh submit`",
            segment.bookmark
        );
    };

    println!("{}", pr.url);
    if !print_only {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(windows) {
            "explorer"
        } else {
            "xdg-open"
        };
        // Best effort: the URL is already printed.
        let _ = Command::new(opener).arg(&pr.url).status();
    }
    Ok(())
}

/// Fetch a PR's branch and start a new change on top of it.
pub fn checkout(ctx: &Ctx, number: u64) -> Result<()> {
    let client = ctx.client()?;
    let pr = client.pull_request(number)?;
    let head_repo = client.head_repository(number)?;
    if head_repo.as_deref() != Some(&client.repo.slug()) {
        bail!(
            "#{number} comes from {}; only same-repository PRs can be checked out",
            head_repo.as_deref().unwrap_or("a deleted fork")
        );
    }
    ctx.jj.run_visible(&[
        "git",
        "fetch",
        "--remote",
        &ctx.remote,
        "--branch",
        &pr.head_ref_name,
    ])?;
    ctx.jj
        .run_visible(&["new", &format!("{}@{}", pr.head_ref_name, ctx.remote)])
}
