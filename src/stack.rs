//! The stack model: a linear run of jj changes above trunk, cut into
//! segments, each of which becomes one pull request.

use anyhow::{Result, bail};

use crate::jj::{Change, Jj};

/// Changes `trunk()..@`, ignoring an empty, undescribed working copy.
pub const DEFAULT_REVSET: &str = "trunk()..@ ~ (@ & empty() & description(exact:\"\"))";

#[derive(Debug, Clone)]
pub struct Segment {
    /// Existing bookmark, or the name one will be created with on submit.
    pub bookmark: String,
    /// True when `bookmark` does not exist yet.
    pub new_bookmark: bool,
    /// Oldest first; the last element is the segment head.
    pub changes: Vec<Change>,
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
}

pub struct Options<'a> {
    pub revset: &'a str,
    pub per_change: bool,
    pub prefix: &'a str,
    /// Bookmarks that must never be treated as stack members (the trunk).
    pub exclude: &'a [String],
}

/// Load the changes and cut them into segments. Fails if the revset isn't a
/// single linear chain, or if a change can't be turned into a PR.
pub fn load(jj: &Jj, opts: &Options) -> Result<Vec<Segment>> {
    let changes = jj.changes(opts.revset)?;
    segment(changes, opts)
}

pub fn segment(changes: Vec<Change>, opts: &Options) -> Result<Vec<Segment>> {
    for (i, c) in changes.iter().enumerate() {
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

    let mut segments = Vec::new();
    let mut pending: Vec<Change> = Vec::new();
    let n = changes.len();
    for (i, c) in changes.into_iter().enumerate() {
        let existing = c
            .bookmarks
            .iter()
            .filter(|b| !opts.exclude.contains(b))
            .min()
            .cloned();
        pending.push(c);
        let last = i + 1 == n;
        if existing.is_some() || opts.per_change || last {
            let head = pending.last().unwrap();
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
            segments.push(Segment {
                bookmark,
                new_bookmark,
                changes: std::mem::take(&mut pending),
            });
        }
    }
    Ok(segments)
}

pub const MARKER_START: &str = "<!-- jj-gh:stack:start -->";
pub const MARKER_END: &str = "<!-- jj-gh:stack:end -->";

/// Render the managed stack section for the PR at `current` (an index into `numbers`).
pub fn stack_section(numbers: &[u64], current: usize) -> String {
    let mut s = String::new();
    s.push_str(MARKER_START);
    s.push_str("\n---\n**Stack** (bottom merges first), managed by [jj-gh](https://github.com/andrewaylett/jj-gh):\n\n");
    for (i, n) in numbers.iter().enumerate() {
        if i == current {
            s.push_str(&format!("{}. **#{n}** ← this PR\n", i + 1));
        } else {
            s.push_str(&format!("{}. #{n}\n", i + 1));
        }
    }
    s.push_str(MARKER_END);
    s
}

/// Replace (or append) the managed section in `body`.
pub fn with_stack_section(body: &str, section: Option<&str>) -> String {
    let (before, after) = match (body.find(MARKER_START), body.find(MARKER_END)) {
        (Some(s), Some(e)) if s < e => (
            body[..s].trim_end(),
            body[e + MARKER_END.len()..].trim_start_matches('\n'),
        ),
        _ => (body.trim_end(), ""),
    };
    let mut out = before.to_string();
    if let Some(section) = section {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(section);
    }
    if !after.is_empty() {
        out.push_str("\n\n");
        out.push_str(after.trim_end());
    }
    out.push('\n');
    out.trim_start().to_string()
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

    #[test]
    fn stack_section_round_trips() {
        let sec = stack_section(&[1, 2, 3], 1);
        assert!(sec.contains("**#2** ← this PR"));
        let once = with_stack_section("Body text.", Some(&sec));
        assert!(once.starts_with("Body text.\n\n<!-- jj-gh:stack:start -->"));
        let sec2 = stack_section(&[1, 2], 0);
        let twice = with_stack_section(&once, Some(&sec2));
        assert_eq!(twice.matches(MARKER_START).count(), 1);
        assert!(twice.contains("**#1** ← this PR"));
        assert!(!twice.contains("#3"));
        // Removing it restores the original text.
        assert_eq!(with_stack_section(&twice, None), "Body text.\n");
    }

    #[test]
    fn section_preserves_text_after_it() {
        let sec = stack_section(&[5], 0);
        let body = format!("Intro\n\n{sec}\n\nFooter");
        let out = with_stack_section(&body, Some(&stack_section(&[5, 6], 0)));
        assert!(out.starts_with("Intro\n\n"));
        assert!(out.ends_with("Footer\n"));
    }
}
