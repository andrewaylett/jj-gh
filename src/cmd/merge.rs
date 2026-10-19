use anyhow::{Result, bail};

use super::{Ctx, sync};
use crate::github::PrState;

pub struct Args {
    pub revset: Option<String>,
    pub method: String,
    pub force: bool,
    pub submit: bool,
}

/// Merge the bottom PR of the stack, then restack what's left.
pub fn run(ctx: &Ctx, args: &Args) -> Result<()> {
    let client = ctx.client()?;
    let default_branch = client.default_branch()?;
    let segments = ctx.segments(&client, args.revset.as_deref(), &default_branch)?;
    let Some(bottom) = segments.first() else {
        bail!("the stack is empty");
    };
    if bottom.new_bookmark {
        bail!("the bottom of the stack hasn't been submitted; run `jj-gh submit`");
    }
    let Some(pr) = client
        .pull_requests_for(std::slice::from_ref(&bottom.bookmark))?
        .remove(0)
    else {
        bail!(
            "no pull request for {}; run `jj-gh submit`",
            bottom.bookmark
        );
    };
    if pr.state != PrState::Open {
        bail!("#{} is not open; run `jj-gh sync`", pr.number);
    }

    if !args.force {
        let mut problems = Vec::new();
        if pr.head_ref_oid != bottom.head().commit_id {
            problems.push("local changes haven't been pushed (run `jj-gh submit`)".to_string());
        }
        if pr.is_draft {
            problems.push("it is still a draft".into());
        }
        match pr.checks.as_deref() {
            Some("SUCCESS") | None => {}
            Some(other) => problems.push(format!("checks are {}", other.to_lowercase())),
        }
        match pr.review_decision.as_deref() {
            Some("CHANGES_REQUESTED") => problems.push("changes were requested".into()),
            Some("REVIEW_REQUIRED") => problems.push("a review is required".into()),
            _ => {}
        }
        if pr.mergeable.as_deref() == Some("CONFLICTING") {
            problems.push("it has merge conflicts".into());
        }
        if !problems.is_empty() {
            bail!(
                "#{} isn't ready to merge: {} (use --force to try anyway)",
                pr.number,
                problems.join("; ")
            );
        }
    }

    println!("Merging #{} {}", pr.number, pr.title);
    client.merge_pull_request(pr.number, &args.method)?;

    sync::run(
        ctx,
        &sync::Args {
            revset: args.revset.clone(),
            submit: args.submit,
        },
    )
}
