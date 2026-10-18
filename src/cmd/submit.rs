use anyhow::{Result, bail};
use serde_json::json;

use super::{Ctx, paint};
use crate::github::{Client, PrState, PullRequest};
use crate::stack::Segment;

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
            }
        }
    }

    // 3. Tell GitHub about the stack, so its own stack UI appears.
    if numbers.len() > 1
        && let Err(e) = sync_native_stack(&client, &numbers)
    {
        eprintln!(
            "{} couldn't update GitHub's native stack: {e:#}",
            paint("33", "warning:")
        );
    }
    Ok(())
}

/// Make GitHub's stack for these PRs (bottom first) match `numbers`, creating,
/// extending, or rebuilding it as needed.
fn sync_native_stack(client: &Client, numbers: &[u64]) -> Result<()> {
    let mut existing = None;
    for &n in numbers {
        if let Some(s) = client.stack_containing(n)? {
            existing = Some(s);
            break;
        }
    }
    match existing {
        None => client.create_stack(numbers),
        Some(s) if s.open == numbers => Ok(()),
        Some(s) if numbers.starts_with(&s.open) => {
            client.add_to_stack(s.number, &numbers[s.open.len()..])
        }
        Some(s) => {
            client.unstack(s.number)?;
            client.create_stack(numbers)
        }
    }
}

fn report(verb: &str, number: u64, url: &str, seg: &Segment) {
    println!(
        "{} #{number} {} {}",
        paint("32", verb),
        seg.title(),
        paint("2", url)
    );
}
