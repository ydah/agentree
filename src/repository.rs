use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    domain::{AppError, ErrorKind},
    git::{args, GitRunner},
};

#[derive(Debug, Clone)]
pub struct RepositoryFacts {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
    pub index_path: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub object_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryManifest {
    pub schema_version: u32,
    pub repository_id: String,
    pub common_dir: String,
    pub state_dir: String,
    pub worktree_root: String,
    pub created_at: u64,
}

impl RepositoryManifest {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.common_dir).join("agentree/repository.json")
    }
}

pub fn discover(git: &GitRunner, start: &Path) -> Result<RepositoryFacts, AppError> {
    let start = if start.is_dir() {
        start
    } else {
        start.parent().unwrap_or(start)
    };
    let root = canonical(PathBuf::from(git.text(
        start,
        crate::domain::InternalGitProfile::Discovery,
        &args(&["rev-parse", "--show-toplevel"]),
    )?))?;
    let common_dir = canonical(resolve_path(
        &root,
        &git.text(
            &root,
            crate::domain::InternalGitProfile::Discovery,
            &args(&["rev-parse", "--git-common-dir"]),
        )?,
    ))?;
    let git_dir = canonical(resolve_path(
        &root,
        &git.text(
            &root,
            crate::domain::InternalGitProfile::Discovery,
            &args(&["rev-parse", "--git-dir"]),
        )?,
    ))?;
    let index_path = canonical(resolve_path(
        &root,
        &git.text(
            &root,
            crate::domain::InternalGitProfile::Discovery,
            &args(&["rev-parse", "--git-path", "index"]),
        )?,
    ))?;
    let branch = git
        .text(
            &root,
            crate::domain::InternalGitProfile::Discovery,
            &args(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
        )
        .ok();
    let head = git.text(
        &root,
        crate::domain::InternalGitProfile::Discovery,
        &args(&["rev-parse", "HEAD"]),
    )?;
    let object_format = git.text(
        &root,
        crate::domain::InternalGitProfile::Discovery,
        &args(&["rev-parse", "--show-object-format"]),
    )?;
    let bare = git.text(
        &root,
        crate::domain::InternalGitProfile::Discovery,
        &args(&["rev-parse", "--is-bare-repository"]),
    )?;
    if bare == "true" {
        return Err(AppError::diagnostic(
            "AGT-0204",
            "bare repositories are not supported",
            ErrorKind::Unsupported,
        ));
    }
    Ok(RepositoryFacts {
        root,
        common_dir,
        git_dir,
        index_path,
        branch,
        head,
        object_format,
    })
}

pub fn manifest_for(facts: &RepositoryFacts) -> RepositoryManifest {
    let repository_id = repository_id(&facts.common_dir);
    let state_dir = facts.common_dir.join("agentree");
    let worktree_root = facts.common_dir.join("agentree/worktrees");
    RepositoryManifest {
        schema_version: 1,
        repository_id,
        common_dir: facts.common_dir.display().to_string(),
        state_dir: state_dir.display().to_string(),
        worktree_root: worktree_root.display().to_string(),
        created_at: now(),
    }
}

pub fn load_manifest(facts: &RepositoryFacts) -> Result<RepositoryManifest, AppError> {
    let path = facts.common_dir.join("agentree/repository.json");
    let bytes = fs::read(&path).map_err(|error| {
        AppError::diagnostic(
            "AGT-0205",
            format!("Agentree is not initialized ({error})"),
            ErrorKind::StateInconsistent,
        )
    })?;
    let manifest: RepositoryManifest = serde_json::from_slice(&bytes)?;
    if manifest.schema_version != 1
        || canonical(PathBuf::from(&manifest.common_dir))? != facts.common_dir
        || manifest.repository_id != repository_id(&facts.common_dir)
    {
        return Err(AppError::diagnostic(
            "AGT-0206",
            "repository manifest identity mismatch",
            ErrorKind::StateInconsistent,
        ));
    }
    Ok(manifest)
}

pub fn publish_manifest(manifest: &RepositoryManifest) -> Result<(), AppError> {
    let path = manifest.path();
    if path.exists() {
        let existing: RepositoryManifest = serde_json::from_slice(&fs::read(&path)?)?;
        if existing.repository_id != manifest.repository_id
            || existing.common_dir != manifest.common_dir
        {
            return Err(AppError::diagnostic(
                "AGT-0207",
                "existing manifest does not match this repository",
                ErrorKind::StateInconsistent,
            ));
        }
        return Ok(());
    }
    durable_replace(&path, &serde_json::to_vec_pretty(manifest)?)
}

pub fn durable_replace(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::diagnostic(
            "AGT-0208",
            "manifest has no parent directory",
            ErrorKind::Io,
        )
    })?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        crate::domain::Id::new("tmp")
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn resolve_path(root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}
fn canonical(path: PathBuf) -> Result<PathBuf, AppError> {
    fs::canonicalize(path).map_err(AppError::from)
}
fn repository_id(common_dir: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(
        common_dir
            .to_str()
            .unwrap_or("<unsupported-path>")
            .as_bytes(),
    );
    format!("repo-{:x}", hash.finalize())
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
