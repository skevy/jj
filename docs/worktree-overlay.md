# Worktree Overlay integration

This branch adds the Jujutsu side of an optional integration with
`worktree-overlay`. It is maintained in a personal fork while colocated
workspace support is still under development upstream.

The two projects have separate ownership:

- `worktree-overlay` creates and persists detached Git worktrees backed by
  OverlayFS. It has no Jujutsu-specific behavior.
- This fork attaches an existing linked Git worktree as a native Jujutsu
  workspace and maintains its file-monitor state.

The boundary matters because the Git worktree implementation is useful on its
own. Jujutsu should be an optional consumer of its lifecycle API rather than a
mode inside the Git shim.

## Install

The `jj-cli` package installs both `jj` and the `jj-worktree-overlay`
companion:

```bash
cargo install \
  --git https://github.com/skevy/jj \
  --branch worktree-overlay-v0.42.0 \
  --locked \
  --root ~/.local \
  jj-cli
```

Configure a repository to invoke the companion for lifecycle events:

```bash
git worktree-overlay configure \
  --repo ~/repos/treehouse \
  --target-prefix ~/.codex/worktrees \
  --lifecycle-hook ~/.local/bin/jj-worktree-overlay
```

The source checkout must already be a colocated Jujutsu repository. Both the
interactive shell and the lifecycle companion must resolve the `jj` binary
installed from this branch.

## Creation

`worktree-overlay` always starts with an ordinary detached Git worktree:

1. Git creates the linked-worktree registration with `--no-checkout`.
2. The shim prepares the OverlayFS upper layer and index.
3. The merged worktree is mounted and its Git `HEAD` is verified.
4. The shim invokes `jj-worktree-overlay post-create`.

The companion then runs:

```text
jj workspace add
  --existing-git-worktree
  --assume-files-present
  --sparse-patterns empty
  --name overlay-<instance-id>
  --revision <requested-commit>
  <worktree-path>
```

`--existing-git-worktree` verifies that the destination belongs to the same
Git common directory as the source checkout. It registers a native Jujutsu
workspace without asking Git to create another worktree. The companion ignores
the source working copy while doing this, so worktree creation does not
snapshot or rewrite the user's project checkout.

The workspace starts with empty sparse patterns, so attaching it does not
rewrite the mounted files. The companion starts a file monitor, refreshes the
existing Git index, then seeds Jujutsu's file-state table from that index while
expanding the sparse patterns to the full tree.

If the lifecycle command fails, `worktree-overlay` removes the Git worktree and
invokes the removal lifecycle events for best-effort cleanup.

## Removal and recovery

Before unmounting, `pre-remove` drops the exact-path file watch. After Git has
removed its linked-worktree registration, `post-remove` forgets the Jujutsu
workspace.

If `post-remove` fails, `worktree-overlay` retains the upper directory and
instance record. Repeating removal or running `git worktree-overlay recover`
retries cleanup.

After a workspace restart, `post-recover` starts a new watch on the remounted
merged directory. A Git-clean workspace adopts the new clock directly. A dirty
workspace clears the old clock, so its next Jujutsu snapshot performs a full
scan rather than missing changes that predate the new watch.

## Why the file monitor watches the merged directory

The visible working copy is the OverlayFS mount. Watching only the lower
directory misses local changes. Watching only the upper directory does not
provide normal filesystem semantics for unchanged paths, whiteouts, opaque
directories, or redirected directories.

The current implementation therefore watches the complete merged directory.
This is correct, but each large worktree carries another complete in-memory
tree and another set of directory watches.

## Upper-layer file state

OverlayFS gives us a possible replacement for the complete-directory watch:

- The lower generation is immutable.
- Every local mutation is represented in the durable upper directory.
- Regular upper entries represent added, modified, or copied-up paths.
- Character-device whiteouts represent deletions.
- Overlay attributes represent opaque directories, redirects, and metacopy
  state.

The upper directory is therefore a sparse representation of the working-copy
delta. In the current Ergo worktree it contained roughly 4,000 entries and a
complete scan took about 10 milliseconds, while the merged repository contains
hundreds of thousands of paths.

A future Jujutsu filesystem monitor could scan the upper directory and report
dirty paths against the merged worktree:

1. Seed the initial file-state table from the linked Git index.
2. Scan upper entries on each snapshot.
3. Stat candidate paths through the merged mount.
4. Expand directory deletions and redirects against the Git index.
5. Persist enough upper-directory metadata to detect changes between scans.

This follows the same broad model as EdenFS: an immutable source-control tree,
small persistent mutable state, and change detection driven by that mutable
state. EdenFS owns the filesystem and journal. This design would interpret
OverlayFS's existing upper representation instead.

The approach should scale with the number of changed paths rather than the
size of the repository or the number of concurrent worktrees. The cost is a
Linux-specific filesystem monitor that must correctly track OverlayFS
whiteouts, opaque directories, redirects, metacopy behavior, and kernel
version differences.

The current Watchman implementation remains the correctness baseline until
that scanner exists.
