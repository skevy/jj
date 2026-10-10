// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs;

use futures::future::try_join_all;
use itertools::Itertools as _;
#[cfg(feature = "git")]
use jj_lib::backend::CommitId;
use jj_lib::commit::CommitIteratorExt as _;
use jj_lib::file_util;
use jj_lib::file_util::IoResultExt as _;
#[cfg(feature = "git")]
use jj_lib::git;
#[cfg(feature = "git")]
use jj_lib::git::GitSubprocessOptions;
#[cfg(feature = "git")]
use jj_lib::git::create_worktree;
#[cfg(feature = "git")]
use jj_lib::git_backend::GitRepoAtWorkdirError;
#[cfg(feature = "git")]
use jj_lib::local_working_copy::LockedLocalWorkingCopy;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::workspace::Workspace;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
#[cfg(feature = "git")]
use crate::cli_util::start_repo_transaction;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
#[cfg(feature = "git")]
use crate::commands::git::maybe_add_gitignore;
use crate::description_util::add_trailers;
use crate::description_util::join_message_paragraphs;
use crate::ui::Ui;

/// How to handle sparse patterns when creating a new workspace.
#[derive(clap::ValueEnum, Clone, Debug, Eq, PartialEq)]
pub(crate) enum SparseInheritance {
    /// Copy all sparse patterns from the current workspace.
    Copy,
    /// Include all files in the new workspace.
    Full,
    /// Clear all files from the workspace (it will be empty).
    Empty,
}

