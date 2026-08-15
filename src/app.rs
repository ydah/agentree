use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use clap::error::ErrorKind as ClapErrorKind;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    cli::{
        self, CheckpointCommand, Command as CliCommand, ConfigCommand, LandArgs, NewArgs, RunArgs,
        SyncArgs,
    },
    config,
    domain::{
        AppError, BranchRef, ErrorKind, InternalGitProfile, JsonEnvelope, Lifecycle, OperationKind,
        OperationStatus,
    },
    git::{args, GitRunner},
    lock::FileLock,
    repository::{self, RepositoryFacts, RepositoryManifest},
    state::{CheckpointRecord, State, TaskRecord},
    task,
};

pub struct Application;

struct Context {
    git: GitRunner,
    facts: RepositoryFacts,
    manifest: RepositoryManifest,
    state: State,
}

impl Application {
    pub fn dispatch(raw_args: Vec<OsString>) -> Result<i32, AppError> {
        if cli::is_git_shim(&raw_args) {
            return Self::dispatch_shim(raw_args);
        }
        let cli = cli::parse_args(raw_args).map_err(clap_to_error)?;
        let git = GitRunner::resolve()?;
        let start = cli
            .repository
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        let facts = repository::discover(&git, &start)?;
        match cli.command {
            CliCommand::Init => Self::init(&git, &facts, cli.json),
            CliCommand::Config {
                command: ConfigCommand::Scaffold,
            } => Self::scaffold(&facts.root),
            command => {
                let context = Context::open(git, facts)?;
                Self::execute(context, command, cli.json)
            }
        }
    }

    fn execute(context: Context, command: CliCommand, json: bool) -> Result<i32, AppError> {
        match command {
            CliCommand::New(args) => Self::new_task(&context, args, json),
            CliCommand::Status => Self::status(&context, json),
            CliCommand::Context { task: selector } => Self::context(&context, &selector, json),
            CliCommand::Diff { task: selector } => Self::diff(&context, &selector),
            CliCommand::Run(args) => Self::run(&context, args, json),
            CliCommand::Shell { task: selector } => Self::run(
                &context,
                RunArgs {
                    task: selector,
                    program: vec![OsString::from(
                        std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh")),
                    )],
                    require_post_checks: false,
                },
                json,
            ),
            CliCommand::Git {
                task: selector,
                args,
            } => Self::human_git(&context, &selector, args),
            CliCommand::Remove { task: selector } => Self::remove(&context, &selector, json),
            CliCommand::Archive { task: selector } => Self::archive(&context, &selector, json),
            CliCommand::DeleteBranch {
                task: selector,
                yes,
            } => Self::delete_branch(&context, &selector, yes, json),
            CliCommand::Doctor {
                operation,
                apply,
                plan_fingerprint,
            } => Self::doctor(
                &context,
                operation.as_deref(),
                apply,
                plan_fingerprint.as_deref(),
                json,
            ),
            CliCommand::Checkpoint { command } => Self::checkpoint_command(&context, command, json),
            CliCommand::Overlap { tasks } => Self::overlap(&context, tasks, json),
            CliCommand::Check { task: selector } => Self::check(&context, &selector, json),
            CliCommand::Fetch { remote } => Self::fetch(&context, &remote, json),
            CliCommand::Sync(args) => Self::sync(&context, args, json),
            CliCommand::Resolve {
                task: selector,
                shell,
            } => {
                if !shell {
                    return Err(AppError::diagnostic(
                        "AGT-0701",
                        "resolve requires --shell",
                        ErrorKind::Usage,
                    ));
                }
                Self::run(
                    &context,
                    RunArgs {
                        task: selector,
                        program: vec![OsString::from(
                            std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh")),
                        )],
                        require_post_checks: false,
                    },
                    json,
                )
            }
            CliCommand::Land(args) => Self::land(&context, args, json),
            CliCommand::Init | CliCommand::Config { .. } => unreachable!(),
        }
    }

    fn init(git: &GitRunner, facts: &RepositoryFacts, json: bool) -> Result<i32, AppError> {
        let manifest = repository::manifest_for(facts);
        fs::create_dir_all(&manifest.state_dir)?;
        fs::create_dir_all(&manifest.worktree_root)?;
        git.prepare_hooks()?;
        let _lock = FileLock::acquire(&PathBuf::from(&manifest.state_dir).join("repository.lock"))?;
        let state = State::open(&PathBuf::from(&manifest.state_dir).join("state.sqlite3"))?;
        state.register_repository(
            &manifest.repository_id,
            &facts.common_dir,
            Path::new(&manifest.state_dir),
            &facts.object_format,
        )?;
        repository::publish_manifest(&manifest)?;
        output(
            json,
            "init",
            None,
            &serde_json::json!({ "repository_id": manifest.repository_id, "state_dir": manifest.state_dir, "worktree_root": manifest.worktree_root }),
        )
    }

    fn scaffold(root: &Path) -> Result<i32, AppError> {
        let path = root.join(".agentree.toml");
        if path.exists() {
            return Err(AppError::diagnostic(
                "AGT-0702",
                ".agentree.toml already exists",
                ErrorKind::StateInconsistent,
            ));
        }
        repository::durable_replace(&path, config::scaffold().as_bytes())?;
        println!("created {}", path.display());
        Ok(0)
    }

