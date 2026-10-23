//! Retarget PRs so their bases follow the commit tree rather than the order
//! they were stacked in, e.g. after parallelising part of a stack.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde_json::json;

use super::submit::{sync_native_stack, unstack_native};
use super::{Ctx, paint};
use crate::github::PrState;
use crate::jj::Change;

pub struct Args {
    pub revset: Option<String>,
    pub dry_run: bool,
}

/// An open PR whose head branch is on a change.
#[derive(Debug, Clone)]
struct Head {
    number: u64,
    title: String,
    branch: String,
    base: String,
}

#[derive(Debug, PartialEq, Eq)]
struct Move {
    number: u64,
    title: String,
    branch: String,
    from: String,
    to: String,
}

pub fn run(ctx: &Ctx, args: &Args) -> Result<()> {
    let client = ctx.client()?;
    let default_branch = client.default_branch()?;
    // What `jj git push` considers when given no arguments.
    let revset = args
        .revset
        .clone()
        .unwrap_or_else(|| format!("remote_bookmarks(remote=exact:{})..@", json!(ctx.remote)));
    let targets = ctx.jj.changes(&format!("({revset}) & mutable()"))?;
    if targets.is_empty() {
        println!("Nothing to reparent: no mutable changes in {revset}.");
        return Ok(());
    }
    // Parents may be outside the revset, e.g. already pushed and unchanged.
    let graph = ctx.jj.changes(&format!("mutable() & ::({revset})"))?;

    let exclude = ctx.excluded_bookmarks(&default_branch);
    let mut names: Vec<String> = graph
        .iter()
        .flat_map(|c| c.bookmarks.iter().cloned())
        .filter(|b| !exclude.contains(b))
        .collect();
    names.sort();
    names.dedup();
    let open: HashMap<String, Head> = names
        .iter()
        .cloned()
        .zip(client.pull_requests_for(&names)?)
        .filter_map(|(name, pr)| Some((name, pr?)))
        .filter(|(_, pr)| pr.state == PrState::Open)
        .map(|(name, pr)| {
            let head = Head {
                number: pr.number,
                title: pr.title,
                branch: name.clone(),
                base: pr.base_ref_name,
            };
            (name, head)
        })
        .collect();
    let heads: HashMap<String, Head> = graph
        .iter()
        .filter_map(|c| {
            let mut marks = c.bookmarks.clone();
            marks.sort();
            let head = marks.iter().find_map(|b| open.get(b))?;
            Some((c.commit_id.clone(), head.clone()))
        })
        .collect();

    let target_ids: HashSet<&str> = targets.iter().map(|c| c.commit_id.as_str()).collect();
    let moves = plan(&graph, &target_ids, &heads, &default_branch);
    if moves.is_empty() {
        println!("Every pull request already has the right base.");
        return Ok(());
    }

    if args.dry_run {
        println!("Would retarget {} pull request(s):", moves.len());
        for m in &moves {
            report("retarget", m);
        }
        return Ok(());
    }

    // The branches must hold the rewritten commits before GitHub compares them with a new base.
    let branches: Vec<String> = moves.iter().map(|m| m.branch.clone()).collect();
    ctx.jj.push(&ctx.remote, &branches)?;

    // GitHub refuses to change the base of a PR in a native stack.
    let numbers: Vec<u64> = moves.iter().map(|m| m.number).collect();
    let dissolved = unstack_native(&client, &numbers).unwrap_or_else(|e| {
        eprintln!(
            "{} couldn't dissolve GitHub's native stack before retargeting: {e:#}",
            paint("33", "warning:")
        );
        false
    });
    for m in &moves {
        client.update_pull_request(m.number, json!({ "base": m.to }))?;
        report("retargeted", m);
    }

    if dissolved {
        let now: Vec<(u64, String, String)> = heads
            .values()
            .map(|h| {
                let base = moves
                    .iter()
                    .find(|m| m.number == h.number)
                    .map_or(&h.base, |m| &m.to);
                (h.number, h.branch.clone(), base.clone())
            })
            .collect();
        match chain(&now) {
            Some(order) if order.len() > 1 => {
                if let Err(e) = sync_native_stack(&client, &order) {
                    eprintln!(
                        "{} couldn't update GitHub's native stack: {e:#}",
                        paint("33", "warning:")
                    );
                }
            }
            _ => println!(
                "{} the PRs no longer form a single stack, and GitHub's native stacks are linear, so the stack was dissolved",
                paint("33", "note:")
            ),
        }
    }
    Ok(())
}

