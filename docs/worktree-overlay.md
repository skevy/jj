# Worktree Overlay support

This branch contains the Jujutsu primitives needed by an external
`worktree-overlay` lifecycle adapter. The adapter itself is user policy and is
not part of `jj-cli`.

## Install

```bash
cargo install \
  --git https://github.com/skevy/jj \
  --branch worktree-overlay-v0.42.0 \
  --locked \
  --root ~/.local \
  --force \
  jj-cli
```

This installs `jj`. A lifecycle adapter can then invoke the hidden commands
described below.

## Adopt an existing Git worktree

`jj workspace add --existing-git-worktree` registers an existing linked Git
worktree as a native JJ workspace. It is intended for worktrees that another
tool has already created and populated.

The command requires:

- A colocated source workspace.
- `--assume-files-present`.
- `--sparse-patterns empty`.
- A destination whose Git common directory matches the source repository.

The empty sparse pattern prevents workspace initialization from rewriting the
destination. The command also skips snapshotting the source working copy.

Example:

```bash
jj -R "$source" workspace add \
  --existing-git-worktree \
  --assume-files-present \
  --sparse-patterns empty \
  --name "$workspace_name" \
  --revision "$commit" \
  "$target"
```

## Seed working-copy state

After the external filesystem and Git index are ready, the adapter can expand
the sparse patterns without checking out files:

```bash
jj -R "$target" sparse reset \
  --assume-files-present \
  --watchman-clock "$clock"
```

JJ seeds its file-state table from the colocated Git index. File changes made
after the mount remain visible and are not overwritten by baseline state.

## Restore Watchman state

An external adapter can set a new Watchman clock after remounting a clean
working copy:

```bash
jj -R "$target" debug watchman set-clock "$clock"
```

For a dirty working copy, it should clear the old clock:

```bash
jj -R "$target" debug watchman reset-clock
```

Clearing the clock forces the next snapshot to scan the working copy rather
than assume the new watch includes changes made before it started.

## External lifecycle

The expected consumer creates an ordinary linked Git worktree first, mounts
its filesystem, and then uses these JJ commands during create, recover, and
remove events. The consumer owns Watchman process orchestration, Git index
refresh, workspace naming, retries, and cleanup.