    fn new_task(context: &Context, arguments: NewArgs, json: bool) -> Result<i32, AppError> {
        let base = arguments.base.unwrap_or_else(|| "HEAD".to_owned());
        let base_oid = context.git.text(
            &context.facts.root,
            InternalGitProfile::Discovery,
            &[
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from(format!("{base}^{{commit}}")),
            ],
        )?;
        let task_id = crate::domain::Id::new("task-").0;
        let branch = task::branch_for(&arguments.slug, &task_id)?;
        let path = task::path_for(
            Path::new(&context.manifest.worktree_root),
            &arguments.slug,
            &task_id,
        )?;
        let snapshot = config::snapshot(&context.git, &context.facts.root, &base_oid)?;
        let expected = serde_json::json!({ "branch": branch, "path": path, "base_oid": base_oid });
        let task_record = TaskRecord {
            id: task_id.clone(),
            slug: arguments.slug.clone(),
            branch: branch.clone(),
            path: path.clone(),
            base_oid: base_oid.clone(),
            head_oid: base_oid.clone(),
            lifecycle: Lifecycle::Creating,
            config_hash: snapshot.hash.clone(),
            scopes: arguments.scopes,
        };
        context.state.insert_task(&task_record)?;
        let operation = context.state.create_operation(
            OperationKind::CreateTask,
            Some(&task_id),
            &expected.to_string(),
        )?;
        let _task_lock = task_lock(&context.manifest, &task_id)?;
        let _repo_lock = repo_lock(&context.manifest)?;
        let branch_status = context.git.run(
            &context.facts.root,
            InternalGitProfile::WorktreeManagement,
            &[
                OsString::from("branch"),
                OsString::from(&branch),
                OsString::from(&base_oid),
            ],
        );
        if let Err(error) = branch_status {
            let _ = context.state.update_operation(
                &operation.id,
                OperationStatus::ManualIntervention,
                &format!("branch creation failed: {error}"),
            );
            return Err(error);
        }
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            &serde_json::json!({ "phase": "branch_created" }).to_string(),
        )?;
        let add_result = context.git.run(
            &context.facts.root,
            InternalGitProfile::WorktreeManagement,
            &[
                OsString::from("worktree"),
                OsString::from("add"),
                path.as_os_str().to_owned(),
                OsString::from(&branch),
            ],
        );
        if let Err(error) = add_result {
            let _ = context.state.update_operation(
                &operation.id,
                OperationStatus::ManualIntervention,
                &serde_json::json!({ "phase": "branch_created", "error": error.to_string() })
                    .to_string(),
            );
            return Err(error);
        }
        context.state.update_operation(
            &operation.id,
            OperationStatus::Verifying,
            &serde_json::json!({ "phase": "worktree_added" }).to_string(),
        )?;
        let task_facts = RepositoryFacts {
            root: path.clone(),
            common_dir: context.facts.common_dir.clone(),
            git_dir: resolve_git_dir(&context.git, &path)?,
            index_path: resolve_git_path(&context.git, &path, "index")?,
            branch: Some(branch.trim_start_matches("refs/heads/").to_owned()),
            head: base_oid.clone(),
            object_format: context.facts.object_format.clone(),
        };
        if let Err(error) = task::ensure_mutation_pristine(&context.git, &task_facts, &path) {
            let _ = context
                .state
                .update_task_lifecycle(&task_id, Lifecycle::Broken, None, None);
            let _ = context.state.update_operation(
                &operation.id,
                OperationStatus::ManualIntervention,
                &serde_json::json!({ "phase": "worktree_added", "error": error.to_string() })
                    .to_string(),
            );
            return Err(error);
        }
        let marker = task::marker_path(&context.git, &path)?;
        repository::durable_replace(
            &marker,
            &serde_json::to_vec(
                &serde_json::json!({ "schema_version": 1, "repository_id": context.manifest.repository_id, "task_id": task_id, "branch": branch, "worktree": path }),
            )?,
        )?;
        context.state.save_config(&task_id, &snapshot)?;
        context
            .state
            .update_task_lifecycle(&task_id, Lifecycle::Active, Some(&base_oid), None)?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Completed,
            &serde_json::json!({ "phase": "active" }).to_string(),
        )?;
        output(
            json,
            "new",
            Some(&operation.id),
            &serde_json::json!({ "task_id": task_id, "branch": branch, "worktree": path, "base_oid": base_oid, "config_hash": snapshot.hash }),
        )
    }

    fn status(context: &Context, json: bool) -> Result<i32, AppError> {
        let tasks = context.state.tasks()?;
        let mut result = Vec::new();
        for record in &tasks {
            let facts = task_facts(&context.git, record, &context.facts)?;
            let content = task::content_state(&context.git, &facts, &record.path)?;
            result.push(serde_json::json!({ "task_id": record.id, "slug": record.slug, "state": record.lifecycle.as_str(), "branch": record.branch, "head": record.head_oid, "worktree": record.path, "dirty": !content.review_clean(), "mutation_pristine": content.mutation_pristine(), "session": context.state.sessions_for_task(&record.id)? }));
        }
        if json {
            print_json("status", &result);
        } else {
            println!("TASK\tSTATE\tHEAD\tDIRTY\tSESSION\tWORKTREE");
            for item in &result {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    item["slug"],
                    item["state"],
                    item["head"].as_str().unwrap_or(""),
                    item["dirty"],
                    item["session"],
                    item["worktree"]
                );
            }
        }
        Ok(0)
    }

    fn context(context: &Context, selector: &str, json: bool) -> Result<i32, AppError> {
        let record = context.state.task(selector)?;
        let facts = task_facts(&context.git, &record, &context.facts)?;
        let content = task::content_state(&context.git, &facts, &record.path)?;
        let checks = context
            .state
            .config(&record.id)
            .map(|config| config.checks)
            .unwrap_or_default();
        output(
            json,
            "context",
            None,
            &serde_json::json!({ "task_id": record.id, "slug": record.slug, "branch": record.branch, "worktree": record.path, "base_oid": record.base_oid, "head_oid": record.head_oid, "config_hash": record.config_hash, "scopes": record.scopes, "content": content, "checks": checks, "warnings": ["Git shim is a guardrail, not an OS security boundary", "path overlap is heuristic and not semantic conflict detection"] }),
        )
    }

    fn diff(context: &Context, selector: &str) -> Result<i32, AppError> {
        let record = context.state.task(selector)?;
        let output = context.git.run(
            &record.path,
            InternalGitProfile::Agent,
            &args(&["diff", "--stat"]),
        )?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        Ok(0)
    }

    fn run(context: &Context, arguments: RunArgs, json: bool) -> Result<i32, AppError> {
        let record = context.state.task(&arguments.task)?;
        if record.lifecycle != Lifecycle::Active {
            return Err(AppError::diagnostic(
                "AGT-0703",
                "task is not active",
                ErrorKind::StateInconsistent,
            ));
        }
        if context.state.sessions_for_task(&record.id)? > 0 {
            return Err(AppError::diagnostic(
                "AGT-0704",
                "a session is already active for this task",
                ErrorKind::LockConflict,
            ));
        }
        task::facts_match(&context.git, &record)?;
        let session_id = context.state.start_session(&record.id, 0, 0)?;
        let session_root = PathBuf::from(&context.manifest.state_dir)
            .join("sessions")
            .join(&session_id);
        let shim_dir = session_root.join("bin");
        fs::create_dir_all(&shim_dir)?;
        let executable = std::env::current_exe()?;
        let shim = shim_dir.join("git");
        create_link_or_copy(&executable, &shim)?;
        let mut command = Command::new(&arguments.program[0]);
        command.args(arguments.program.iter().skip(1));
        command.current_dir(&record.path);
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        sanitize_child_environment(&mut command);
        command.env("AGENTREE_SESSION_ID", &session_id);
        command.env("AGENTREE_TASK_ID", &record.id);
        command.env("AGENTREE_REPOSITORY_ID", &context.manifest.repository_id);
        command.env("AGENTREE_CONFIG_HASH", &record.config_hash);
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![shim_dir.clone()];
        paths.extend(std::env::split_paths(&old_path).filter(|path| path != &shim_dir));
        command.env(
            "PATH",
            std::env::join_paths(paths).map_err(|_| {
                AppError::diagnostic("AGT-0705", "cannot construct child PATH", ErrorKind::Io)
            })?,
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        #[cfg(unix)]
        {
            let _ = unsafe { libc::setpgid(pid as i32, pid as i32) };
        }
        let status = child.wait()?;
        let exit_code = status
            .code()
            .or_else(|| if status.success() { Some(0) } else { Some(1) });
        context.state.finish_session(
            &session_id,
            if status.success() {
                "finished"
            } else {
                "failed"
            },
            exit_code,
        )?;
        let observed_head = context.git.text(
            &record.path,
            InternalGitProfile::Discovery,
            &args2(&["rev-parse", "HEAD"]),
        )?;
        context.state.update_task_lifecycle(
            &record.id,
            record.lifecycle,
            Some(&observed_head),
            None,
        )?;
        let mut post_run_record = record.clone();
        post_run_record.head_oid = observed_head;
        let checkpoint = Self::checkpoint_task(context, &post_run_record, None)
            .map(|_| "created")
            .unwrap_or("not-created");
        let result = serde_json::json!({ "session_id": session_id, "child_exit_code": exit_code, "post_run_checkpoint": checkpoint });
        if json {
            print_json("run", &result);
        }
        Ok(exit_code.unwrap_or(1))
    }

    fn human_git(
        context: &Context,
        selector: &str,
        git_args: Vec<OsString>,
    ) -> Result<i32, AppError> {
        deny_inside_session()?;
        let record = context.state.task(selector)?;
        task::facts_match(&context.git, &record)?;
        let decision = policy_decision(&git_args)?;
        if !decision {
            return Err(AppError::diagnostic(
                "AGT-0710",
                "Git command denied by task policy",
                ErrorKind::PolicyDenied,
            ));
        }
        let output = context
            .git
            .run(&record.path, InternalGitProfile::Agent, &git_args)?;
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        Ok(output.status.code().unwrap_or(1))
    }

    fn remove(context: &Context, selector: &str, json: bool) -> Result<i32, AppError> {
        let record = context.state.task(selector)?;
        if context.state.sessions_for_task(&record.id)? > 0 {
            return Err(AppError::diagnostic(
                "AGT-0711",
                "active session prevents removal",
                ErrorKind::LockConflict,
            ));
        }
        if !Path::new(&context.manifest.worktree_root)
            .canonicalize()?
            .starts_with(
                Path::new(&record.path)
                    .canonicalize()
                    .unwrap_or_else(|_| record.path.clone())
                    .parent()
                    .unwrap_or(Path::new("/")),
            )
        {
            return Err(AppError::diagnostic(
                "AGT-0712",
                "worktree is outside the managed root",
                ErrorKind::StateInconsistent,
            ));
        }
        task::facts_match(&context.git, &record)?;
        let facts = task_facts(&context.git, &record, &context.facts)?;
        let content = task::content_state(&context.git, &facts, &record.path)?;
        if !content.mutation_pristine() {
            return Err(AppError::diagnostic(
                "AGT-0713",
                "remove requires a residue-free worktree; use archive for dirty tasks",
                ErrorKind::DirtyWorktree,
            ));
        }
        verify_marker(context, &record)?;
        let operation = context.state.create_operation(OperationKind::RemoveWorktree, Some(&record.id), &serde_json::json!({ "path": record.path, "branch": record.branch, "head": record.head_oid }).to_string())?;
        let _task_lock = task_lock(&context.manifest, &record.id)?;
        let _repo_lock = repo_lock(&context.manifest)?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            "{\"phase\":\"remove_started\"}",
        )?;
        context.git.run(
            &context.facts.root,
            InternalGitProfile::WorktreeManagement,
            &[
                OsString::from("worktree"),
                OsString::from("remove"),
                record.path.as_os_str().to_owned(),
            ],
        )?;
        if record.path.exists() {
            return Err(AppError::diagnostic(
                "AGT-0714",
                "Git reported removal but worktree path remains",
                ErrorKind::RecoveryRequired,
            ));
        }
        context
            .state
            .update_task_lifecycle(&record.id, Lifecycle::Archived, None, None)?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Completed,
            "{\"phase\":\"archived\"}",
        )?;
        output(
            json,
            "remove",
            Some(&operation.id),
            &serde_json::json!({ "task_id": record.id, "branch_retained": true }),
        )
    }

    fn archive(context: &Context, selector: &str, json: bool) -> Result<i32, AppError> {
        let record = context.state.task(selector)?;
        if context.state.sessions_for_task(&record.id)? > 0 {
            return Err(AppError::diagnostic(
                "AGT-0715",
                "active session prevents archive",
                ErrorKind::LockConflict,
            ));
        }
        let operation = context.state.create_operation(
            OperationKind::Archive,
            Some(&record.id),
            &serde_json::json!({ "path": record.path, "branch": record.branch }).to_string(),
        )?;
        context
            .state
            .update_task_lifecycle(&record.id, Lifecycle::Archived, None, None)?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Completed,
            "{\"phase\":\"metadata_only\"}",
        )?;
        output(
            json,
            "archive",
            Some(&operation.id),
            &serde_json::json!({ "task_id": record.id, "filesystem_changed": false, "branch_retained": true }),
        )
    }

    fn delete_branch(
        context: &Context,
        selector: &str,
        yes: bool,
        json: bool,
    ) -> Result<i32, AppError> {
        if !yes {
            return Err(AppError::diagnostic(
                "AGT-0716",
                "branch deletion requires --yes",
                ErrorKind::Usage,
            ));
        }
        let record = context.state.task(selector)?;
        if record.path.exists() {
            return Err(AppError::diagnostic(
                "AGT-0717",
                "remove the managed worktree before deleting its branch",
                ErrorKind::StateInconsistent,
            ));
        }
        let branch_oid = context.git.text(
            &context.facts.root,
            InternalGitProfile::Discovery,
            &[OsString::from("rev-parse"), OsString::from(&record.branch)],
        )?;
        if branch_oid != record.head_oid {
            return Err(AppError::diagnostic(
                "AGT-0718",
                "task branch moved outside Agentree",
                ErrorKind::StateInconsistent,
            ));
        }
        let operation = context.state.create_operation(
            OperationKind::DeleteBranch,
            Some(&record.id),
            &serde_json::json!({ "branch": record.branch, "oid": branch_oid }).to_string(),
        )?;
        let safety = format!("refs/agentree/safety/delete/{}/{}", record.id, operation.id);
        context.git.run(
            &context.facts.root,
            InternalGitProfile::RepairReadOnly,
            &[
                OsString::from("update-ref"),
                OsString::from(&safety),
                OsString::from(&branch_oid),
            ],
        )?;
        context.git.run(
            &context.facts.root,
            InternalGitProfile::RepairReadOnly,
            &[
                OsString::from("update-ref"),
                OsString::from("-d"),
                OsString::from(&record.branch),
                OsString::from(&branch_oid),
            ],
        )?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Completed,
            "{\"phase\":\"branch_deleted\"}",
        )?;
        output(
            json,
            "delete-branch",
            Some(&operation.id),
            &serde_json::json!({ "task_id": record.id, "branch_deleted": true, "safety_ref": safety }),
        )
    }

    fn doctor(
        context: &Context,
        operation: Option<&str>,
        apply: bool,
        plan_fingerprint: Option<&str>,
        json: bool,
    ) -> Result<i32, AppError> {
        if apply {
            return Err(AppError::diagnostic(
                "AGT-0719",
                "doctor --apply is not enabled until operation-scoped recovery plans are available",
                ErrorKind::Unsupported,
            ));
        }
        let operations = context.state.incomplete_operations()?;
        let selected = operations.into_iter().filter(|item| operation.is_none_or(|id| id == item.id)).map(|item| serde_json::json!({ "operation_id": item.id, "kind": item.kind, "status": item.status, "expected": item.expected, "observed": item.observed, "action": "inspect Git facts; no automatic rollback or deletion" })).collect::<Vec<_>>();
        let _ = plan_fingerprint;
        output(
            json,
            "doctor",
            None,
            &serde_json::json!({ "read_only": true, "operations": selected }),
        )
    }

    fn checkpoint_command(
        context: &Context,
        command: CheckpointCommand,
        json: bool,
    ) -> Result<i32, AppError> {
        match command {
            CheckpointCommand::Create {
                task: selector,
                message,
            } => {
                let record = context.state.task(&selector)?;
                let checkpoint = Self::checkpoint_task(context, &record, message.as_deref())?;
                output(
                    json,
                    "checkpoint",
                    Some(&checkpoint.id),
                    &serde_json::json!(checkpoint),
                )
            }
            CheckpointCommand::List { task: selector } => {
                let record = context.state.task(&selector)?;
                output(
                    json,
                    "checkpoint-list",
                    None,
                    &serde_json::to_value(context.state.checkpoints(&record.id)?)?,
                )
            }
            CheckpointCommand::Show { id } => {
                let found = context.state.tasks()?.into_iter().find_map(|task| {
                    context
                        .state
                        .checkpoints(&task.id)
                        .ok()
                        .and_then(|items| items.into_iter().find(|item| item.id == id))
                });
                let checkpoint = found.ok_or_else(|| {
                    AppError::diagnostic("AGT-0720", "checkpoint not found", ErrorKind::Usage)
                })?;
                output(
                    json,
                    "checkpoint-show",
                    None,
                    &serde_json::to_value(checkpoint)?,
                )
            }
            CheckpointCommand::Restore { id, to_new_task } => {
                Self::restore_checkpoint(context, &id, &to_new_task, json)
            }
        }
    }

    fn checkpoint_task(
        context: &Context,
        record: &TaskRecord,
        message: Option<&str>,
    ) -> Result<CheckpointRecord, AppError> {
        let mut record = record.clone();
        let observed_head = context.git.text(
            &record.path,
            InternalGitProfile::Discovery,
            &args2(&["rev-parse", "HEAD"]),
        )?;
        if observed_head != record.head_oid {
            context.state.update_task_lifecycle(
                &record.id,
                record.lifecycle,
                Some(&observed_head),
                None,
            )?;
            record.head_oid = observed_head;
        }
        task::facts_match(&context.git, &record)?;
        let facts = task_facts(&context.git, &record, &context.facts)?;
        let state = task::content_state(&context.git, &facts, &record.path)?;
        if state.in_progress || !state.visibility_flags.is_empty() {
            return Err(AppError::diagnostic(
                "AGT-0721",
                "checkpoint does not support in-progress or visibility-suppressing index state",
                ErrorKind::Unsupported,
            ));
        }
        let operation = context.state.create_operation(
            OperationKind::Checkpoint,
            Some(&record.id),
            &serde_json::json!({ "head": record.head_oid, "index": facts.index_path }).to_string(),
        )?;
        let tmp_root = PathBuf::from(&context.manifest.state_dir).join("tmp");
        fs::create_dir_all(&tmp_root)?;
        let frozen = tmp_root.join(format!("index-{}", operation.id));
        fs::copy(&facts.index_path, &frozen)?;
        let before = sha256_file(&facts.index_path)?;
        if before != sha256_file(&frozen)? {
            return Err(AppError::diagnostic(
                "AGT-0722",
                "real index changed while being copied",
                ErrorKind::RecoveryRequired,
            ));
        }
        let mut env = BTreeMap::new();
        env.insert(
            OsString::from("GIT_INDEX_FILE"),
            frozen.as_os_str().to_owned(),
        );
        let index_tree = context.git.text_with_env(
            &facts.root,
            InternalGitProfile::Checkpoint,
            &args(&["write-tree"]),
            &env,
        )?;
        let copy_one = tmp_root.join(format!("work-1-{}", operation.id));
        let copy_two = tmp_root.join(format!("work-2-{}", operation.id));
        fs::copy(&frozen, &copy_one)?;
        fs::copy(&frozen, &copy_two)?;
        let mut env_one = BTreeMap::new();
        env_one.insert(
            OsString::from("GIT_INDEX_FILE"),
            copy_one.as_os_str().to_owned(),
        );
        let mut env_two = BTreeMap::new();
        env_two.insert(
            OsString::from("GIT_INDEX_FILE"),
            copy_two.as_os_str().to_owned(),
        );
        context.git.run_with_env(
            &record.path,
            InternalGitProfile::Checkpoint,
            &args(&["add", "-A", "--", "."]),
            &env_one,
            None,
        )?;
        let tree_one = context.git.text_with_env(
            &record.path,
            InternalGitProfile::Checkpoint,
            &args(&["write-tree"]),
            &env_one,
        )?;
        context.git.run_with_env(
            &record.path,
            InternalGitProfile::Checkpoint,
            &args(&["add", "-A", "--", "."]),
            &env_two,
            None,
        )?;
        let tree_two = context.git.text_with_env(
            &record.path,
            InternalGitProfile::Checkpoint,
            &args(&["write-tree"]),
            &env_two,
        )?;
        if tree_one != tree_two || before != sha256_file(&facts.index_path)? {
            return Err(AppError::diagnostic(
                "AGT-0723",
                "working tree was not stable during checkpoint capture",
                ErrorKind::RecoveryRequired,
            ));
        }
        let checkpoint_id = crate::domain::Id::new("checkpoint-").0;
        let snapshot = serde_json::json!({ "schema_version": 1, "checkpoint_id": checkpoint_id, "task_id": record.id, "head_oid": record.head_oid, "index_tree_oid": index_tree, "worktree_tree_oid": tree_one, "config_hash": record.config_hash, "message": message });
        let blob = context.git.run_with_env(
            &facts.root,
            InternalGitProfile::Checkpoint,
            &args(&["hash-object", "-w", "--stdin"]),
            &BTreeMap::new(),
            Some(snapshot.to_string().as_bytes()),
        )?;
        let blob_oid = String::from_utf8_lossy(&blob.stdout).trim().to_owned();
        let tree_input = format!("100644 blob {blob_oid}\tsnapshot.json\n040000 tree {index_tree}\tindex\n040000 tree {tree_one}\tworktree\n");
        let root_tree = context.git.run_with_env(
            &facts.root,
            InternalGitProfile::Checkpoint,
            &args(&["mktree"]),
            &BTreeMap::new(),
            Some(tree_input.as_bytes()),
        )?;
        let root_tree = String::from_utf8_lossy(&root_tree.stdout).trim().to_owned();
        let metadata = context.git.run_with_env(
            &facts.root,
            InternalGitProfile::Checkpoint,
            &args(&["commit-tree", &root_tree, "-p", &record.head_oid]),
            &BTreeMap::new(),
            Some(
                format!(
                    "Agentree checkpoint {checkpoint_id}\n\n{}",
                    message.unwrap_or_default()
                )
                .as_bytes(),
            ),
        )?;
        let metadata_oid = String::from_utf8_lossy(&metadata.stdout).trim().to_owned();
        let immutable = format!(
            "refs/agentree/checkpoints/{}/{}/{}",
            context.manifest.repository_id, record.id, checkpoint_id
        );
        context.git.run(
            &facts.root,
            InternalGitProfile::RepairReadOnly,
            &args(&["update-ref", &immutable, &metadata_oid]),
        )?;
        let latest = format!(
            "refs/agentree/checkpoints/{}/{}/latest",
            context.manifest.repository_id, record.id
        );
        context.git.run(
            &facts.root,
            InternalGitProfile::RepairReadOnly,
            &args(&["update-ref", &latest, &metadata_oid]),
        )?;
        let checkpoint = CheckpointRecord {
            id: checkpoint_id,
            task_id: record.id.clone(),
            head_oid: record.head_oid.clone(),
            index_tree_oid: index_tree,
            worktree_tree_oid: tree_one,
            metadata_oid,
            message: message.map(str::to_owned),
            config_hash: record.config_hash.clone(),
        };
        context.state.save_checkpoint(&checkpoint)?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Completed,
            &serde_json::json!({ "checkpoint_id": checkpoint.id }).to_string(),
        )?;
        let _ = fs::remove_file(frozen);
        let _ = fs::remove_file(copy_one);
        let _ = fs::remove_file(copy_two);
        Ok(checkpoint)
    }

    fn restore_checkpoint(
        context: &Context,
        checkpoint_id: &str,
        slug: &str,
        json: bool,
    ) -> Result<i32, AppError> {
        let source = context
            .state
            .tasks()?
            .into_iter()
            .find_map(|task| {
                context
                    .state
                    .checkpoints(&task.id)
                    .ok()
                    .and_then(|items| items.into_iter().find(|item| item.id == checkpoint_id))
            })
            .ok_or_else(|| {
                AppError::diagnostic("AGT-0724", "checkpoint not found", ErrorKind::Usage)
            })?;
        let task_id = crate::domain::Id::new("task-").0;
        let branch = task::branch_for(slug, &task_id)?;
        let path = task::path_for(Path::new(&context.manifest.worktree_root), slug, &task_id)?;
        let source_task = context.state.task(&source.task_id)?;
        let operation = context.state.create_operation(
            OperationKind::CreateTask,
            Some(&task_id),
            &serde_json::json!({ "restore_from": checkpoint_id, "base_oid": source.head_oid })
                .to_string(),
        )?;
        context.git.run(
            &context.facts.root,
            InternalGitProfile::WorktreeManagement,
            &args(&["branch", &branch, &source.head_oid]),
        )?;
        context.git.run(
            &context.facts.root,
            InternalGitProfile::WorktreeManagement,
            &[
                OsString::from("worktree"),
                OsString::from("add"),
                path.as_os_str().to_owned(),
                OsString::from(&branch),
            ],
        )?;
        let record = TaskRecord {
            id: task_id.clone(),
            slug: slug.to_owned(),
            branch: branch.clone(),
            path: path.clone(),
            base_oid: source.head_oid.clone(),
            head_oid: source.head_oid.clone(),
            lifecycle: Lifecycle::Creating,
            config_hash: source_task.config_hash.clone(),
            scopes: source_task.scopes.clone(),
        };
        context.state.insert_task(&record)?;
        let mut env = BTreeMap::new();
        env.insert(
            OsString::from("GIT_INDEX_FILE"),
            resolve_git_path(&context.git, &path, "index")?
                .as_os_str()
                .to_owned(),
        );
        context.git.run_with_env(
            &path,
            InternalGitProfile::Checkpoint,
            &args(&["read-tree", "--reset", "-u", &source.worktree_tree_oid]),
            &env,
            None,
        )?;
        context.git.run_with_env(
            &path,
            InternalGitProfile::Checkpoint,
            &args(&["read-tree", &source.index_tree_oid]),
            &env,
            None,
        )?;
        let facts = task_facts(&context.git, &record, &context.facts)?;
        if !task::content_state(&context.git, &facts, &path)?.review_clean() {
            return Err(AppError::diagnostic(
                "AGT-0725",
                "restored checkpoint did not reproduce a review-clean state",
                ErrorKind::RecoveryRequired,
            ));
        }
        let marker = task::marker_path(&context.git, &path)?;
        repository::durable_replace(
            &marker,
            &serde_json::to_vec(
                &serde_json::json!({ "schema_version": 1, "repository_id": context.manifest.repository_id, "task_id": task_id, "branch": branch, "worktree": path, "restored_from": checkpoint_id }),
            )?,
        )?;
        context
            .state
            .save_config(&task_id, &context.state.config(&source_task.id)?)?;
        context.state.update_task_lifecycle(
            &task_id,
            Lifecycle::Active,
            Some(&source.head_oid),
            None,
        )?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Completed,
            &serde_json::json!({ "restored_from": checkpoint_id }).to_string(),
        )?;
        output(
            json,
            "checkpoint-restore",
            Some(&operation.id),
            &serde_json::json!({ "task_id": task_id, "worktree": path, "source_checkpoint": checkpoint_id }),
        )
    }

    fn overlap(context: &Context, selectors: Vec<String>, json: bool) -> Result<i32, AppError> {
        let tasks = if selectors.is_empty() {
            context.state.tasks()?
        } else {
            selectors
                .iter()
                .map(|selector| context.state.task(selector))
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut changes = BTreeMap::new();
        for record in &tasks {
            changes.insert(
                record.id.clone(),
                task::all_change_paths(&context.git, record)?,
            );
        }
        let mut pairs = Vec::new();
        let ids: Vec<_> = changes.keys().cloned().collect();
        for (index, left) in ids.iter().enumerate() {
            for right in ids.iter().skip(index + 1) {
                let intersection: Vec<_> = changes[left]
                    .intersection(&changes[right])
                    .cloned()
                    .collect();
                if !intersection.is_empty() {
                    pairs.push(serde_json::json!({ "left": left, "right": right, "paths": intersection, "semantic_conflict": false }));
                }
            }
        }
        output(
            json,
            "overlap",
            None,
            &serde_json::json!({ "pairs": pairs, "heuristic": true }),
        )
    }

    fn check(context: &Context, selector: &str, json: bool) -> Result<i32, AppError> {
        let record = context.state.task(selector)?;
        if context.state.sessions_for_task(&record.id)? > 0 {
            return Err(AppError::diagnostic(
                "AGT-0726",
                "active session prevents check",
                ErrorKind::LockConflict,
            ));
        }
        let snapshot = context.state.config(&record.id)?;
        let start_head = context.git.text(
            &record.path,
            InternalGitProfile::Discovery,
            &args(&["rev-parse", "HEAD"]),
        )?;
        let mut results = Vec::new();
        for definition in snapshot.checks {
            let mut command = Command::new(&definition.command[0]);
            command.args(definition.command.iter().skip(1));
            command.current_dir(&record.path);
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            sanitize_child_environment(&mut command);
            let output = command.output()?;
            let end_head = context.git.text(
                &record.path,
                InternalGitProfile::Discovery,
                &args(&["rev-parse", "HEAD"]),
            )?;
            let stale = start_head != end_head
                || !task::content_state(
                    &context.git,
                    &task_facts(&context.git, &record, &context.facts)?,
                    &record.path,
                )?
                .review_clean();
            results.push(serde_json::json!({ "name": definition.name, "required": definition.required, "exit_code": output.status.code(), "passed": output.status.success(), "stale": stale, "head_oid": start_head, "stdout": String::from_utf8_lossy(&output.stdout), "stderr": String::from_utf8_lossy(&output.stderr) }));
        }
        output(
            json,
            "check",
            None,
            &serde_json::json!({ "task_id": record.id, "results": results }),
        )
    }

    fn fetch(context: &Context, remote: &str, json: bool) -> Result<i32, AppError> {
        if remote.is_empty() || remote.contains('/') || remote.contains(char::is_whitespace) {
            return Err(AppError::diagnostic(
                "AGT-0727",
                "invalid remote name",
                ErrorKind::Usage,
            ));
        }
        let _repo_lock = repo_lock(&context.manifest)?;
        context.git.text(
            &context.facts.root,
            InternalGitProfile::Discovery,
            &[
                OsString::from("remote"),
                OsString::from("get-url"),
                OsString::from(remote),
            ],
        )?;
        let operation = context.state.create_operation(
            OperationKind::Fetch,
            None,
            &serde_json::json!({ "remote": remote }).to_string(),
        )?;
        let refspec = format!("+refs/heads/*:refs/remotes/{remote}/*");
        let args = vec![
            OsString::from("fetch"),
            OsString::from("--no-prune"),
            OsString::from("--no-tags"),
            OsString::from("--no-recurse-submodules"),
            OsString::from("--no-write-fetch-head"),
            OsString::from(remote),
            OsString::from(refspec),
        ];
        context
            .git
            .run(&context.facts.root, InternalGitProfile::Fetch, &args)?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Completed,
            "{\"phase\":\"refs_verified\"}",
        )?;
        output(
            json,
            "fetch",
            Some(&operation.id),
            &serde_json::json!({ "remote": remote, "fetch_head_changed": false }),
        )
    }

    fn sync(context: &Context, arguments: SyncArgs, json: bool) -> Result<i32, AppError> {
        let record = context.state.task(&arguments.task)?;
        let target = arguments.onto.ok_or_else(|| {
            AppError::diagnostic(
                "AGT-0728",
                "sync requires --onto <branch>",
                ErrorKind::Usage,
            )
        })?;
        let target_oid = context.git.text(
            &context.facts.root,
            InternalGitProfile::Discovery,
            &[
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from(format!("refs/heads/{target}")),
            ],
        )?;
        if arguments.r#continue || arguments.abort {
            return Self::sync_continuation(
                context,
                &record,
                arguments.r#continue,
                arguments.abort,
                json,
            );
        }
        if record.lifecycle != Lifecycle::Active {
            return Err(AppError::diagnostic(
                "AGT-0729",
                "sync requires an active task",
                ErrorKind::StateInconsistent,
            ));
        }
        let facts = task_facts(&context.git, &record, &context.facts)?;
        task::ensure_mutation_pristine(&context.git, &facts, &record.path)?;
        let operation = context.state.create_operation(OperationKind::Sync, Some(&record.id), &serde_json::json!({ "target": target, "target_oid": target_oid, "pre_head": record.head_oid, "base": record.base_oid }).to_string())?;
        let safety = format!("refs/agentree/safety/sync/{}/{}", record.id, operation.id);
        context.git.run(
            &context.facts.root,
            InternalGitProfile::RepairReadOnly,
            &args(&["update-ref", &safety, &record.head_oid]),
        )?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            "{\"phase\":\"safety_ref_created\"}",
        )?;
        let args = vec![
            OsString::from("rebase"),
            OsString::from("--no-update-refs"),
            OsString::from("--no-autostash"),
            OsString::from("--onto"),
            OsString::from(&target_oid),
            OsString::from(&record.base_oid),
        ];
        let result = context
            .git
            .run(&record.path, InternalGitProfile::Sync, &args);
        match result {
            Ok(_) => {
                let head = context.git.text(
                    &record.path,
                    InternalGitProfile::Discovery,
                    &args2(&["rev-parse", "HEAD"]),
                )?;
                context.state.update_task_lifecycle(
                    &record.id,
                    Lifecycle::Active,
                    Some(&head),
                    Some(&target_oid),
                )?;
                context.state.update_operation(
                    &operation.id,
                    OperationStatus::Completed,
                    &serde_json::json!({ "phase": "rebase_succeeded", "head": head }).to_string(),
                )?;
                output(
                    json,
                    "sync",
                    Some(&operation.id),
                    &serde_json::json!({ "task_id": record.id, "head_oid": head, "target_oid": target_oid }),
                )
            }
            Err(error) => {
                let _ = context.state.update_task_lifecycle(
                    &record.id,
                    Lifecycle::Conflicted,
                    None,
                    None,
                );
                let _ = context.state.update_operation(
                    &operation.id,
                    OperationStatus::ManualIntervention,
                    &serde_json::json!({ "phase": "conflicted", "error": error.to_string() })
                        .to_string(),
                );
                Err(error)
            }
        }
    }

    fn sync_continuation(
        context: &Context,
        record: &TaskRecord,
        continue_rebase: bool,
        abort: bool,
        json: bool,
    ) -> Result<i32, AppError> {
        if !continue_rebase && !abort {
            return Err(AppError::diagnostic(
                "AGT-0730",
                "choose --continue or --abort",
                ErrorKind::Usage,
            ));
        }
        let command = if continue_rebase {
            "--continue"
        } else {
            "--abort"
        };
        let result = context.git.run(
            &record.path,
            InternalGitProfile::Sync,
            &args(&["rebase", command]),
        );
        match result {
            Ok(_) => {
                let head = context.git.text(
                    &record.path,
                    InternalGitProfile::Discovery,
                    &args2(&["rev-parse", "HEAD"]),
                )?;
                context.state.update_task_lifecycle(
                    &record.id,
                    Lifecycle::Active,
                    Some(&head),
                    None,
                )?;
                output(
                    json,
                    "sync",
                    None,
                    &serde_json::json!({ "task_id": record.id, "phase": command, "head_oid": head }),
                )
            }
            Err(error) => {
                context.state.update_task_lifecycle(
                    &record.id,
                    Lifecycle::Conflicted,
                    None,
                    None,
                )?;
                Err(error)
            }
        }
    }

    fn land(context: &Context, arguments: LandArgs, json: bool) -> Result<i32, AppError> {
        let record = context.state.task(&arguments.task)?;
        if arguments.into_current == arguments.onto.is_some() {
            return Err(AppError::diagnostic(
                "AGT-0731",
                "choose exactly one of --into-current or --onto",
                ErrorKind::Usage,
            ));
        }
        task::facts_match(&context.git, &record)?;
        task::ensure_review_clean(
            &context.git,
            &task_facts(&context.git, &record, &context.facts)?,
            &record.path,
        )?;
        let target = arguments.onto.unwrap_or_else(|| {
            current_branch(&context.git, &context.facts.root).unwrap_or_default()
        });
        let target_ref = BranchRef::local(&target)?.0;
        let target_oid = context.git.text(
            &context.facts.root,
            InternalGitProfile::Discovery,
            &args2(&["rev-parse", &target_ref]),
        )?;
        let task_oid = context.git.text(
            &record.path,
            InternalGitProfile::Discovery,
            &args2(&["rev-parse", "HEAD"]),
        )?;
        let operation = context.state.create_operation(OperationKind::Land, Some(&record.id), &serde_json::json!({ "task_ref": record.branch, "task_oid": task_oid, "target_ref": target_ref, "target_old_oid": target_oid }).to_string())?;
        let safety_task = format!(
            "refs/agentree/safety/land/task/{}/{}",
            record.id, operation.id
        );
        let safety_target = format!(
            "refs/agentree/safety/land/target/{}/{}",
            record.id, operation.id
        );
        context.git.run(
            &context.facts.root,
            InternalGitProfile::RepairReadOnly,
            &args2(&["update-ref", &safety_task, &task_oid]),
        )?;
        context.git.run(
            &context.facts.root,
            InternalGitProfile::RepairReadOnly,
            &args2(&["update-ref", &safety_target, &target_oid]),
        )?;
        if arguments.into_current {
            let current = repository::discover(&context.git, &std::env::current_dir()?)?;
            if current.branch.as_deref() != Some(target.trim_start_matches("refs/heads/"))
                || current.head != target_oid
            {
                return Err(AppError::diagnostic(
                    "AGT-0732",
                    "--into-current must run from the exact target worktree",
                    ErrorKind::StateInconsistent,
                ));
            }
            let target_state = task::content_state(&context.git, &current, &current.root)?;
            if !target_state.mutation_pristine() {
                return Err(AppError::diagnostic(
                    "AGT-0733",
                    "target worktree is not TargetPristine",
                    ErrorKind::DirtyWorktree,
                ));
            }
            context.git.run(
                &current.root,
                InternalGitProfile::Land,
                &args2(&["merge", "--no-autostash", "--ff-only", &task_oid]),
            )?;
            context
                .state
                .update_task_lifecycle(&record.id, Lifecycle::Landed, None, None)?;
            context.state.update_operation(
                &operation.id,
                OperationStatus::Completed,
                &serde_json::json!({ "phase": "target_updated", "target_oid": task_oid })
                    .to_string(),
            )?;
            return output(
                json,
                "land",
                Some(&operation.id),
                &serde_json::json!({ "task_id": record.id, "target": target_ref, "mode": "into-current", "target_oid": task_oid }),
            );
        }
        let target_claimed = worktree_claimed(&context.git, &context.facts.root, &target_ref)?;
        if target_claimed {
            return Err(AppError::diagnostic(
                "AGT-0734",
                "target branch is checked out; use --into-current from that worktree",
                ErrorKind::StateInconsistent,
            ));
        }
        let landing_path = PathBuf::from(&context.manifest.state_dir)
            .join("landing")
            .join(&operation.id);
        fs::create_dir_all(landing_path.parent().unwrap_or(Path::new(".")))?;
        context.git.run(
            &context.facts.root,
            InternalGitProfile::WorktreeManagement,
            &[
                OsString::from("worktree"),
                OsString::from("add"),
                landing_path.as_os_str().to_owned(),
                OsString::from(&target_ref),
            ],
        )?;
        let landing_facts = repository::discover(&context.git, &landing_path)?;
        if !task::content_state(&context.git, &landing_facts, &landing_path)?.mutation_pristine() {
            return Err(AppError::diagnostic(
                "AGT-0735",
                "temporary landing worktree is not TargetPristine",
                ErrorKind::DirtyWorktree,
            ));
        }
        context.git.run(
            &landing_path,
            InternalGitProfile::Land,
            &args2(&["merge", "--no-autostash", "--ff-only", &task_oid]),
        )?;
        let landed_head = context.git.text(
            &landing_path,
            InternalGitProfile::Discovery,
            &args2(&["rev-parse", "HEAD"]),
        )?;
        if landed_head != task_oid {
            return Err(AppError::diagnostic(
                "AGT-0736",
                "temporary landing worktree did not reach task OID",
                ErrorKind::RecoveryRequired,
            ));
        }
        let post_state = task::content_state(&context.git, &landing_facts, &landing_path)?;
        if !post_state.mutation_pristine() {
            return Err(AppError::diagnostic(
                "AGT-0737",
                "landing worktree has unexpected residue; cleanup is pending",
                ErrorKind::RecoveryRequired,
            ));
        }
        context.git.run(
            &context.facts.root,
            InternalGitProfile::WorktreeManagement,
            &[
                OsString::from("worktree"),
                OsString::from("remove"),
                landing_path.as_os_str().to_owned(),
            ],
        )?;
        context
            .state
            .update_task_lifecycle(&record.id, Lifecycle::Landed, None, None)?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Completed,
            &serde_json::json!({ "phase": "completed", "target_oid": task_oid }).to_string(),
        )?;
        output(
            json,
            "land",
            Some(&operation.id),
            &serde_json::json!({ "task_id": record.id, "target": target_ref, "mode": "temporary-worktree", "target_oid": task_oid }),
        )
    }

    fn dispatch_shim(raw_args: Vec<OsString>) -> Result<i32, AppError> {
        let raw = raw_args.into_iter().skip(1).collect::<Vec<_>>();
        let task_id = std::env::var("AGENTREE_TASK_ID").map_err(|_| {
            AppError::diagnostic(
                "AGT-0740",
                "Git shim requires an Agentree session",
                ErrorKind::SessionGuardDenied,
            )
        })?;
        if raw.iter().any(|arg| is_denied_global(arg)) {
            return Err(AppError::diagnostic(
                "AGT-0741",
                "Git repository override options are denied",
                ErrorKind::PolicyDenied,
            ));
        }
        let git = GitRunner::resolve()?;
        let start = std::env::current_dir()?;
        let facts = repository::discover(&git, &start)?;
        let manifest = repository::load_manifest(&facts)?;
        let state = State::open(
            Path::new(&manifest.state_dir)
                .join("state.sqlite3")
                .as_path(),
        )?;
        let record = state.task(&task_id)?;
        if std::env::var("AGENTREE_SESSION_ID").is_ok()
            && raw.first().is_some_and(|arg| {
                matches!(
                    arg.to_string_lossy().as_ref(),
                    "new" | "remove" | "fetch" | "sync" | "land" | "doctor" | "agentree"
                )
            })
        {
            return Err(AppError::diagnostic(
                "AGT-0742",
                "administrative Agentree operation is denied inside a task session",
                ErrorKind::SessionGuardDenied,
            ));
        }
        task::facts_match(&git, &record)?;
        if !policy_decision(&raw)? {
            return Err(AppError::diagnostic(
                "AGT-0743",
                "Git command denied by strict task policy",
                ErrorKind::PolicyDenied,
            ));
        }
        let output = git.run(&record.path, InternalGitProfile::Agent, &raw)?;
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        Ok(output.status.code().unwrap_or(1))
    }
}

