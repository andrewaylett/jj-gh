//! The stack model: a linear run of jj changes above trunk, cut into
//! segments, each of which becomes one pull request.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};

use crate::github::PullRequest;
use crate::jj::{Change, Jj};

/// Changes `trunk()..@`, leaving out `@` if `jj git push` would refuse it.
pub fn default_revset(jj: &Jj) -> String {
    format!("trunk()..@ ~ (@ & {})", jj.unpushable_revset())
}

#[derive(Debug, Clone)]
pub struct Segment {
    /// Existing bookmark, or the name one will be created with on submit.
    pub bookmark: String,
    /// True when `bookmark` does not exist yet.
    pub new_bookmark: bool,
    /// Oldest first; the last element is the segment head.
    pub changes: Vec<Change>,
    /// Segments whose heads are parents of the first change. Always earlier in
    /// the list; empty when the segment sits directly on something outside it.
    pub parents: Vec<usize>,
}

impl Segment {
    pub fn head(&self) -> &Change {
        self.changes.last().expect("segments are never empty")
    }

    pub fn title(&self) -> &str {
        self.head().title()
    }

    pub fn pr_body(&self) -> String {
        let mut body = self.head().body().to_string();
        if self.changes.len() > 1 {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str("### Changes\n\n");
            for c in &self.changes {
                body.push_str(&format!("- {}\n", c.title()));
            }
        }
        body
    }

    /// Whether the PR's title and body are what `submit --update-description`
    /// would write. Line endings and surrounding whitespace don't count, since
    /// GitHub's web editor normalises them.
    pub fn description_matches(&self, pr: &PullRequest) -> bool {
        let norm = |s: &str| s.replace("\r\n", "\n").trim().to_string();
        norm(&pr.title) == norm(self.title()) && norm(&pr.body) == norm(&self.pr_body())
    }
}

pub struct Options<'a> {
    pub revset: &'a str,
    pub per_change: bool,
    pub prefix: &'a str,
    /// Bookmarks that must never be treated as stack members (the trunk).
    pub exclude: &'a [String],
    /// Allow forks and merges, instead of insisting on one linear chain.
    pub tree: bool,
}

/// Load the changes and cut them into segments. Fails if the revset isn't a
/// single linear chain, or if a change can't be turned into a PR.
pub fn load(jj: &Jj, opts: &Options) -> Result<Vec<Segment>> {
    let changes = jj.changes(opts.revset)?;
    segment(changes, opts)
}

pub fn segment(changes: Vec<Change>, opts: &Options) -> Result<Vec<Segment>> {
    for (i, c) in changes.iter().enumerate() {
        if !opts.tree {
            match (i, c.parents.as_slice()) {
                (0, [_]) => {}
                (0, _) => bail!(
                    "change {} has {} parents; the stack's first change must have exactly one",
                    c.short_id(),
                    c.parents.len()
                ),
                (_, [p]) if *p == changes[i - 1].commit_id => {}
                _ => bail!(
                    "the revset isn't a single linear stack (problem at change {}); \
                     pick one with -r",
                    c.short_id()
                ),
            }
        }
        if c.conflict {
            bail!("change {} has conflicts; resolve them first", c.short_id());
        }
        if c.title().is_empty() {
            bail!(
                "change {} has no description; run `jj describe {}`",
                c.short_id(),
                c.short_id()
            );
        }
    }

    // Oldest first means parents come before children. Only edges inside the
    // revset matter.
    let index: HashMap<&str, usize> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.commit_id.as_str(), i))
        .collect();
    let in_set: Vec<Vec<usize>> = changes
        .iter()
        .map(|c| {
            c.parents
                .iter()
                .filter_map(|p| index.get(p.as_str()).copied())
                .collect()
        })
        .collect();
    let mut children = vec![0usize; changes.len()];
    for p in in_set.iter().flatten() {
        children[*p] += 1;
    }
    let ends: Vec<bool> = (0..changes.len())
        .map(|i| {
            let c = &changes[i];
            let marked = c.bookmarks.iter().any(|b| !opts.exclude.contains(b));
            // A fork or a leaf ends it, and so does a child that is a merge.
            marked
                || opts.per_change
                || children[i] != 1
                || in_set.iter().any(|ps| ps.len() > 1 && ps.contains(&i))
        })
        .collect();

    // Each change either extends its only parent's segment or starts one.
    let mut seg_of = vec![0usize; changes.len()];
    let mut groups: Vec<(Vec<Change>, Vec<usize>)> = Vec::new();
    for (i, c) in changes.into_iter().enumerate() {
        match in_set[i].as_slice() {
            [p] if !ends[*p] => {
                seg_of[i] = seg_of[*p];
                groups[seg_of[i]].0.push(c);
            }
            ps => {
                let mut parents: Vec<usize> = ps.iter().map(|p| seg_of[*p]).collect();
                parents.dedup();
                seg_of[i] = groups.len();
                groups.push((vec![c], parents));
            }
        }
    }

    Ok(groups
        .into_iter()
        .map(|(changes, parents)| {
            let head = changes.last().unwrap();
            let existing = head
                .bookmarks
                .iter()
                .filter(|b| !opts.exclude.contains(b))
                .min()
                .cloned();
            let (bookmark, new_bookmark) = match existing {
                Some(b) => (b, false),
                None => (
                    format!(
                        "{}{}",
                        opts.prefix,
                        &head.change_id[..12.min(head.change_id.len())]
                    ),
                    true,
                ),
            };
            Segment {
                bookmark,
                new_bookmark,
                changes,
                parents,
            }
        })
        .collect())
}

