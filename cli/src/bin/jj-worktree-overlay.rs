// Copyright 2026 The Jujutsu Authors
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

use std::env;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Output;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;

const WATCHMAN_TIMEOUT: Duration = Duration::from_secs(5);

struct HookContext {
    repo_root: PathBuf,
    target: PathBuf,
    instance_id: String,
    requested_commit: String,
    real_git: PathBuf,
    jj: PathBuf,
}

impl HookContext {
    fn from_environment() -> Result<Self, String> {
        Ok(Self {
            repo_root: required_path("WORKTREE_OVERLAY_REPO_ROOT")?,
            target: required_path("WORKTREE_OVERLAY_TARGET")?,
            instance_id: required_string("WORKTREE_OVERLAY_INSTANCE_ID")?,
            requested_commit: required_string("WORKTREE_OVERLAY_REQUESTED_COMMIT")?,
            real_git: required_path("WORKTREE_OVERLAY_REAL_GIT")?,
            jj: jj_binary()?,
        })
    }

    fn workspace_name(&self) -> String {
        format!("overlay-{}", self.instance_id)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("jj-worktree-overlay: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let event = env::args()
        .nth(1)
        .ok_or_else(|| "expected one lifecycle event argument".to_string())?;
    let context = HookContext::from_environment()?;
    match event.as_str() {
        "post-create" => post_create(&context),
        "pre-remove" => pre_remove(&context),
        "post-remove" => post_remove(&context),
        "post-recover" => post_recover(&context),
        _ => Err(format!("unsupported lifecycle event: {event}")),
    }
}

fn post_create(context: &HookContext) -> Result<(), String> {
    run_checked(
        Command::new(&context.jj)
            .arg("-R")
            .arg(&context.repo_root)
            .arg("workspace")
            .arg("add")
            .arg("--existing-git-worktree")
            .arg("--assume-files-present")
            .arg("--sparse-patterns")
            .arg("empty")
            .arg("--name")
            .arg(context.workspace_name())
            .arg("--revision")
            .arg(&context.requested_commit)
            .arg(&context.target),
        "attach existing Git worktree",
    )?;

    let clock = start_watchman(&context.target)?;
    refresh_git_index(context)?;
    run_checked(
        Command::new(&context.jj)
            .arg("-R")
            .arg(&context.target)
            .arg("sparse")
            .arg("reset")
            .arg("--assume-files-present")
            .arg("--watchman-clock")
            .arg(clock),
        "seed working-copy file state",
    )
}

fn pre_remove(context: &HookContext) -> Result<(), String> {
    remove_watchman_watch(&context.target);
    Ok(())
}

fn post_remove(context: &HookContext) -> Result<(), String> {
    remove_watchman_watch(&context.target);
    run_checked(
        Command::new(&context.jj)
            .arg("--ignore-working-copy")
            .arg("-R")
            .arg(&context.repo_root)
            .arg("workspace")
            .arg("forget")
            .arg(context.workspace_name()),
        "forget workspace",
    )
}

fn post_recover(context: &HookContext) -> Result<(), String> {
    let clock = start_watchman(&context.target)?;
    let clean = git_worktree_is_clean(context)?;
    let action = if clean { "set-clock" } else { "reset-clock" };
    let mut command = Command::new(&context.jj);
    command
        .arg("-R")
        .arg(&context.target)
        .arg("debug")
        .arg("watchman")
        .arg(action);
    if clean {
        command.arg(clock);
    }
    run_checked(&mut command, "restore working-copy file monitor state")
}

fn refresh_git_index(context: &HookContext) -> Result<(), String> {
    run_checked(
        Command::new(&context.real_git)
            .arg("-C")
            .arg(&context.target)
            .arg("-c")
            .arg("core.fsmonitor=false")
            .arg("update-index")
            .arg("--refresh"),
        "refresh Git index",
    )?;
    run_checked(
        Command::new(&context.real_git)
            .arg("-C")
            .arg(&context.target)
            .arg("diff-index")
            .arg("--cached")
            .arg("--quiet")
            .arg(&context.requested_commit)
            .arg("--"),
        "verify Git index",
    )
}

fn git_worktree_is_clean(context: &HookContext) -> Result<bool, String> {
    let output = run_output(
        Command::new(&context.real_git)
            .arg("-C")
            .arg(&context.target)
            .arg("-c")
            .arg("core.fsmonitor=false")
            .arg("status")
            .arg("--porcelain=v1")
            .arg("-z")
            .arg("--untracked-files=all"),
        "inspect recovered Git worktree",
    )?;
    Ok(output.stdout.is_empty())
}

fn start_watchman(target: &Path) -> Result<String, String> {
    let watchman = watchman_binary()?;
    remove_watchman_watch_with(&watchman, target);

    let request = serde_json::to_vec(&json!(["debug-set-parallel-crawl", target, true]))
        .map_err(|error| format!("failed to encode Watchman request: {error}"))?;
    let parallel_enabled = run_watchman_request(&watchman, &request)
        .ok()
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
        .and_then(|response| {
            response
                .get("enable_parallel_crawl")
                .and_then(Value::as_bool)
        })
        .unwrap_or(false);
    if !parallel_enabled {
        Command::new(&watchman)
            .arg("watch")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("failed to start Watchman: {error}"))?;
    }

    let deadline = Instant::now() + WATCHMAN_TIMEOUT;
    let mut last_error = String::new();
    while Instant::now() < deadline {
        match Command::new(&watchman).arg("clock").arg(target).output() {
            Ok(output) if output.status.success() => {
                let response: Value = serde_json::from_slice(&output.stdout)
                    .map_err(|error| format!("Watchman returned invalid JSON: {error}"))?;
                if let Some(clock) = response.get("clock").and_then(Value::as_str) {
                    if !clock.is_empty() {
                        return Ok(clock.to_string());
                    }
                }
                return Err("Watchman returned an invalid clock".to_string());
            }
            Ok(output) => last_error = output_detail(&output),
            Err(error) => last_error = error.to_string(),
        }
        thread::sleep(Duration::from_millis(10));
    }

    remove_watchman_watch_with(&watchman, target);
    if last_error.is_empty() {
        Err(format!("Watchman did not initialize {}", target.display()))
    } else {
        Err(format!(
            "Watchman did not initialize {}: {last_error}",
            target.display()
        ))
    }
}

fn run_watchman_request(watchman: &Path, request: &[u8]) -> Result<Output, String> {
    let mut child = Command::new(watchman)
        .arg("-j")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run Watchman: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "failed to open Watchman stdin".to_string())?
        .write_all(request)
        .map_err(|error| format!("failed to write Watchman request: {error}"))?;
    child
        .wait_with_output()
        .map_err(|error| format!("failed to read Watchman response: {error}"))
}