impl Context {
    fn open(git: GitRunner, facts: RepositoryFacts) -> Result<Self, AppError> {
        let manifest = repository::load_manifest(&facts)?;
        let state = State::open(
            Path::new(&manifest.state_dir)
                .join("state.sqlite3")
                .as_path(),
        )?;
        Ok(Self {
            git,
            facts,
            manifest,
            state,
        })
    }
}

fn task_facts(
    git: &GitRunner,
    record: &TaskRecord,
    repository: &RepositoryFacts,
) -> Result<RepositoryFacts, AppError> {
    Ok(RepositoryFacts {
        root: record.path.clone(),
        common_dir: repository.common_dir.clone(),
        git_dir: resolve_git_dir(git, &record.path)?,
        index_path: resolve_git_path(git, &record.path, "index")?,
        branch: Some(record.branch.trim_start_matches("refs/heads/").to_owned()),
        head: record.head_oid.clone(),
        object_format: repository.object_format.clone(),
    })
}

fn resolve_git_dir(git: &GitRunner, root: &Path) -> Result<PathBuf, AppError> {
    let value = git.text(
        root,
        InternalGitProfile::Discovery,
        &args2(&["rev-parse", "--git-dir"]),
    )?;
    Ok(resolve_relative(root, &value))
}
fn resolve_git_path(git: &GitRunner, root: &Path, name: &str) -> Result<PathBuf, AppError> {
    let value = git.text(
        root,
        InternalGitProfile::Discovery,
        &args2(&["rev-parse", "--git-path", name]),
    )?;
    Ok(resolve_relative(root, &value))
}
fn resolve_relative(root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}
fn args2(values: &[&str]) -> Vec<OsString> {
    values.iter().map(|value| OsString::from(*value)).collect()
}

