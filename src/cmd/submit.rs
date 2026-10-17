use anyhow::{Result, bail};
use serde_json::json;

use super::{Ctx, paint};
use crate::github::{PrState, PullRequest};
use crate::stack::{self, Segment};

pub struct Args {
    pub revset: Option<String>,
    pub draft: bool,
    pub dry_run: bool,
    pub update_description: bool,
    pub reviewers: Vec<String>,
}

pub fn run(ctx: &Ctx, args: &Args) -> Result<()> {
    let client = ctx.client()?;
    let default_branch = client.default_branch()?;
    let segments = ctx.segments(args.revset.as_deref(), &default_branch)?;
    if segments.is_empty() {
        println!("Nothing to submit: no changes between trunk and the working copy.");
        return Ok(());
    }

    let existing: Vec<String> = segments
        .iter()
        .filter(|s| !s.new_bookmark)
        .map(|s| s.bookmark.clone())
        .collect();
    let mut found = client.pull_requests_for(&existing)?.into_iter();
    let mut prs: Vec<Option<PullRequest>> = Vec::new();
    for s in &segments {
        let pr = if s.new_bookmark {
            None
        } else {
            found.next().flatten()
        };
        match pr {
            Some(p) if p.state == PrState::Merged => bail!(
                "#{} ({}) is already merged; run `jj-gh sync` to tidy the stack first",
                p.number,
                s.bookmark
            ),
            Some(p) if p.state == PrState::Closed => prs.push(None),
            other => prs.push(other),
        }
    }

    let base_of = |i: usize| -> &str {
        if i == 0 {
            &default_branch
        } else {
            &segments[i - 1].bookmark
        }
    };

    if args.dry_run {
        println!(
            "Would submit {} pull request(s) to {}:",
            segments.len(),
            client.repo.slug()
        );
        for (i, s) in segments.iter().enumerate().rev() {
            let what = match &prs[i] {
                Some(p) => format!("update #{}", p.number),
                None => "create".to_string(),
            };
            println!("  {what}: {} ({} → {})", s.title(), s.bookmark, base_of(i));
        }
        return Ok(());
    }

    // 1. Make sure every segment head has a bookmark, then push the lot.
    for s in segments.iter().filter(|s| s.new_bookmark) {
        ctx.jj.create_bookmark(&s.bookmark, &s.head().commit_id)?;
    }
    let names: Vec<String> = segments.iter().map(|s| s.bookmark.clone()).collect();
    ctx.jj.push(&ctx.remote, &names)?;

    // 2. Create or retarget PRs, bottom first so each base exists.
    let mut numbers: Vec<u64> = Vec::new();
    let mut current: Vec<PullRequest> = Vec::new();
    for (i, s) in segments.iter().enumerate() {
        let base = base_of(i);
        match &prs[i] {
            Some(pr) => {
                let mut patch = serde_json::Map::new();
                if pr.base_ref_name != base {
                    patch.insert("base".into(), json!(base));
                }
                if args.update_description {
                    patch.insert("title".into(), json!(s.title()));
                    patch.insert("body".into(), json!(s.pr_body()));
                }
                let changed = !patch.is_empty();
                if changed {
                    client.update_pull_request(pr.number, patch.into())?;
                }
                report(
                    if changed { "updated" } else { "up to date" },
                    pr.number,
                    &pr.url,
                    s,
                );
                numbers.push(pr.number);
                let mut p = pr.clone();
                if args.update_description {
                    p.body = s.pr_body();
                }
                current.push(p);
            }
            None => {
                let created = client.create_pull_request(
                    s.title(),
                    &s.pr_body(),
                    &s.bookmark,
                    base,
                    args.draft,
                )?;
                client.request_reviewers(created.number, &args.reviewers)?;
                report("created", created.number, &created.url, s);
                numbers.push(created.number);
                current.push(PullRequest {
                    number: created.number,
                    title: s.title().to_string(),
                    url: created.url,
                    body: s.pr_body(),
                    state: PrState::Open,
                    is_draft: args.draft,
                    base_ref_name: base.to_string(),
                    head_ref_name: s.bookmark.clone(),
                    head_ref_oid: s.head().commit_id.clone(),
                    review_decision: None,
                    mergeable: None,
                    checks: None,
                });
            }
        }
    }

    // 3. Now that every number is known, refresh the stack section.
    for (i, pr) in current.iter().enumerate() {
        let section = (numbers.len() > 1).then(|| stack::stack_section(&numbers, i));
        let body = stack::with_stack_section(&pr.body, section.as_deref());
        if body.trim_end() != pr.body.trim_end() {
            client.update_pull_request(pr.number, json!({ "body": body }))?;
        }
    }
    Ok(())
}

fn report(verb: &str, number: u64, url: &str, seg: &Segment) {
    println!(
        "{} #{number} {} {}",
        paint("32", verb),
        seg.title(),
        paint("2", url)
    );
}
