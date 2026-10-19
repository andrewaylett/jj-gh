use anyhow::Result;

use super::{Ctx, paint, submit};
use crate::github::PrState;

pub struct Args {
    pub revset: Option<String>,
    pub submit: bool,
}

pub fn run(ctx: &Ctx, args: &Args) -> Result<()> {
    ctx.jj.fetch(&ctx.remote)?;

    let client = ctx.client()?;
    let default_branch = client.default_branch()?;
    let segments = ctx.segments(args.revset.as_deref(), &default_branch)?;
    if segments.is_empty() {
        println!("Stack is empty; nothing to sync.");
        return Ok(());
    }

    let existing: Vec<String> = segments
        .iter()
        .filter(|s| !s.new_bookmark)
        .map(|s| s.bookmark.clone())
        .collect();
    let mut found = client.pull_requests_for(&existing)?.into_iter();
    let states: Vec<Option<(u64, PrState)>> = segments
        .iter()
        .map(|s| {
            if s.new_bookmark {
                None
            } else {
                found.next().flatten().map(|p| (p.number, p.state))
            }
        })
        .collect();

    // Only a merged run at the bottom can be dropped; a merged PR above an
    // unmerged one would be a strange stack, so leave it alone and say so.
    let merged_prefix = states
        .iter()
        .take_while(|s| matches!(s, Some((_, PrState::Merged))))
        .count();
    for (i, s) in states.iter().enumerate().skip(merged_prefix) {
        match s {
            Some((n, PrState::Merged)) => eprintln!(
                "{} #{n} is merged but sits above unmerged PRs; leaving it in place",
                paint("33", "warning:")
            ),
            Some((n, PrState::Closed)) => eprintln!(
                "{} #{n} ({}) was closed without merging",
                paint("33", "warning:"),
                segments[i].bookmark
            ),
            _ => {}
        }
    }

    // Restack whatever is left onto the freshly fetched trunk.
    if let Some(first_kept) = segments.get(merged_prefix) {
        let id = &first_kept.changes[0].change_id;
        ctx.jj.run_visible(&["rebase", "-s", id, "-d", "trunk()"])?;
    }

    if merged_prefix > 0 {
        let ids: Vec<&str> = segments[..merged_prefix]
            .iter()
            .flat_map(|s| s.changes.iter().map(|c| c.change_id.as_str()))
            .collect();
        let mut abandon = vec!["abandon"];
        abandon.extend(ids);
        ctx.jj.run_visible(&abandon)?;
        for (s, st) in segments.iter().zip(&states).take(merged_prefix) {
            if let Some((n, _)) = st {
                println!("{} #{n} {}", paint("35", "merged"), s.title());
            }
        }
    }

    if args.submit {
        submit::run(
            ctx,
            &submit::Args {
                revset: args.revset.clone(),
                draft: false,
                dry_run: false,
                update_description: false,
                reviewers: vec![],
            },
        )
    } else {
        if segments.len() > merged_prefix {
            println!("Run `jj-gh submit` to push the restacked branches.");
        }
        Ok(())
    }
}
