use anyhow::{Result, bail};

use super::{Ctx, sync};
use crate::github::{Client, PrState, PullRequest};
use crate::stack::Segment;

pub struct Args {
    pub revset: Option<String>,
    pub method: Option<String>,
    pub force: bool,
    pub all: bool,
    pub submit: bool,
}

/// Rebase if the repository allows it, otherwise the first method it does allow
/// (allowed methods come in preference order). An explicit request is passed
/// through untouched so GitHub can explain why it's refused.
fn choose_method(requested: Option<&str>, allowed: &[&str]) -> Result<String> {
    match (requested, allowed.first()) {
        (Some(m), _) => Ok(m.to_string()),
        (None, Some(m)) => Ok((*m).to_string()),
        (None, None) => bail!("the repository doesn't allow any merge method"),
    }
}

/// Why a PR shouldn't be merged yet, empty if it's ready.
fn problems(pr: &PullRequest, segment: &Segment) -> Vec<String> {
    let mut problems = Vec::new();
    if pr.head_ref_oid != segment.head().commit_id {
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
    problems
}

/// Merge the bottom PR of the stack (or, with `all`, as many as are ready, bottom first),
/// then restack what's left.
pub fn run(ctx: &Ctx, args: &Args) -> Result<()> {
    let client = ctx.client()?;
    let default_branch = client.default_branch()?;
    let segments = ctx.segments(&client, args.revset.as_deref(), &default_branch)?;
    if segments.is_empty() {
        bail!("the stack is empty");
    }
    let method = choose_method(args.method.as_deref(), &client.allowed_merge_methods()?)?;
    let limit = if args.all { segments.len() } else { 1 };

    let mut merged = 0;
    let mut failure = None;
    for (i, segment) in segments.iter().take(limit).enumerate() {
        // Fetched afresh each time: merging the PR below changes what GitHub says about this one.
        match merge_one(&client, segment, &method, args.force) {
            Ok(Step::Merged) => merged += 1,
            // Not the first, so we've done something useful already; just say where we stopped.
            Ok(Step::Stopped(why)) if i > 0 => {
                println!("Stopping before {}: {why}", segment.bookmark);
                break;
            }
            Ok(Step::Stopped(why)) => bail!("{why}"),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }

    // Restack even after a failure part-way, so what did merge doesn't leave the stack stale.
    if merged > 0 {
        sync::run(
            ctx,
            &sync::Args {
                revset: args.revset.clone(),
                submit: args.submit,
            },
        )?;
    }
    failure.map_or(Ok(()), Err)
}

enum Step {
    Merged,
    Stopped(String),
}

fn merge_one(client: &Client, segment: &Segment, method: &str, force: bool) -> Result<Step> {
    if segment.new_bookmark {
        return Ok(Step::Stopped(format!(
            "{} hasn't been submitted; run `jj-gh submit`",
            segment.bookmark
        )));
    }
    let Some(pr) = client
        .pull_requests_for(std::slice::from_ref(&segment.bookmark))?
        .remove(0)
    else {
        return Ok(Step::Stopped(format!(
            "no pull request for {}; run `jj-gh submit`",
            segment.bookmark
        )));
    };
    if pr.state != PrState::Open {
        return Ok(Step::Stopped(format!(
            "#{} is not open; run `jj-gh sync`",
            pr.number
        )));
    }
    if !force {
        let problems = problems(&pr, segment);
        if !problems.is_empty() {
            return Ok(Step::Stopped(format!(
                "#{} isn't ready to merge: {} (use --force to try anyway)",
                pr.number,
                problems.join("; ")
            )));
        }
    }

    println!("Merging #{} {} ({method})", pr.number, pr.title);
    client.merge_pull_request(pr.number, method)?;
    Ok(Step::Merged)
}

#[cfg(test)]
mod tests {
    use super::choose_method;

    #[test]
    fn prefers_what_the_repository_lists_first() {
        assert_eq!(
            choose_method(None, &["rebase", "squash"]).unwrap(),
            "rebase"
        );
        assert_eq!(choose_method(None, &["squash", "merge"]).unwrap(), "squash");
    }

    #[test]
    fn explicit_method_wins() {
        assert_eq!(choose_method(Some("merge"), &["rebase"]).unwrap(), "merge");
    }

    #[test]
    fn no_allowed_method_is_an_error() {
        assert!(choose_method(None, &[]).is_err());
    }
}
