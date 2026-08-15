use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use globset::{Glob, GlobSetBuilder};

use crate::{
    domain::{AppError, ErrorKind, InternalGitProfile, WorktreeContentState},
    git::{args, nul_records, GitRunner},
    repository::RepositoryFacts,
    state::TaskRecord,
};

pub fn branch_for(slug: &str, task_id: &str) -> Result<String, AppError> {
    let clean = slug.trim().replace(['/', '\\', ' '], "-");
    if clean.is_empty() || clean.contains("..") {
        return Err(AppError::diagnostic(
            "AGT-0501",
            "task slug cannot be empty or contain parent traversal",
            ErrorKind::Usage,
        ));
    }
    Ok(format!("refs/heads/agentree/{clean}-{task_id}"))
}

pub fn validate_scopes(scopes: &[String]) -> Result<(), AppError> {
    let mut builder = GlobSetBuilder::new();
    for scope in scopes {
        if scope.is_empty() || scope.starts_with('/') || scope.split('/').any(|part| part == "..") {
            return Err(AppError::diagnostic(
                "AGT-0509",
                format!("invalid task scope: {scope}"),
                ErrorKind::Usage,
            ));
        }
        builder.add(Glob::new(scope).map_err(|error| {
            AppError::diagnostic("AGT-0510", error.to_string(), ErrorKind::Usage)
        })?);
    }
    let _ = builder
        .build()
        .map_err(|error| AppError::diagnostic("AGT-0511", error.to_string(), ErrorKind::Usage))?;
    Ok(())
}

pub fn scope_violations(
    scopes: &[String],
    paths: &std::collections::BTreeSet<String>,
) -> Result<Vec<String>, AppError> {
    validate_scopes(scopes)?;
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = GlobSetBuilder::new();
    for scope in scopes {
        builder.add(Glob::new(scope).map_err(|error| {
            AppError::diagnostic("AGT-0512", error.to_string(), ErrorKind::Usage)
        })?);
    }
    let set = builder
        .build()
        .map_err(|error| AppError::diagnostic("AGT-0513", error.to_string(), ErrorKind::Usage))?;
    Ok(paths
        .iter()
        .filter(|path| !set.is_match(path))
        .cloned()
        .collect())
}