/// Add a workspace
///
/// By default, the new workspace inherits the sparse patterns of the current
/// workspace. You can override this with the `--sparse-patterns` option.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceAddArgs {
    /// Where to create the new workspace
    #[arg(value_hint = clap::ValueHint::DirPath)]
    destination: String,

    /// A name for the workspace
    ///
    /// To override the default, which is the basename of the destination
    /// directory.
    #[arg(long)]
    name: Option<WorkspaceNameBuf>,

    /// A list of parent revisions for the working-copy commit of the newly
    /// created workspace. You may specify nothing, or any number of parents.
    ///
    /// If no revisions are specified, the new workspace will be created, and
    /// its working-copy commit will exist on top of the parent(s) of the
    /// working-copy commit in the current workspace, i.e. they will share the
    /// same parent(s).
    ///
    /// If any revisions are specified, the new workspace will be created, and
    /// the new working-copy commit will be created with all these revisions as
    /// parents, i.e. the working-copy commit will exist as if you had run `jj
    /// new r1 r2 r3 ...`.
    #[arg(long = "revision", short, value_name = "REVSETS", alias = "revisions")]
    revisions: Vec<RevisionArg>,

    /// The change description to use
    #[arg(long = "message", short, value_name = "MESSAGE")]
    message_paragraphs: Vec<String>,

    /// Create a corresponding Git worktree for this workspace
    ///
    /// By default, a Git worktree is created when the current workspace is
    /// colocated and the [git.colocate config] is `true`.
    ///
    /// [git.colocate config]:
    ///     https://docs.jj-vcs.dev/latest/config/#default-colocation
    #[cfg(feature = "git")]
    #[arg(long, conflicts_with = "no_colocate")]
    colocate: bool,

    /// Do not create a Git worktree for this workspace
    #[cfg(feature = "git")]
    #[arg(long, conflicts_with = "colocate")]
    no_colocate: bool,

    /// Attach to an existing worktree without checking out its files.
    #[cfg(feature = "git")]
    #[arg(long, hide = true, conflicts_with = "no_colocate")]
    existing_git_worktree: bool,

    /// A Watchman clock captured before attachment began.
    #[cfg(feature = "git")]
    #[arg(long, hide = true, requires = "existing_git_worktree")]
    watchman_clock: Option<String>,

    /// How to handle sparse patterns when creating a new workspace.
    #[arg(long, value_enum, default_value_t = SparseInheritance::Copy)]
    sparse_patterns: SparseInheritance,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_add(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceAddArgs,
) -> Result<(), CommandError> {
    #[cfg(feature = "git")]
    let adopt_existing = args.existing_git_worktree;
    #[cfg(not(feature = "git"))]
    let adopt_existing = false;
    // Adoption only needs source repository metadata. Snapshotting its working
    // copy can be expensive and would couple otherwise independent worktrees.
    let old_workspace_command = if adopt_existing {
        command.workspace_helper_no_snapshot(ui).await?
    } else {
        command.workspace_helper(ui).await?
    };
    let destination_path = command.cwd().join(&args.destination);
    let workspace_name = if let Some(name) = &args.name {
        name.to_owned()
    } else {
        let file_name = destination_path.file_name().unwrap();
        file_name
            .to_str()
            .ok_or_else(|| user_error("Destination path is not valid UTF-8"))?
            .into()
    };
    if workspace_name.as_str().is_empty() {
        return Err(user_error("New workspace name cannot be empty"));
    }
    if adopt_existing && args.sparse_patterns != SparseInheritance::Copy {
        return Err(user_error(
            "--existing-git-worktree cannot be combined with --sparse-patterns",
        ));
    }

    let repo = old_workspace_command.repo();
    if repo.view().get_wc_commit_id(&workspace_name).is_some() {
        return Err(user_error(format!(
            "Workspace named '{name}' already exists",
            name = workspace_name.as_symbol()
        )));
    }
    #[cfg(feature = "git")]
    let destination_git_repo = if adopt_existing {
        if !old_workspace_command.working_copy_shared_with_git() {
            return Err(user_error(
                "--existing-git-worktree requires a colocated source workspace",
            ));
        }
        if !destination_path.is_dir() {
            return Err(user_error(format!(
                "Existing Git worktree does not exist: {}",
                destination_path.display()
            )));
        }
        // The backend checks that the worktree shares the source Git repository.
        // Keep its handle so seeding reads the destination's separate index.
        let git_repo = git::get_git_backend(repo.store())?
            .open_git_repo_at_workdir(&destination_path)
            .map_err(|err| match err {
                GitRepoAtWorkdirError::Unrelated { .. } => {
                    user_error("Existing Git worktree belongs to a different repository")
                }
                err => user_error(format!("Failed to open existing Git worktree: {err}")),
            })?;
        Some(git_repo)
    } else {
        None
    };

    #[cfg(feature = "git")]
    let old_workspace_command = if let Some(git_repo) = &destination_git_repo {
        // Git-first callers can request a commit that JJ hasn't imported yet.
        // Import only the destination HEAD's metadata, without the source
        // workspace's snapshot or the Git exports performed by tx.finish().
        let mut tx = start_repo_transaction(
            old_workspace_command.repo(),
            old_workspace_command.workspace_name(),
            command.string_args(),
        );
        if let Ok(oid) = git_repo.head_id() {
            let head_id = CommitId::from_bytes(oid.as_bytes());
            if !tx.repo().index().has_id(&head_id).await? {
                git::get_git_backend(tx.repo().store())?
                    .import_head_commits([&head_id])
                    .map_err(|err| git::GitImportError::MissingHeadTarget {
                        id: head_id.clone(),
                        err,
                    })?;
            }
            let commit = tx.repo().store().get_commit_async(&head_id).await?;
            tx.repo_mut().add_head(&commit).await?;
        }
        if tx.repo().has_changes() {
            let repo = tx.commit("import existing Git worktree head").await?;
            command.for_workable_repo(ui, command.load_workspace()?, repo)?
        } else {
            old_workspace_command
        }
    } else {
        old_workspace_command
    };

    let repo = old_workspace_command.repo();
    // Resolve revisions before creating or registering the destination, so an
    // invalid revision leaves an existing Git worktree available for retry.
    let parents = if args.revisions.is_empty() {
        // Use the source working-copy commit's parents, or the root if there is
        // no working-copy commit in the source workspace.
        if let Some(old_wc_commit_id) = repo
            .view()
            .get_wc_commit_id(old_workspace_command.workspace_name())
        {
            repo.store()
                .get_commit_async(old_wc_commit_id)
                .await?
                .parents()
                .await?
        } else {
            vec![repo.store().root_commit()]
        }
    } else {
        try_join_all(
            old_workspace_command
                .resolve_some_revsets(ui, &args.revisions)
                .await?
                .iter()
                .map(|id| repo.store().get_commit_async(id)),
        )
        .await?
    };

    if !adopt_existing {
        if !destination_path.exists() {
            fs::create_dir(&destination_path).context(&destination_path)?;
        } else if !file_util::is_empty_dir(&destination_path)? {
            return Err(user_error(
                "Destination path exists and is not an empty directory",
            ));
        }
    }

    #[cfg(feature = "git")]
    {
        let should_colocate = if args.colocate {
            true
        } else if args.no_colocate {
            false
        } else {
            old_workspace_command.working_copy_shared_with_git()
                && old_workspace_command.settings().get_bool("git.colocate")?
        };
        if should_colocate && !adopt_existing {
            let subprocess_options =
                GitSubprocessOptions::from_settings(old_workspace_command.settings())?;
            create_worktree(repo.store(), subprocess_options, &destination_path)?;
            writeln!(ui.status(), "Created Git worktree for the new workspace.")?;
        }
    }

    let working_copy_factory = command.get_working_copy_factory()?;
    let repo_path = old_workspace_command.repo_path();
    // If we add per-workspace configuration, we'll need to reload settings for
    // the new workspace.
    let (new_workspace, repo) = Workspace::init_workspace_with_existing_repo(
        &destination_path,
        repo_path,
        repo,
        working_copy_factory,
        workspace_name.clone(),
    )
    .await?;
    writeln!(
        ui.status(),
        "Created workspace in \"{}\"",
        file_util::relative_path(command.cwd(), &destination_path).display()
    )?;
    // Show a warning if the user passed a path without a separator, since they
    // may have intended the argument to only be the name for the workspace.
    if !args.destination.contains(std::path::is_separator) {
        writeln!(
            ui.warning_default(),
            r#"Workspace created inside current directory. If this was unintentional, delete the "{}" directory and run `jj workspace forget {name}` to remove it."#,
            args.destination,
            name = workspace_name.as_symbol()
        )?;
    }

    let mut new_workspace_command = command.for_workable_repo(ui, new_workspace, repo)?;
    // Keep the colocated Git worktree from tracking Jujutsu's own files.
    #[cfg(feature = "git")]
    maybe_add_gitignore(&new_workspace_command)?;

    let sparsity = match args.sparse_patterns {
        SparseInheritance::Full => None,
        SparseInheritance::Empty => Some(vec![]),
        SparseInheritance::Copy => {
            let sparse_patterns = old_workspace_command
                .working_copy()
                .sparse_patterns()?
                .to_vec();
            Some(sparse_patterns)
        }
    };
    // Initialize the commit without materializing any files over the populated
    // worktree. Seeding below restores full sparsity and the existing file stats.
    let sparsity = if adopt_existing {
        Some(vec![])
    } else {
        sparsity
    };

    if let Some(sparse_patterns) = sparsity {
        let (mut locked_ws, _wc_commit) =
            new_workspace_command.start_working_copy_mutation().await?;
        locked_ws
            .locked_wc()
            .set_sparse_patterns(sparse_patterns)
            .await
            .map_err(|err| internal_error_with_message("Failed to set sparse patterns", err))?;
        let operation_id = locked_ws.locked_wc().old_operation_id().clone();
        locked_ws.finish(operation_id).await?;
    }

    let mut tx = new_workspace_command.start_transaction();

    let tree = merge_commit_trees(tx.repo(), &parents).await?;
    let parent_ids = parents.iter().ids().cloned().collect_vec();
    let mut commit_builder = tx.repo_mut().new_commit(parent_ids, tree).detach();
    let mut description = join_message_paragraphs(&args.message_paragraphs);
    if !description.is_empty() {
        // The first trailer would become the first line of the description.
        // Also, a commit with no description is treated in a special way in jujutsu: it
        // can be discarded as soon as it's no longer the working copy. Adding a
        // trailer to an empty description would break that logic.
        commit_builder.set_description(description);
        description = add_trailers(ui, &tx, &commit_builder).await?;
    }
    commit_builder.set_description(&description);
    let new_wc_commit = commit_builder.write(tx.repo_mut()).await?;

    tx.edit(&new_wc_commit)?;
    tx.finish(
        ui,
        format!(
            "create initial working-copy commit in workspace {name}",
            name = workspace_name.as_symbol()
        ),
    )
    .await?;

    #[cfg(feature = "git")]
    if let Some(destination_git_repo) = destination_git_repo {
        let git_index = destination_git_repo.index_or_empty().map_err(|err| {
            internal_error_with_message("Failed to read the colocated Git index", err)
        })?;
        let (mut locked_ws, _wc_commit) =
            new_workspace_command.start_working_copy_mutation().await?;
        let Some(locked_local_wc): Option<&mut LockedLocalWorkingCopy> =
            locked_ws.locked_wc().downcast_mut()
        else {
            return Err(user_error(
                "--existing-git-worktree requires a standard local-disk working copy",
            ));
        };
        locked_local_wc
            .seed_from_git_index(&git_index, args.watchman_clock.as_deref())
            .await
            .map_err(|err| {
                internal_error_with_message("Failed to seed working copy from Git index", err)
            })?;
        let operation_id = locked_ws.locked_wc().old_operation_id().clone();
        locked_ws.finish(operation_id).await?;
    }
    Ok(())
}
