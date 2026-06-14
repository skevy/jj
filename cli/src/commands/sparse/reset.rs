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

use jj_lib::local_working_copy::LockedLocalWorkingCopy;
use jj_lib::repo_path::RepoPathBuf;
use tracing::instrument;

use super::update_sparse_patterns_with;
use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::command_error::internal_error_with_message;
use crate::command_error::user_error;
use crate::ui::Ui;

/// Reset the patterns to include all files in the working copy
#[derive(clap::Args, Clone, Debug)]
pub struct SparseResetArgs {
    #[arg(long, hide = true)]
    assume_files_present: bool,
}

#[instrument(skip_all)]
pub async fn cmd_sparse_reset(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SparseResetArgs,
) -> Result<(), CommandError> {
    if args.assume_files_present {
        let mut workspace_command = command.workspace_helper_no_snapshot(ui).await?;
        let (mut locked_ws, _wc_commit) = workspace_command.start_working_copy_mutation().await?;
        let Some(locked_local_wc): Option<&mut LockedLocalWorkingCopy> =
            locked_ws.locked_wc().downcast_mut()
        else {
            return Err(user_error(
                "--assume-files-present requires a standard local-disk working copy",
            ));
        };
        locked_local_wc
            .assume_files_present(vec![RepoPathBuf::root()])
            .await
            .map_err(|err| {
                internal_error_with_message("Failed to record the existing working-copy files", err)
            })?;
        let operation_id = locked_ws.locked_wc().old_operation_id().clone();
        locked_ws.finish(operation_id).await?;
        return Ok(());
    }

    let mut workspace_command = command.workspace_helper(ui).await?;
    update_sparse_patterns_with(ui, &mut workspace_command, |_ui, _old_patterns| {
        Ok(vec![RepoPathBuf::root()])
    })
    .await
}