fn report(verb: &str, m: &Move) {
    println!(
        "{} #{} {} ({} → {})",
        paint("32", verb),
        m.number,
        m.title,
        m.from,
        m.to
    );
}

/// All proper ancestors of `id` within the graph.
fn ancestors<'a>(by_id: &HashMap<&'a str, &'a Change>, id: &str) -> HashSet<&'a str> {
    let mut seen = HashSet::new();
    let mut todo: Vec<&'a str> = by_id
        .get(id)
        .map(|c| c.parents.iter().map(String::as_str).collect())
        .unwrap_or_default();
    while let Some(p) = todo.pop() {
        if let Some(c) = by_id.get(p)
            && seen.insert(c.commit_id.as_str())
        {
            todo.extend(c.parents.iter().map(String::as_str));
        }
    }
    seen
}

/// Members of `set` that aren't an ancestor of another member.
fn maximal<'a>(by_id: &HashMap<&'a str, &'a Change>, set: &HashSet<&'a str>) -> Vec<&'a str> {
    let below: HashSet<&str> = set.iter().flat_map(|id| ancestors(by_id, id)).collect();
    set.iter()
        .copied()
        .filter(|id| !below.contains(id))
        .collect()
}

/// For each target with an open PR, the base it should have: the nearest PR
/// below it, or if there are several, the nearest PR they all sit on, or the
/// default branch if there isn't exactly one of those. Returns only the PRs
/// whose base is wrong, oldest first.
fn plan(
    graph: &[Change],
    targets: &HashSet<&str>,
    heads: &HashMap<String, Head>,
    default_branch: &str,
) -> Vec<Move> {
    let by_id: HashMap<&str, &Change> = graph.iter().map(|c| (c.commit_id.as_str(), c)).collect();
    let head_ids: HashSet<&str> = heads.keys().map(String::as_str).collect();
    let mut moves = Vec::new();
    for c in graph {
        let id = c.commit_id.as_str();
        let (true, Some(head)) = (targets.contains(id), heads.get(id)) else {
            continue;
        };
        let below: HashSet<&str> = ancestors(&by_id, id)
            .intersection(&head_ids)
            .copied()
            .collect();
        let parents = maximal(&by_id, &below);
        let base = match parents.as_slice() {
            [] => None,
            [only] => Some(*only),
            many => {
                // Everything below all of them, counting each of them too.
                let mut shared: Option<HashSet<&str>> = None;
                for &p in many {
                    let mut mine = ancestors(&by_id, p);
                    mine.insert(p);
                    mine.retain(|a| head_ids.contains(a));
                    shared = Some(match shared {
                        None => mine,
                        Some(s) => s.intersection(&mine).copied().collect(),
                    });
                }
                match maximal(&by_id, &shared.unwrap_or_default()).as_slice() {
                    [one] => Some(*one),
                    _ => None,
                }
            }
        };
        let to = base.map_or(default_branch, |b| heads[b].branch.as_str());
        if head.base != to {
            moves.push(Move {
                number: head.number,
                title: head.title.clone(),
                branch: head.branch.clone(),
                from: head.base.clone(),
                to: to.to_string(),
            });
        }
    }
    moves
}