fn remove_watchman_watch(target: &Path) {
    if let Ok(watchman) = watchman_binary() {
        remove_watchman_watch_with(&watchman, target);
    }
}

fn remove_watchman_watch_with(watchman: &Path, target: &Path) {
    drop(
        Command::new(watchman)
            .arg("watch-del")
            .arg(target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    );
}

fn watchman_binary() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("JJ_WORKTREE_OVERLAY_WATCHMAN") {
        return executable(PathBuf::from(path), "Watchman");
    }
    for path in ["/usr/local/bin/watchman", "/usr/bin/watchman"] {
        let candidate = PathBuf::from(path);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(path) = find_in_path("watchman") {
        return Ok(path);
    }
    Err("could not locate Watchman".to_string())
}

fn jj_binary() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("JJ_WORKTREE_OVERLAY_JJ") {
        return executable(PathBuf::from(path), "Jujutsu");
    }
    let current = env::current_exe()
        .map_err(|error| format!("failed to resolve companion executable: {error}"))?;
    let sibling = current
        .parent()
        .ok_or_else(|| "companion executable has no parent directory".to_string())?
        .join("jj");
    executable(sibling, "Jujutsu")
}

fn executable(path: PathBuf, name: &str) -> Result<PathBuf, String> {
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{name} executable does not exist: {}",
            path.display()
        ))
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn required_path(name: &str) -> Result<PathBuf, String> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}"))
}

fn required_string(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("missing or non-UTF-8 {name}"))
}

fn run_checked(command: &mut Command, description: &str) -> Result<(), String> {
    let output = run_output(command, description)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("{description} failed: {}", output_detail(&output)))
    }
}

fn run_output(command: &mut Command, description: &str) -> Result<Output, String> {
    command
        .output()
        .map_err(|error| format!("failed to {description}: {error}"))
}

fn output_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("exit status {}", output.status)
    } else {
        detail.to_string()
    }
}
