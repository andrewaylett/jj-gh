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