fn task_lock(manifest: &RepositoryManifest, task_id: &str) -> Result<FileLock, AppError> {
    FileLock::acquire(
        &PathBuf::from(&manifest.state_dir)
            .join("locks")
            .join(format!("task-{task_id}.lock")),
    )
}
fn repo_lock(manifest: &RepositoryManifest) -> Result<FileLock, AppError> {
    FileLock::acquire(&PathBuf::from(&manifest.state_dir).join("repository.lock"))
}

fn verify_marker(context: &Context, record: &TaskRecord) -> Result<(), AppError> {
    let marker = task::marker_path(&context.git, &record.path)?;
    let value: serde_json::Value = serde_json::from_slice(&fs::read(marker).map_err(|_| {
        AppError::diagnostic(
            "AGT-0744",
            "ownership marker is missing",
            ErrorKind::StateInconsistent,
        )
    })?)?;
    if value["repository_id"] != context.manifest.repository_id
        || value["task_id"] != record.id
        || value["branch"] != record.branch
    {
        return Err(AppError::diagnostic(
            "AGT-0745",
            "ownership marker does not match task facts",
            ErrorKind::StateInconsistent,
        ));
    }
    Ok(())
}

fn worktree_claimed(git: &GitRunner, root: &Path, target_ref: &str) -> Result<bool, AppError> {
    let output = git.text(
        root,
        InternalGitProfile::Discovery,
        &args2(&["worktree", "list", "--porcelain"]),
    )?;
    Ok(output
        .lines()
        .any(|line| line == format!("branch {target_ref}")))
}

