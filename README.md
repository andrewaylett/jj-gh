# jj-gh

jj-native access to GitHub pull requests, built around **stacks**.

`jj-gh` looks at the changes between `trunk()` and your working copy, and turns
each one into a pull request based on the one below it. It keeps GitHub's
stack up to date, restacks when the bottom PR merges, and
shows CI/review state for the whole stack in one go. It talks to `jj` through
its CLI and to GitHub through its API, so there is no `git` or `gh` plumbing in
your way.

```
$ jj-gh status
○ #43 Wire up the frobnicator
│   push-kkpmotlwvvyq  change kkpmotlw  checks ✓ · review required
│   https://github.com/acme/app/pull/43
○ #42 Add frobnicator
│   push-ltlnlxqqqpvl  change ltlnlxqq  checks ✓ · approved
│   https://github.com/acme/app/pull/42
◆ main
```

## Install

With [mise](https://mise.jdx.dev) (installs a prebuilt binary from the GitHub release,
verified against its signed packslip manifest):

```sh
mise use -g packslip:andrewaylett/jj-gh@0.2.1
```

Or from source: `cargo install --git https://github.com/andrewaylett/jj-gh`.

To run it as `jj gh …`, add an alias to your jj config (`jj config edit --user`):

```toml
[aliases]
gh = ["util", "exec", "--", "jj-gh"]
```

Shell completions: `jj-gh completions bash|zsh|fish|…`.

## Authentication

`jj-gh` uses `GITHUB_TOKEN` (then `GH_TOKEN`) if set, and otherwise asks the
GitHub CLI with `gh auth token`. `jj-gh auth` shows which one it found and who
you are. GitHub Enterprise works: the host is taken from the remote's URL.

## Commands

| Command | What it does |
| --- | --- |
| `status` (`st`) | Show the stack with PR numbers, checks, review state, and whether local changes still need pushing or the PR description differs from the change description. |
| `submit` (`sub`) | Create bookmarks as needed, push, and create or update one PR per change, each based on the one below. Registers the PRs as a native GitHub stack (creating, extending, or rebuilding it as needed; a failure there is only a warning). Idempotent. |
| `sync` | `jj git fetch`, then drop merged PRs from the bottom of the stack and rebase the rest onto the new trunk. `--submit` pushes the restacked branches afterwards. |
| `merge` | Merge the bottom PR once it's ready (not a draft, checks green, no changes requested, pushed), then `sync`. `--method squash\|merge\|rebase` (default: rebase if the repository allows it, else squash, else merge), `--all` merges every PR that's ready, bottom first, stopping at the first that isn't, in one atomic request to GitHub. `--force`, `--submit`. |
| `pr open [REV]` | Open (or `--print`) the PR for a change; defaults to the stack head. |
| `pr checkout N` | Fetch a same-repo PR's branch and `jj new` on top of it. |
| `auth` | Show the credential source and user. |

Common flags: `-v/--verbose` (on every command): on a terminal, progress always
shows as a status line that each step overwrites. `-v` keeps those steps as plain
lines on stderr instead, as a record. `-v -v` also prints every `jj` command line and HTTP request (method,
URL, status, time), never the token. `-r/--revision` picks the stack. By default the bookmarks on `@`
(then on its parents) are looked up, and the first one whose open PR is in a
native GitHub stack gives the whole stack, even when `@` is part-way up it. A
pushable `@` above the stack is included; one that `jj git push` would refuse
(undescribed, conflicted, or private) is left out. If there's no such stack, the
default is `trunk()..@`, again leaving out an unpushable `@`; `-n/--dry-run` on `submit`; `--draft`;
`--reviewer LOGIN`; `--update-description` to push edited change descriptions
to existing PRs (otherwise titles and bodies you've edited on GitHub are left
alone).

### A typical session

```sh
jj new main -m "Add frobnicator"      # …hack…
jj new -m "Wire up the frobnicator"   # …hack…
jj-gh submit                           # two PRs, #42 ← main, #43 ← #42
# review, fixes: edit any change, then
jj-gh submit                           # pushes the rewritten stack
jj-gh merge --submit                   # lands #42, restacks #43 (GitHub retargets it at main)
```

### How changes map to PRs

By default every change in the stack gets its own PR, on a bookmark named the
way jj names `--change` bookmarks (`git.push-bookmark-prefix` + change id).
If a change already has a bookmark, that name is used. The branch survives
rewrites because it follows the change.

With `--by-bookmark` (or `JJ_GH_BY_BOOKMARK=1`) a PR instead spans everything
from the previous bookmark up to the next one, and the head of the stack gets a
bookmark if it doesn't have one. PR title and description come from the
head change's description, without a closing block made up entirely of
`Co-Authored-By:` trailers, which GitHub drops when it creates a PR.

The stack has to be a single linear chain of described, conflict-free changes.

### Other settings

- `--remote NAME` / `JJ_GH_REMOTE` — the GitHub remote (default `origin`).
- `--repo OWNER/NAME` / `JJ_GH_REPO` — override what the remote URL says.
- `-R PATH` — operate on a repository elsewhere.
- `JJ_GH_API_URL`, `JJ_GH_GRAPHQL_URL` — API endpoints, for testing or odd setups.

## Limitations

- Only same-repository PRs can be checked out; fork PRs aren't supported yet.
- `sync` recognises merged PRs through the GitHub API, so it works with squash,
  rebase and merge-commit strategies, but only cleans up PRs it finds on bookmarks
  in the current stack.
- Requires `jj` 0.45 or newer on `PATH` (tested with 0.45.1).

## Development

```sh
mise install        # rust + jj
cargo test          # unit tests, plus end-to-end tests against real jj and a mock GitHub
```

The end-to-end tests are skipped when `jj` isn't installed.

## Releasing

Push a tag named `v` followed by the new version; there is nothing to commit
first. The tag is the source of truth, and the workflow sets `version` in
`Cargo.toml` from it before building.

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The release workflow builds Linux (x86_64/aarch64, musl), macOS (x86_64/arm64)
and Windows binaries, attaches them with SHA-256 checksums to a GitHub release
named after the tag, and mise's `packslip:` backend picks them up by platform.
