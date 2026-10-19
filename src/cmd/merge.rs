use anyhow::{Result, bail};

use super::{Ctx, paint, sync};
use crate::github::{MergeOutcome, PrState, PullRequest};
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

    // Only submitted segments can have PRs, and they come first.
    let limit = if args.all { segments.len() } else { 1 };
    let submitted: Vec<String> = segments
        .iter()
        .take(limit)
        .take_while(|s| !s.new_bookmark)
        .map(|s| s.bookmark.clone())
        .collect();
    let mut found = client.pull_requests_for(&submitted)?.into_iter();

    let mut ready: Vec<PullRequest> = Vec::new();
    for segment in segments.iter().take(limit) {
        let pr = if segment.new_bookmark {
            None
        } else {
            found.next().flatten()
        };
        match assess(segment, pr, args.force) {
            Ok(pr) => ready.push(pr),
            Err(why) if ready.is_empty() => bail!("{why}"),
            // We can still merge what's below, so just say where we stopped.
            Err(why) => {
                println!("Stopping before {}: {why}", segment.bookmark);
                break;
            }
        }
    }

    // GitHub merges "up to and including" a PR only within a native stack. If these aren't
    // in one (it may be disabled, or `submit` only warned), merging the top one would land
    // it on its parent's branch, so settle for the bottom.
    if ready.len() > 1 {
        let in_stack = client
            .stack_containing(ready[0].number)
            .ok()
            .flatten()
            .map(|s| s.open)
            .unwrap_or_default();
        let keep = ready
            .iter()
            .zip(&in_stack)
            .take_while(|(pr, n)| pr.number == **n)
            .count()
            .max(1);
        if keep < ready.len() {
            println!(
                "{} #{} isn't in the same GitHub stack as the PR below it; merging only up to #{}",
                paint("33", "warning:"),
                ready[keep].number,
                ready[keep - 1].number
            );
            ready.truncate(keep);
        }
    }

    let top = ready.last().expect("ready is never empty here");
    for pr in &ready {
        println!("Merging #{} {} ({method})", pr.number, pr.title);
    }
    match client.merge_pull_request(top.number, &method, &top.head_ref_oid)? {
        MergeOutcome::Enqueued => {
            println!(
                "#{} was added to the merge queue; run `jj-gh sync` once it lands",
                top.number
            );
            Ok(())
        }
        MergeOutcome::Merged => sync::run(
            ctx,
            &sync::Args {
                revset: args.revset.clone(),
                submit: args.submit,
            },
        ),
    }
}

/// The PR for `segment` if it's fit to merge, otherwise the reason it isn't.
fn assess(
    segment: &Segment,
    pr: Option<PullRequest>,
    force: bool,
) -> std::result::Result<PullRequest, String> {
    if segment.new_bookmark {
        return Err(format!(
            "{} hasn't been submitted; run `jj-gh submit`",
            segment.bookmark
        ));
    }
    let Some(pr) = pr else {
        return Err(format!(
            "no pull request for {}; run `jj-gh submit`",
            segment.bookmark
        ));
    };
    if pr.state != PrState::Open {
        return Err(format!("#{} is not open; run `jj-gh sync`", pr.number));
    }
    if !force {
        let problems = problems(&pr, segment);
        if !problems.is_empty() {
            return Err(format!(
                "#{} isn't ready to merge: {} (use --force to try anyway)",
                pr.number,
                problems.join("; ")
            ));
        }
    }
    Ok(pr)
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