/// The segment each one's PR should be based on, or `None` for the trunk.
///
/// That is its parent segment; for a merge, the nearest segment below all of
/// its parents, or `None` if that isn't a single segment. (`reparent` makes
/// the same call, but only among changes that have PRs.)
pub fn bases(segments: &[Segment]) -> Vec<Option<usize>> {
    // Segments are in topological order, so one pass fills in each one's ancestors.
    let mut below: Vec<HashSet<usize>> = Vec::with_capacity(segments.len());
    for s in segments {
        let mut set = HashSet::new();
        for &p in &s.parents {
            set.insert(p);
            set.extend(&below[p]);
        }
        below.push(set);
    }
    segments
        .iter()
        .map(|s| match s.parents.as_slice() {
            [] => None,
            [only] => Some(*only),
            many => {
                let mut shared = many.iter().map(|&p| {
                    let mut mine = below[p].clone();
                    mine.insert(p);
                    mine
                });
                let first = shared.next().unwrap_or_default();
                let common = shared.fold(first, |a, b| a.intersection(&b).copied().collect());
                // Nearest = not below any other common segment.
                let nearest: Vec<usize> = common
                    .iter()
                    .copied()
                    .filter(|c| !common.iter().any(|o| below[*o].contains(c)))
                    .collect();
                match nearest.as_slice() {
                    [one] => Some(*one),
                    _ => None,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: &str, parent: &str, desc: &str, bookmarks: &[&str]) -> Change {
        Change {
            change_id: format!("{id:k<32}"),
            commit_id: id.into(),
            description: desc.into(),
            empty: false,
            conflict: false,
            parents: vec![parent.into()],
            bookmarks: bookmarks.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn opts(per_change: bool) -> Options<'static> {
        Options {
            revset: "",
            per_change,
            prefix: "push-",
            exclude: &[],
            tree: false,
        }
    }

    #[test]
    fn groups_by_bookmark_and_names_the_tail() {
        let cs = vec![
            ch("a", "root", "one\n", &[]),
            ch("b", "a", "two\n", &["feature-b"]),
            ch("c", "b", "three\n", &[]),
        ];
        let segs = segment(cs, &opts(false)).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].bookmark, "feature-b");
        assert!(!segs[0].new_bookmark);
        assert_eq!(segs[0].changes.len(), 2);
        assert!(segs[1].new_bookmark);
        assert!(segs[1].bookmark.starts_with("push-ckkk"));
    }

    fn pr(title: &str, body: &str) -> PullRequest {
        serde_json::from_value(serde_json::json!({
            "number": 1, "title": title, "body": body, "url": "", "state": "OPEN",
            "isDraft": false, "baseRefName": "main", "headRefName": "b",
            "headRefOid": "a", "reviewDecision": null, "mergeable": null,
        }))
        .unwrap()
    }

    #[test]
    fn description_comparison_ignores_line_endings_and_padding() {
        let segs = segment(
            vec![ch("a", "root", "one\n\nbody text\n", &[])],
            &opts(false),
        )
        .unwrap();
        assert!(segs[0].description_matches(&pr("one", "body text")));
        assert!(segs[0].description_matches(&pr("one", "body text\r\n")));
        assert!(!segs[0].description_matches(&pr("one", "other")));
        assert!(!segs[0].description_matches(&pr("two", "body text")));
    }

    #[test]
    fn per_change_makes_one_segment_each() {
        let cs = vec![ch("a", "root", "one", &[]), ch("b", "a", "two", &[])];
        assert_eq!(segment(cs, &opts(true)).unwrap().len(), 2);
    }

    #[test]
    fn rejects_forks_and_undescribed_changes() {
        let forked = vec![ch("a", "root", "one", &[]), ch("b", "root", "two", &[])];
        assert!(segment(forked, &opts(false)).is_err());
        let blank = vec![ch("a", "root", "", &[])];
        assert!(segment(blank, &opts(false)).is_err());
    }

    #[test]
    fn excluded_bookmarks_are_ignored() {
        let excl = vec!["main".to_string()];
        let o = Options {
            exclude: &excl,
            ..opts(false)
        };
        let segs = segment(vec![ch("a", "root", "one", &["main"])], &o).unwrap();
        assert!(segs[0].new_bookmark);
    }

    fn tree_opts() -> Options<'static> {
        Options {
            tree: true,
            ..opts(false)
        }
    }

    #[test]
    fn a_tree_is_cut_at_forks_and_merges() {
        // a ─┬─ b1 ─┬─ c ─ d
        //    └─ b2 ─┘
        let cs = vec![
            ch("a", "root", "a", &[]),
            ch("b1", "a", "b1", &[]),
            ch("b2", "a", "b2", &[]),
            Change {
                parents: vec!["b1".into(), "b2".into()],
                ..ch("c", "", "c", &[])
            },
            ch("d", "c", "d", &[]),
        ];
        assert!(segment(cs.clone(), &opts(false)).is_err());
        let segs = segment(cs, &tree_opts()).unwrap();
        let sizes: Vec<usize> = segs.iter().map(|s| s.changes.len()).collect();
        assert_eq!(sizes, [1, 1, 1, 2]);
        assert_eq!(segs[3].parents, [1, 2]);
        assert_eq!(bases(&segs), [None, Some(0), Some(0), Some(0)]);
    }

    #[test]
    fn independent_branches_are_based_on_the_trunk() {
        let cs = vec![
            ch("a", "root", "a", &[]),
            ch("b", "root", "b", &[]),
            Change {
                parents: vec!["a".into(), "b".into()],
                ..ch("c", "", "c", &[])
            },
        ];
        let segs = segment(cs, &tree_opts()).unwrap();
        assert_eq!(bases(&segs), [None, None, None]);
    }
}
