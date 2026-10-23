# Notes for working on jj-gh

## Why the version in `Cargo.toml` is `0.0.0`

Releases are driven by the pushed `v*` tag, not by a commit that bumps
`Cargo.toml`. The release workflow rewrites the `version` line from the tag
just before building each target, so the shipped binary and its archive agree
with the tag by construction, and nothing on `main` can drift from it.

The consequence is that `Cargo.toml` is deliberately a placeholder. Don't
"fix" it, and don't add a check that it matches a tag.

## Where `--version` comes from

`build.rs` runs `git describe --tags --long --match 'v[0-9]*'` and exports the
result as `JJ_GH_VERSION`, which clap uses instead of `CARGO_PKG_VERSION`. It
falls back to the package version when git or tags are unavailable (for
instance a source tarball). It is a hand-rolled script rather than `vergen` or
`git-version` because it needs about sixty lines and no extra dependencies.

The dirty check ignores `Cargo.toml` and `Cargo.lock`. That's not an
oversight: the release workflow edits them, and an exact tag would otherwise
report `+dirty`.

Because the release build edits `Cargo.toml`, do not pass `--locked` to the
release build unless `Cargo.lock` is also updated to match.

## Native GitHub stacks

`submit` calls GitHub's stacked pull request REST endpoints (`/repos/{owner}/{repo}/stacks`,
API version `2026-03-10`, public preview) after the PRs exist. Without that, GitHub only
offers to create the stack when someone opens a PR. GraphQL is read-only for stacks, so this
is REST.

Failure is deliberately a warning, not an error: the feature may not be enabled for a
repository. GitHub keeps merged PRs
in a stack, so we compare only the open ones against our list.

GitHub retargets the PRs above a merged one at its base, so `merge` and `sync` don't
check or fix bases. `submit` still dissolves the stack before retargeting, because that
covers us changing the shape of the stack (inserting or reordering changes), not merges.

Finding the stack to act on works the same way: without `-r`, `Ctx::segments` first looks at
the bookmarks on `@` (if jj would push it) and then on `@-`, asks GitHub which stack their open
PRs are in, and takes the first it finds. Any failure in that lookup is a warning and we fall
back to `trunk()..@`, for the same reason as above.

## Reparenting

`reparent` is the one command that works on a tree rather than a linear stack, so it doesn't use
`Ctx::segments`. It loads `mutable() & ::(revset)` as a graph, treats every change carrying a
bookmark with an open PR as a node, and works out each target's base in `plan`. Nodes outside the
revset still count as candidate bases; they just aren't retargeted. Changes without a PR are
looked through, and `reparent` never creates PRs or bookmarks, that's `submit`'s job.

The base is the single nearest PR ancestor; with several (a merge), the nearest PR that is an
ancestor of all of them; with none, or an ambiguous answer, the default branch. After dissolving
the native stack it rebuilds one only if every PR in the graph now forms one chain.

## Merging

GitHub refuses to merge a PR that's in a native stack through the classic `PUT .../merge`
endpoint, so `merge` uses `PUT .../merge-async` (API version `2026-03-10`) and polls the
result. That endpoint merges the named PR *and every PR below it* in one atomic request,
which is what the web UI's "merge up to here" does, so `merge --all` is a single request for
the topmost ready PR rather than a loop.

That is only safe when the PRs really are in one native stack: otherwise the top PR would
land on its parent's branch. `merge` checks with `stack_containing` and falls back to the
bottom PR, with a warning, if they aren't. The request carries the PR's head SHA so GitHub
cancels the merge if the branch moved after we checked readiness.

If the base branch has a merge queue the PR is enqueued rather than merged. We say so and
stop, without syncing, because nothing has landed yet.

## Verbose output

`--verbose` is a global count (`-v`, `-v -v`) stored in a process-wide atomic by
`log::set_level`, rather than threaded through `Ctx`, `Jj` and `Client`, because
those are built in many places and the output is purely diagnostic. Everything goes to
stderr so stdout stays pipeable.

A `log::Step` is held for as long as we're waiting on something. Without `-v`,
on a terminal it is a status line, erased when the step is dropped or the next
message arrives, so nothing that prints to stdout may run while a step is
alive; off a terminal it prints nothing. With `-v` it is an ordinary line, so
the status lines become the permanent record. Steps wrap the public `Client` methods and `Jj::run`; the
HTTP layer and `Jj` add the exact request and command line at level 2. Only the
URL and headers we choose are traced, so the token can't leak.
`Jj::run_visible` uses a permanent note instead of a step, because jj's own
progress output follows it on the same stream.