/// PRs as (number, head branch, base branch). The numbers bottom to top, if
/// the bases make them one linear stack.
fn chain(prs: &[(u64, String, String)]) -> Option<Vec<u64>> {
    let mut roots = prs
        .iter()
        .filter(|(_, _, base)| !prs.iter().any(|(_, head, _)| head == base));
    let mut cur = roots.next()?;
    if roots.next().is_some() {
        return None;
    }
    let mut order = vec![cur.0];
    loop {
        let mut kids = prs.iter().filter(|(_, _, base)| *base == cur.1);
        match (kids.next(), kids.next()) {
            (None, _) => break,
            (Some(kid), None) if order.len() <= prs.len() => {
                order.push(kid.0);
                cur = kid;
            }
            _ => return None,
        }
    }
    (order.len() == prs.len()).then_some(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: &str, parents: &[&str]) -> Change {
        Change {
            change_id: format!("{id:k<32}"),
            commit_id: id.into(),
            description: id.into(),
            empty: false,
            conflict: false,
            parents: parents.iter().map(|s| s.to_string()).collect(),
            bookmarks: vec![],
        }
    }

    /// PRs numbered in the order given, each with branch `b-<id>`.
    fn heads(prs: &[(&str, &str)]) -> HashMap<String, Head> {
        prs.iter()
            .enumerate()
            .map(|(i, (id, base))| {
                (
                    id.to_string(),
                    Head {
                        number: i as u64 + 1,
                        title: id.to_string(),
                        branch: format!("b-{id}"),
                        base: base.to_string(),
                    },
                )
            })
            .collect()
    }

    fn everything(graph: &[Change]) -> HashSet<&str> {
        graph.iter().map(|c| c.commit_id.as_str()).collect()
    }

    fn summary(moves: &[Move]) -> Vec<(&str, &str)> {
        moves
            .iter()
            .map(|m| (m.title.as_str(), m.to.as_str()))
            .collect()
    }

    #[test]
    fn parallelised_changes_and_the_merge_above_them_move() {
        let g = [
            ch("a", &["root"]),
            ch("b1", &["a"]),
            ch("b2", &["a"]),
            ch("c", &["b1", "b2"]),
        ];
        // Stacked linearly on GitHub, as before the rebase.
        let h = heads(&[("a", "main"), ("b1", "b-a"), ("b2", "b-b1"), ("c", "b-b2")]);
        let moves = plan(&g, &everything(&g), &h, "main");
        assert_eq!(summary(&moves), [("b2", "b-a"), ("c", "b-a")]);
        assert_eq!(moves[1].from, "b-b2");
    }

    #[test]
    fn a_common_ancestor_without_a_pr_falls_back_to_the_nearest_one_below() {
        let g = [
            ch("a", &["root"]),
            ch("x", &["a"]),
            ch("b1", &["x"]),
            ch("b2", &["x"]),
            ch("c", &["b1", "b2"]),
        ];
        let h = heads(&[("a", "main"), ("b1", "b-a"), ("b2", "b-b1"), ("c", "b-b2")]);
        let moves = plan(&g, &everything(&g), &h, "main");
        assert_eq!(summary(&moves), [("b2", "b-a"), ("c", "b-a")]);
    }

    #[test]
    fn no_common_pr_means_the_default_branch() {
        let g = [
            ch("b1", &["root"]),
            ch("b2", &["root"]),
            ch("c", &["b1", "b2"]),
        ];
        let h = heads(&[("b1", "main"), ("b2", "b-b1"), ("c", "b-b2")]);
        let moves = plan(&g, &everything(&g), &h, "main");
        assert_eq!(summary(&moves), [("b2", "main"), ("c", "main")]);
    }

    #[test]
    fn only_targets_move_but_others_still_serve_as_bases() {
        let g = [ch("a", &["root"]), ch("b", &["a"]), ch("c", &["b"])];
        let h = heads(&[("a", "stale"), ("b", "stale"), ("c", "stale")]);
        let only_c: HashSet<&str> = ["c"].into();
        let moves = plan(&g, &only_c, &h, "main");
        assert_eq!(summary(&moves), [("c", "b-b")]);
    }

    #[test]
    fn changes_without_a_pr_are_looked_through() {
        let g = [ch("a", &["root"]), ch("x", &["a"]), ch("b", &["x"])];
        let h = heads(&[("a", "main"), ("b", "main")]);
        let moves = plan(&g, &everything(&g), &h, "main");
        assert_eq!(summary(&moves), [("b", "b-a")]);
    }

    #[test]
    fn a_correct_tree_needs_no_moves() {
        let g = [ch("a", &["root"]), ch("b", &["a"])];
        let h = heads(&[("a", "main"), ("b", "b-a")]);
        assert!(plan(&g, &everything(&g), &h, "main").is_empty());
    }

    fn prs(list: &[(u64, &str, &str)]) -> Vec<(u64, String, String)> {
        list.iter()
            .map(|(n, h, b)| (*n, h.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn chains_are_found_only_when_linear() {
        let linear = prs(&[(2, "b", "a"), (1, "a", "main"), (3, "c", "b")]);
        assert_eq!(chain(&linear), Some(vec![1, 2, 3]));
        let forked = prs(&[(1, "a", "main"), (2, "b", "a"), (3, "c", "a")]);
        assert_eq!(chain(&forked), None);
        let two_roots = prs(&[(1, "a", "main"), (2, "b", "main")]);
        assert_eq!(chain(&two_roots), None);
    }
}