fn current_branch(git: &GitRunner, root: &Path) -> Result<String, AppError> {
    let branch = git.text(
        root,
        InternalGitProfile::Discovery,
        &args2(&["symbolic-ref", "--short", "HEAD"]),
    )?;
    Ok(branch.trim_start_matches("refs/heads/").to_owned())
}

fn sha256_file(path: &Path) -> Result<String, AppError> {
    let bytes = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn create_link_or_copy(source: &Path, target: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)?;
    }
    #[cfg(not(unix))]
    {
        fs::copy(source, target)?;
    }
    Ok(())
}

fn sanitize_child_environment(command: &mut Command) {
    const KEYS: &[&str] = &[
        "AGENTREE_REAL_GIT",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_EXTERNAL_DIFF",
    ];
    for key in KEYS {
        command.env_remove(key);
    }
}

fn deny_inside_session() -> Result<(), AppError> {
    if std::env::var_os("AGENTREE_SESSION_ID").is_some() {
        return Err(AppError::diagnostic(
            "AGT-0746",
            "this administrative command is denied inside a task session",
            ErrorKind::SessionGuardDenied,
        ));
    }
    Ok(())
}

fn is_denied_global(arg: &OsString) -> bool {
    let value = arg.to_string_lossy();
    matches!(
        value.as_ref(),
        "-C" | "-c"
            | "--config-env"
            | "--git-dir"
            | "--work-tree"
            | "--namespace"
            | "--bare"
            | "--exec-path"
    ) || value.starts_with("--git-dir=")
        || value.starts_with("--work-tree=")
        || value.starts_with("-c")
}

