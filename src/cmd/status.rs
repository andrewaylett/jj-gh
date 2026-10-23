use anyhow::Result;

use super::{Ctx, paint};
use crate::github::{PrState, PullRequest};
use crate::stack::{self, Segment};

pub fn run(ctx: &Ctx, revset: Option<&str>) -> Result<()> {
    let client = ctx.client()?;
    let default_branch = client.default_branch()?;
    let segments = ctx.segment_tree(&client, revset, &default_branch)?;
    if segments.is_empty() {
        println!("No changes between trunk and the working copy.");
        return Ok(());
    }

    let existing: Vec<String> = segments
        .iter()
        .filter(|s| !s.new_bookmark)
        .map(|s| s.bookmark.clone())
        .collect();
    let mut found = client.pull_requests_for(&existing)?.into_iter();
    let prs: Vec<Option<PullRequest>> = segments
        .iter()
        .map(|s| {
            if s.new_bookmark {
                None
            } else {
                found.next().flatten()
            }
        })
        .collect();

    // Newest on top, like `jj log`. A segment that doesn't sit on the one
    // printed just below it says what it is based on.
    let bases = stack::bases(&segments);
    for i in (0..segments.len()).rev() {
        let expected_base = match bases[i] {
            Some(b) => &segments[b].bookmark,
            None => &default_branch,
        };
        let follows_previous = match segments[i].parents.as_slice() {
            [] => i == 0,
            [p] => *p + 1 == i,
            _ => false,
        };
        let sits_on = (!follows_previous).then(|| match segments[i].parents.as_slice() {
            [] => default_branch.clone(),
            ps => ps
                .iter()
                .map(|&p| match &prs[p] {
                    Some(pr) => format!("#{}", pr.number),
                    None => segments[p].bookmark.clone(),
                })
                .collect::<Vec<_>>()
                .join(" + "),
        });
        print_segment(
            &segments[i],
            prs[i].as_ref(),
            expected_base,
            sits_on.as_deref(),
        );
    }
    println!("{} {}", paint("36", "◆"), default_branch);
    Ok(())
}

fn print_segment(
    seg: &Segment,
    pr: Option<&PullRequest>,
    expected_base: &str,
    sits_on: Option<&str>,
) {
    let head = seg.head();
    let (label, mut notes) = match pr {
        Some(pr) => (format!("#{}", pr.number), pr_notes(pr)),
        None => ("new".to_string(), vec![paint("33", "not submitted")]),
    };
    if let Some(pr) = pr
        && pr.state == PrState::Open
    {
        if pr.head_ref_oid != head.commit_id {
            notes.push(paint("33", "local changes not pushed"));
        }
        if pr.base_ref_name != expected_base {
            notes.push(paint(
                "33",
                &format!("base is {}, expected {expected_base}", pr.base_ref_name),
            ));
        }
        if !seg.description_matches(pr) {
            notes.push(paint("33", "description differs"));
        }
    }
    if let Some(on) = sits_on {
        notes.push(paint("36", &format!("on {on}")));
    }
    let marker = match pr.map(|p| p.state) {
        Some(PrState::Merged) => paint("35", "✔"),
        Some(PrState::Closed) => paint("31", "✖"),
        _ => "○".to_string(),
    };
    println!("{marker} {} {}", paint("1", &label), seg.title());
    let n = seg.changes.len();
    let span = if n > 1 {
        format!("{n} changes, head {}", head.short_id())
    } else {
        format!("change {}", head.short_id())
    };
    let mut line = format!("│   {}  {}", paint("2", &seg.bookmark), paint("2", &span));
    if !notes.is_empty() {
        line.push_str("  ");
        line.push_str(&notes.join(" · "));
    }
    println!("{line}");
    if let Some(pr) = pr {
        println!("│   {}", paint("2", &pr.url));
    }
}

fn pr_notes(pr: &PullRequest) -> Vec<String> {
    let mut notes = Vec::new();
    match pr.state {
        PrState::Merged => notes.push(paint("35", "merged")),
        PrState::Closed => notes.push(paint("31", "closed")),
        PrState::Open => {
            if pr.is_draft {
                notes.push(paint("2", "draft"));
            }
            match pr.checks.as_deref() {
                Some("SUCCESS") => notes.push(paint("32", "checks ✓")),
                Some("FAILURE") | Some("ERROR") => notes.push(paint("31", "checks ✗")),
                Some(_) => notes.push(paint("33", "checks …")),
                None => {}
            }
            match pr.review_decision.as_deref() {
                Some("APPROVED") => notes.push(paint("32", "approved")),
                Some("CHANGES_REQUESTED") => notes.push(paint("31", "changes requested")),
                Some("REVIEW_REQUIRED") => notes.push(paint("33", "review required")),
                _ => {}
            }
            if pr.mergeable.as_deref() == Some("CONFLICTING") {
                notes.push(paint("31", "conflicts"));
            }
        }
    }
    notes
}
