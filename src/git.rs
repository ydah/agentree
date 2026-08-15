use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::domain::{AppError, GitError, InternalGitProfile, Oid};

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: std::process::ExitStatus,
}

#[derive(Debug, Clone)]
pub struct GitRunner {
    pub executable: PathBuf,
    pub hooks_dir: PathBuf,
}

impl GitRunner {
    pub fn resolve() -> Result<Self, AppError> {
        let current = std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::canonicalize(path).ok());
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        for directory in std::env::split_paths(&path_var) {
            let candidate = directory.join(if cfg!(windows) { "git.exe" } else { "git" });
            if !candidate.is_file() {
                continue;
            }
            let canonical = std::fs::canonicalize(&candidate).ok();
            if current.is_some() && canonical == current {
                continue;
            }
            return Ok(Self {
                executable: candidate,
                hooks_dir: std::env::temp_dir()
                    .join(format!("agentree-hooks-{}", std::process::id())),
            });
        }
        Err(AppError::diagnostic(
            "AGT-0201",
            "system Git executable was not found",
            crate::domain::ErrorKind::Unsupported,
        ))
    }

    pub fn prepare_hooks(&self) -> Result<(), AppError> {
        std::fs::create_dir_all(&self.hooks_dir)?;
        if std::fs::read_dir(&self.hooks_dir)?.next().is_some() {
            return Err(AppError::diagnostic(
                "AGT-0209",
                "trusted hooks directory is not empty",
                crate::domain::ErrorKind::StateInconsistent,
            ));
        }
        Ok(())
    }

    pub fn require_options(
        &self,
        cwd: &Path,
        profile: InternalGitProfile,
        command_name: &str,
        options: &[&str],
    ) -> Result<(), AppError> {
        let mut command = Command::new(&self.executable);
        command.current_dir(cwd);
        command.args(self.profile_args(profile));
        command.args([OsString::from(command_name), OsString::from("-h")]);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        sanitize_git_environment(&mut command);
        let output = command.output()?;
        let mut help = output.stdout;
        help.extend(output.stderr);
        let help = String::from_utf8_lossy(&help);
        let missing = options
            .iter()
            .filter(|option| !help.contains(**option))
            .copied()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        Err(AppError::diagnostic(
            "AGT-0210",
            format!(
                "Git {command_name} lacks required capabilities: {}",
                missing.join(", ")
            ),
            crate::domain::ErrorKind::Unsupported,
        ))
    }

    pub fn run(
        &self,
        cwd: &Path,
        profile: InternalGitProfile,
        args: &[OsString],
    ) -> Result<GitOutput, AppError> {
        self.run_with_env(cwd, profile, args, &BTreeMap::new(), None)
    }

    pub fn run_with_env(
        &self,
        cwd: &Path,
        profile: InternalGitProfile,
        args: &[OsString],
        overrides: &BTreeMap<OsString, OsString>,
        input: Option<&[u8]>,
    ) -> Result<GitOutput, AppError> {
        self.prepare_hooks()?;
        let mut command = Command::new(&self.executable);
        command.current_dir(cwd);
        command.args(self.profile_args(profile));
        command.args(args);
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        sanitize_git_environment(&mut command);
        for (key, value) in overrides {
            command.env(key, value);
        }
        let mut child = command.spawn()?;
        if let Some(input) = input {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(input)?;
            }
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(AppError::Git(GitError {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            }));
        }
        Ok(GitOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            status: output.status,
        })
    }

    pub fn text(
        &self,
        cwd: &Path,
        profile: InternalGitProfile,
        args: &[OsString],
    ) -> Result<String, AppError> {
        let output = self.run(cwd, profile, args)?;
        Ok(String::from_utf8(output.stdout)
            .map_err(|_| {
                AppError::diagnostic(
                    "AGT-0202",
                    "Git returned unsupported non-UTF-8 output",
                    crate::domain::ErrorKind::Unsupported,
                )
            })?
            .trim()
            .to_owned())
    }

    pub fn text_with_env(
        &self,
        cwd: &Path,
        profile: InternalGitProfile,
        args: &[OsString],
        overrides: &BTreeMap<OsString, OsString>,
    ) -> Result<String, AppError> {
        let output = self.run_with_env(cwd, profile, args, overrides, None)?;
        Ok(String::from_utf8(output.stdout)
            .map_err(|_| {
                AppError::diagnostic(
                    "AGT-0202",
                    "Git returned unsupported non-UTF-8 output",
                    crate::domain::ErrorKind::Unsupported,
                )
            })?
            .trim()
            .to_owned())
    }

    pub fn oid(&self, cwd: &Path, args: &[OsString]) -> Result<Oid, AppError> {
        Oid::parse(self.text(cwd, InternalGitProfile::Discovery, args)?)
    }

    fn profile_args(&self, profile: InternalGitProfile) -> Vec<OsString> {
        let mut args = Vec::new();
        let add = |args: &mut Vec<OsString>, key: &str, value: &str| {
            args.push(OsString::from("-c"));
            args.push(OsString::from(format!("{key}={value}")));
        };
        add(&mut args, "core.fsmonitor", "false");
        add(
            &mut args,
            "core.hooksPath",
            &self.hooks_dir.to_string_lossy(),
        );
        add(&mut args, "gc.auto", "0");
        add(&mut args, "maintenance.auto", "false");
        match profile {
            InternalGitProfile::Agent => {
                add(&mut args, "diff.external", "");
            }
            InternalGitProfile::WorktreeManagement => add(&mut args, "submodule.recurse", "false"),
            InternalGitProfile::Checkpoint => add(&mut args, "add.ignoreErrors", "false"),
            InternalGitProfile::Fetch => {
                add(&mut args, "fetch.prune", "false");
                add(&mut args, "fetch.pruneTags", "false");
                add(&mut args, "fetch.recurseSubmodules", "false");
                add(&mut args, "fetch.writeCommitGraph", "false");
            }
            InternalGitProfile::Sync => {
                add(&mut args, "rebase.updateRefs", "false");
                add(&mut args, "rebase.autoStash", "false");
                add(&mut args, "rerere.enabled", "false");
                add(&mut args, "rerere.autoupdate", "false");
                add(&mut args, "commit.gpgSign", "false");
            }
            InternalGitProfile::Land => add(&mut args, "merge.autoStash", "false"),
            InternalGitProfile::Discovery | InternalGitProfile::RepairReadOnly => {}
        }
        args
    }
}

fn sanitize_git_environment(command: &mut Command) {
    const KEYS: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_EXTERNAL_DIFF",
        "GIT_DIFF_OPTS",
        "GIT_QUARANTINE_PATH",
        "GIT_SHALLOW_FILE",
        "GIT_EXEC_PATH",
        "GIT_PREFIX",
    ];
    for key in KEYS {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars_os().filter(|(key, _)| {
        key.to_string_lossy().starts_with("GIT_CONFIG_KEY_")
            || key.to_string_lossy().starts_with("GIT_CONFIG_VALUE_")
    }) {
        command.env_remove(key);
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("GIT_PAGER", "cat");
    command.env("GIT_EDITOR", "true");
    command.env("GIT_SEQUENCE_EDITOR", "true");
}

pub fn nul_records(bytes: &[u8]) -> Result<Vec<String>, AppError> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| {
            String::from_utf8(part.to_vec()).map_err(|_| {
                AppError::diagnostic(
                    "AGT-0203",
                    "Git path is not valid UTF-8; this release refuses it",
                    crate::domain::ErrorKind::Unsupported,
                )
            })
        })
        .collect()
}

pub fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}
