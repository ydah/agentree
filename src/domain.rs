use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{code}: {message}")]
    Diagnostic {
        code: &'static str,
        message: String,
        kind: ErrorKind,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Git(#[from] GitError),
}

#[derive(Debug, Clone, Copy)]
pub enum ErrorKind {
    Usage,
    PolicyDenied,
    SessionGuardDenied,
    LockConflict,
    ScopeViolation,
    DirtyWorktree,
    GitFailure,
    StateInconsistent,
    RecoveryRequired,
    Unsupported,
    Io,
    Database,
}

impl AppError {
    pub fn diagnostic(code: &'static str, message: impl Into<String>, kind: ErrorKind) -> Self {
        Self::Diagnostic {
            code,
            message: message.into(),
            kind,
        }
    }

    pub fn render(&self) -> String {
        self.to_string()
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Diagnostic {
                kind: ErrorKind::Usage,
                ..
            } => 2,
            Self::Diagnostic {
                kind: ErrorKind::PolicyDenied | ErrorKind::SessionGuardDenied,
                ..
            } => 3,
            Self::Diagnostic {
                kind: ErrorKind::DirtyWorktree | ErrorKind::ScopeViolation,
                ..
            } => 4,
            Self::Diagnostic {
                kind: ErrorKind::RecoveryRequired | ErrorKind::StateInconsistent,
                ..
            } => 5,
            Self::Diagnostic { .. }
            | Self::Io(_)
            | Self::Sqlite(_)
            | Self::Json(_)
            | Self::Toml(_)
            | Self::Git(_) => 1,
        }
    }
}

#[derive(Debug, Error)]
#[error("git command failed (status={status}): {stderr}")]
pub struct GitError {
    pub status: String,
    pub stderr: String,
    pub stdout: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(pub String);

impl Id {
    pub fn new(prefix: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        Self(format!("{prefix}{nanos:x}{:x}", std::process::id()))
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Oid(pub String);

impl Oid {
    pub fn parse(value: impl Into<String>) -> Result<Self, AppError> {
        let value = value.into();
        if value.len() < 7 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(AppError::diagnostic(
                "AGT-0101",
                "invalid Git object id",
                ErrorKind::GitFailure,
            ));
        }
        Ok(Self(value))
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BranchRef(pub String);

impl BranchRef {
    pub fn local(name: &str) -> Result<Self, AppError> {
        let full = if name.starts_with("refs/heads/") {
            name.to_owned()
        } else {
            format!("refs/heads/{name}")
        };
        let branch = full.strip_prefix("refs/heads/").unwrap_or_default();
        if branch.is_empty()
            || branch.starts_with('/')
            || branch.contains("..")
            || branch.contains("//")
            || branch.ends_with('/')
        {
            return Err(AppError::diagnostic(
                "AGT-0102",
                "invalid local branch name",
                ErrorKind::Usage,
            ));
        }
        Ok(Self(full))
    }
}

impl fmt::Display for BranchRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoPath(pub PathBuf);

impl RepoPath {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref();
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(AppError::diagnostic(
                "AGT-0103",
                "repository-relative path is required",
                ErrorKind::Usage,
            ));
        }
        Ok(Self(path.to_path_buf()))
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Lifecycle {
    Creating,
    Active,
    Conflicted,
    Landed,
    Archived,
    Broken,
}

impl Lifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Active => "active",
            Self::Conflicted => "conflicted",
            Self::Landed => "landed",
            Self::Archived => "archived",
            Self::Broken => "broken",
        }
    }
    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "creating" => Ok(Self::Creating),
            "active" => Ok(Self::Active),
            "conflicted" => Ok(Self::Conflicted),
            "landed" => Ok(Self::Landed),
            "archived" => Ok(Self::Archived),
            "broken" => Ok(Self::Broken),
            _ => Err(AppError::diagnostic(
                "AGT-0104",
                "unknown task lifecycle",
                ErrorKind::StateInconsistent,
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OperationKind {
    Init,
    CreateTask,
    RemoveWorktree,
    Checkpoint,
    RestoreCheckpoint,
    Fetch,
    Sync,
    Land,
    DeleteBranch,
    Archive,
}

impl OperationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::CreateTask => "create_task",
            Self::RemoveWorktree => "remove_worktree",
            Self::Checkpoint => "checkpoint",
            Self::RestoreCheckpoint => "restore_checkpoint",
            Self::Fetch => "fetch",
            Self::Sync => "sync",
            Self::Land => "land",
            Self::DeleteBranch => "delete_branch",
            Self::Archive => "archive",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OperationStatus {
    Prepared,
    Executing,
    Verifying,
    Completed,
    CleanupPending,
    Failed,
    ManualIntervention,
}

impl OperationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Executing => "executing",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
            Self::CleanupPending => "cleanup_pending",
            Self::Failed => "failed",
            Self::ManualIntervention => "manual_intervention",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum InternalGitProfile {
    Discovery,
    Agent,
    WorktreeManagement,
    Checkpoint,
    Fetch,
    Sync,
    Land,
    RepairReadOnly,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonEnvelope<T: Serialize> {
    pub schema_version: u32,
    pub ok: bool,
    pub command: String,
    pub operation_id: Option<String>,
    pub result: Option<T>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeContentState {
    pub tracked_clean: bool,
    pub nonignored_residue: Vec<String>,
    pub ignored_residue: Vec<String>,
    pub visibility_flags: Vec<String>,
    pub in_progress: bool,
    pub filesystem_entries: Vec<String>,
}

impl WorktreeContentState {
    pub fn mutation_pristine(&self) -> bool {
        self.tracked_clean
            && self.nonignored_residue.is_empty()
            && self.ignored_residue.is_empty()
            && self.visibility_flags.is_empty()
            && !self.in_progress
            && self.filesystem_entries.is_empty()
    }
    pub fn review_clean(&self) -> bool {
        self.tracked_clean
            && self.nonignored_residue.is_empty()
            && self.visibility_flags.is_empty()
            && !self.in_progress
    }
}