fn policy_decision(arguments: &[OsString]) -> Result<bool, AppError> {
    let Some(command) = arguments
        .iter()
        .find(|argument| !argument.to_string_lossy().starts_with('-'))
        .map(|argument| argument.to_string_lossy().to_string())
    else {
        return Ok(false);
    };
    let readonly = [
        "status",
        "diff",
        "log",
        "show",
        "grep",
        "blame",
        "rev-parse",
        "merge-base",
        "ls-files",
        "ls-tree",
        "cat-file",
    ];
    let local_write = ["add", "rm", "mv"];
    if readonly.contains(&command.as_str()) {
        return Ok(!arguments.iter().any(|argument| {
            matches!(
                argument.to_string_lossy().as_ref(),
                "--ext-diff" | "--textconv"
            )
        }));
    }
    if local_write.contains(&command.as_str()) {
        return Ok(true);
    }
    if command == "commit" {
        let mut message_value = false;
        for argument in arguments.iter().skip(1) {
            let value = argument.to_string_lossy();
            if message_value {
                message_value = false;
                continue;
            }
            if matches!(value.as_ref(), "-m" | "--message") {
                message_value = true;
                continue;
            }
            if matches!(
                value.as_ref(),
                "-a" | "--all" | "--amend" | "--only" | "--include" | "--no-verify"
            ) {
                return Ok(false);
            }
            if !value.starts_with('-') {
                return Ok(false);
            }
        }
        return Ok(!message_value);
    }
    Ok(false)
}

fn output<T: Serialize>(
    json: bool,
    command: &str,
    operation_id: Option<&str>,
    result: &T,
) -> Result<i32, AppError> {
    if json {
        let envelope = JsonEnvelope {
            schema_version: 1,
            ok: true,
            command: command.to_owned(),
            operation_id: operation_id.map(str::to_owned),
            result: Some(result),
            error: None,
        };
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        println!("{}", serde_json::to_string_pretty(result)?);
    }
    Ok(0)
}

fn print_json<T: Serialize>(command: &str, result: &T) {
    let envelope = JsonEnvelope {
        schema_version: 1,
        ok: true,
        command: command.to_owned(),
        operation_id: None,
        result: Some(result),
        error: None,
    };
    if let Ok(value) = serde_json::to_string_pretty(&envelope) {
        println!("{value}");
    }
}

fn clap_to_error(error: clap::Error) -> AppError {
    let kind = if error.kind() == ClapErrorKind::DisplayHelp
        || error.kind() == ClapErrorKind::DisplayVersion
    {
        ErrorKind::Usage
    } else {
        ErrorKind::Usage
    };
    AppError::diagnostic("AGT-0700", error.to_string(), kind)
}