pub fn path_for(root: &Path, slug: &str, task_id: &str) -> Result<PathBuf, AppError> {
    let clean = slug.trim().replace(['/', '\\', ' '], "-");
    if clean.is_empty() || clean.contains("..") {
        return Err(AppError::diagnostic(
            "AGT-0502",
            "invalid task slug",
            ErrorKind::Usage,
        ));
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let path = root.join(format!("{clean}-{task_id}"));
    if path.exists() {
        return Err(AppError::diagnostic(
            "AGT-0503",
            "managed worktree path already exists",
            ErrorKind::StateInconsistent,
        ));
    }
    Ok(path)
}

pub fn facts_match(git: &GitRunner, task: &TaskRecord) -> Result<(), AppError> {
    let branch = git.text(
        &task.path,
        InternalGitProfile::Discovery,
        &args(&["symbolic-ref", "--short", "HEAD"]),
    )?;
    let branch = if branch.starts_with("refs/heads/") {
        branch
    } else {
        format!("refs/heads/{branch}")
    };
    if branch != task.branch {
        return Err(AppError::diagnostic(
            "AGT-0504",
            "task worktree is not on its owned branch",
            ErrorKind::StateInconsistent,
        ));
    }
    let head = git.text(
        &task.path,
        InternalGitProfile::Discovery,
        &args(&["rev-parse", "HEAD"]),
    )?;
    if head != task.head_oid {
        return Err(AppError::diagnostic(
            "AGT-0505",
            format!(
                "task HEAD drifted (expected {}, observed {head})",
                task.head_oid
            ),
            ErrorKind::StateInconsistent,
        ));
    }
    Ok(())
}

pub fn rebase_facts_match(task: &TaskRecord, rebase_state: &Path) -> Result<(), AppError> {
    let branch = fs::read_to_string(rebase_state.join("head-name")).map_err(|error| {
        AppError::diagnostic(
            "AGT-0514",
            format!("rebase state is missing its original branch: {error}"),
            ErrorKind::RecoveryRequired,
        )
    })?;
    if branch.trim() != task.branch {
        return Err(AppError::diagnostic(
            "AGT-0504",
            "rebase state is not for the task's owned branch",
            ErrorKind::StateInconsistent,
        ));
    }
    let original_head = fs::read_to_string(rebase_state.join("orig-head")).map_err(|error| {
        AppError::diagnostic(
            "AGT-0515",
            format!("rebase state is missing its original HEAD: {error}"),
            ErrorKind::RecoveryRequired,
        )
    })?;
    if original_head.trim() != task.head_oid {
        return Err(AppError::diagnostic(
            "AGT-0505",
            format!(
                "rebase started from an unexpected task HEAD (expected {}, observed {})",
                task.head_oid,
                original_head.trim()
            ),
            ErrorKind::StateInconsistent,
        ));
    }
    Ok(())
}

pub fn content_state(
    git: &GitRunner,
    facts: &RepositoryFacts,
    worktree: &Path,
) -> Result<WorktreeContentState, AppError> {
    let status = git.run(
        worktree,
        InternalGitProfile::Discovery,
        &args(&["status", "--porcelain=v2", "-z", "--untracked-files=all"]),
    )?;
    let status_records = nul_records(&status.stdout)?;
    let nonignored = records(
        git,
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let nonignored_dirs = records(
        git,
        worktree,
        &[
            "ls-files",
            "--others",
            "--directory",
            "--exclude-standard",
            "-z",
        ],
    )?;
    let ignored = records(
        git,
        worktree,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--directory",
            "--exclude-standard",
            "-z",
        ],
    )?;
    let unmerged = !git
        .run(
            worktree,
            InternalGitProfile::Discovery,
            &args(&["ls-files", "-u", "-z"]),
        )?
        .stdout
        .is_empty();
    let mut visibility_flags = Vec::new();
    let verbose = String::from_utf8(
        git.run(
            worktree,
            InternalGitProfile::Discovery,
            &args(&["ls-files", "-v", "-z"]),
        )?
        .stdout,
    )
    .map_err(|_| {
        AppError::diagnostic(
            "AGT-0506",
            "index path is not UTF-8",
            ErrorKind::Unsupported,
        )
    })?;
    for record in verbose.split('\0').filter(|record| !record.is_empty()) {
        if let Some(flag) = record.chars().next() {
            if matches!(flag, 'S' | 's' | 'h' | 'k') {
                visibility_flags.push(record.to_owned());
            }
        }
    }
    for key in [
        "core.sparseCheckout",
        "index.sparse",
        "core.splitIndex",
        "extensions.worktreeConfig",
    ] {
        if git
            .text(
                worktree,
                InternalGitProfile::Discovery,
                &args(&["config", "--bool", key]),
            )
            .unwrap_or_default()
            == "true"
        {
            visibility_flags.push(key.to_owned());
        }
    }
    let in_progress = [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
    ]
    .iter()
    .any(|name| facts.git_dir.join(name).exists());
    let filesystem_entries = filesystem_residue(worktree)?;
    let mut nonignored_residue = nonignored;
    nonignored_residue.extend(nonignored_dirs);
    nonignored_residue.sort();
    nonignored_residue.dedup();
    let mut ignored_residue = ignored;
    ignored_residue.sort();
    ignored_residue.dedup();
    Ok(WorktreeContentState {
        tracked_clean: status_records.is_empty(),
        nonignored_residue,
        ignored_residue,
        visibility_flags,
        in_progress: in_progress || unmerged,
        filesystem_entries,
    })
}

pub fn ensure_mutation_pristine(
    git: &GitRunner,
    facts: &RepositoryFacts,
    worktree: &Path,
) -> Result<WorktreeContentState, AppError> {
    let state = content_state(git, facts, worktree)?;
    if !state.mutation_pristine() {
        return Err(AppError::diagnostic(
            "AGT-0507",
            format!("worktree is not mutation-pristine: {}", describe(&state)),
            ErrorKind::DirtyWorktree,
        ));
    }
    Ok(state)
}

pub fn ensure_review_clean(
    git: &GitRunner,
    facts: &RepositoryFacts,
    worktree: &Path,
) -> Result<WorktreeContentState, AppError> {
    let state = content_state(git, facts, worktree)?;
    if !state.review_clean() {
        return Err(AppError::diagnostic(
            "AGT-0508",
            format!("worktree is not review-clean: {}", describe(&state)),
            ErrorKind::DirtyWorktree,
        ));
    }
    Ok(state)
}

pub fn all_change_paths(git: &GitRunner, task: &TaskRecord) -> Result<BTreeSet<String>, AppError> {
    let mut paths = BTreeSet::new();
    for arguments in [
        vec!["diff", "--name-only", "-z", &task.base_oid, "HEAD"],
        vec!["diff", "--cached", "--name-only", "-z"],
        vec!["diff", "--name-only", "-z"],
        vec!["ls-files", "--others", "--exclude-standard", "-z"],
    ] {
        let output = git.run(
            &task.path,
            InternalGitProfile::Discovery,
            &arguments
                .iter()
                .map(|value| value.to_string().into())
                .collect::<Vec<_>>(),
        )?;
        paths.extend(nul_records(&output.stdout)?);
    }
    Ok(paths)
}

fn records(git: &GitRunner, worktree: &Path, arguments: &[&str]) -> Result<Vec<String>, AppError> {
    let output = git.run(worktree, InternalGitProfile::Discovery, &args(arguments))?;
    nul_records(&output.stdout)
}

fn filesystem_residue(worktree: &Path) -> Result<Vec<String>, AppError> {
    let mut residue = Vec::new();
    walk(worktree, worktree, &mut residue)?;
    Ok(residue)
}

fn walk(root: &Path, current: &Path, residue: &mut Vec<String>) -> Result<(), AppError> {
    let entries = fs::read_dir(current)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        if metadata.file_type().is_symlink() || metadata.file_type().is_file() {
            continue;
        }
        if metadata.file_type().is_dir() {
            let mut children = fs::read_dir(&path)?;
            if children.next().is_none() {
                residue.push(relative);
                continue;
            }
            walk(root, &path, residue)?;
            continue;
        }
        residue.push(relative);
    }
    Ok(())
}

fn describe(state: &WorktreeContentState) -> String {
    format!(
        "tracked_clean={}, untracked={}, ignored={}, flags={}, filesystem={}",
        state.tracked_clean,
        state.nonignored_residue.len(),
        state.ignored_residue.len(),
        state.visibility_flags.len(),
        state.filesystem_entries.len()
    )
}

pub fn marker_path(git: &GitRunner, worktree: &Path) -> Result<PathBuf, AppError> {
    let git_dir = git.text(
        worktree,
        InternalGitProfile::Discovery,
        &args(&["rev-parse", "--git-dir"]),
    )?;
    let path = PathBuf::from(git_dir);
    Ok(if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    }
    .join("agentree-owner.json"))
}
