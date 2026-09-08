use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{
    cli::{
        self, CheckpointCommand, Command as CliCommand, ConfigCommand, LandArgs, NewArgs,
        OverlapArgs, RunArgs, SyncArgs,
    },
    config,
    domain::{
        sha256_hex, AppError, BranchRef, ErrorKind, InternalGitProfile, JsonEnvelope, Lifecycle,
        OperationKind, OperationStatus,
    },
    git::{args, GitRunner},
    lock::FileLock,
    repository::{self, RepositoryFacts, RepositoryManifest},
    state::{CheckRunRecord, CheckpointRecord, SessionRecord, State, TaskRecord},
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
        let json_requested = raw_args.iter().any(|arg| arg == "--json");
        let cli = match cli::parse_args(raw_args) {
            Ok(cli) => cli,
            Err(error) => {
                let error = clap_to_error(error);
                if json_requested {
                    print_error_json("unknown", &error);
                }
                return Err(error);
            }
        };
        let json = cli.json;
        let command = command_name(&cli.command);
        let result = Self::dispatch_cli(cli);
        if let Err(error) = &result {
            if json {
                print_error_json(command, error);
            }
        }
        result
    }

    fn dispatch_cli(cli: cli::Cli) -> Result<i32, AppError> {
        let git = GitRunner::resolve()?;
        let start = cli
            .repository
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        let facts = repository::discover(&git, &start)?;
        enforce_session_guard(&git, &facts, &cli.command)?;
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
                    program: vec![
                        std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"))
                    ],
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
                session,
                plan,
                apply,
                plan_fingerprint,
            } => Self::doctor(
                &context,
                operation.as_deref(),
                session.as_deref(),
                plan,
                apply,
                plan_fingerprint.as_deref(),
                json,
            ),
            CliCommand::Checkpoint { command } => Self::checkpoint_command(&context, command, json),
            CliCommand::Overlap(arguments) => Self::overlap(&context, arguments, json),
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
                        program: vec![
                            std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"))
                        ],
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
        task::validate_scopes(&arguments.scopes)?;
        let task_id = crate::domain::Id::new("task-").0;
        let branch = task::branch_for(&arguments.slug, &task_id)?;
        let path = task::path_for(
            Path::new(&context.manifest.worktree_root),
            &arguments.slug,
            &task_id,
        )?;
        if context
            .git
            .run(
                &context.facts.root,
                InternalGitProfile::Discovery,
                &args2(&["show-ref", "--verify", &branch]),
            )
            .is_ok()
        {
            return Err(AppError::diagnostic(
                "AGT-0514",
                "generated task branch already exists; refusing to overwrite it",
                ErrorKind::StateInconsistent,
            ));
        }
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
            &args2(&["update-ref", &branch, &base_oid]),
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
                OsString::from(branch.trim_start_matches("refs/heads/")),
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
        failpoint("task_create.after_worktree_add");
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
            if !record.path.exists() {
                result.push(serde_json::json!({
                    "task_id": record.id,
                    "slug": record.slug,
                    "state": record.lifecycle.as_str(),
                    "branch": record.branch,
                    "head": record.head_oid,
                    "recorded_head": record.head_oid,
                    "drifted": false,
                    "worktree": record.path,
                    "worktree_exists": false,
                    "dirty": serde_json::Value::Null,
                    "mutation_pristine": serde_json::Value::Null,
                    "ready": false,
                    "session": context.state.sessions_for_task(&record.id)?,
                }));
                continue;
            }
            let facts = task_facts(&context.git, record, &context.facts)?;
            let content = task::content_state(&context.git, &facts, &record.path)?;
            let observed_head = context.git.text(
                &record.path,
                InternalGitProfile::Discovery,
                &args2(&["rev-parse", "HEAD"]),
            )?;
            let config = context.state.config(&record.id)?;
            let definition_hash = hash_json(&config.checks)?;
            let required_count = config.checks.iter().filter(|check| check.required).count();
            let ready = record.lifecycle == Lifecycle::Active
                && content.review_clean()
                && (required_count == 0
                    || context.state.fresh_required_checks(
                        &record.id,
                        &observed_head,
                        &record.config_hash,
                        &definition_hash,
                        required_count,
                    )?);
            result.push(serde_json::json!({ "task_id": record.id, "slug": record.slug, "state": record.lifecycle.as_str(), "branch": record.branch, "head": observed_head, "recorded_head": record.head_oid, "drifted": observed_head != record.head_oid, "worktree": record.path, "worktree_exists": true, "dirty": !content.review_clean(), "mutation_pristine": content.mutation_pristine(), "ready": ready, "session": context.state.sessions_for_task(&record.id)? }));
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
        let observed_head = context.git.text(
            &record.path,
            InternalGitProfile::Discovery,
            &args2(&["rev-parse", "HEAD"]),
        )?;
        let definition_hash = hash_json(&checks)?;
        let required_count = checks.iter().filter(|check| check.required).count();
        let fresh = required_count == 0
            || context.state.fresh_required_checks(
                &record.id,
                &observed_head,
                &record.config_hash,
                &definition_hash,
                required_count,
            )?;
        output(
            json,
            "context",
            None,
            &serde_json::json!({ "task_id": record.id, "slug": record.slug, "branch": record.branch, "worktree": record.path, "base_oid": record.base_oid, "head_oid": observed_head, "recorded_head": record.head_oid, "config_hash": record.config_hash, "scopes": record.scopes, "content": content, "checks": checks, "readiness": { "ready": record.lifecycle == Lifecycle::Active && content.review_clean() && fresh, "required_checks_fresh": fresh }, "warnings": ["Git shim is a guardrail, not an OS security boundary", "path overlap is heuristic and not semantic conflict detection"] }),
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
        let mut record = context.state.task(&arguments.task)?;
        if record.lifecycle != Lifecycle::Active {
            return Err(AppError::diagnostic(
                "AGT-0703",
                "task is not active",
                ErrorKind::StateInconsistent,
            ));
        }
        let _task_lock = task_lock(&context.manifest, &record.id)?;
        if context.state.sessions_for_task(&record.id)? > 0 {
            return Err(AppError::diagnostic(
                "AGT-0704",
                "a session is already active for this task",
                ErrorKind::LockConflict,
            ));
        }
        if context
            .state
            .has_incomplete_operation_for_task(&record.id)?
        {
            return Err(AppError::diagnostic(
                "AGT-0782",
                "incomplete operation prevents removal",
                ErrorKind::RecoveryRequired,
            ));
        }
        refresh_head(context, &mut record)?;
        task::facts_match(&context.git, &record)?;
        let supervisor_pid = std::process::id();
        let supervisor_birth_id = process_birth_identity(supervisor_pid);
        let session_id =
            context
                .state
                .start_session(&record.id, supervisor_pid, &supervisor_birth_id)?;
        let mut child = match spawn_session_child(context, &record, &arguments, &session_id) {
            Ok(child) => child,
            Err(error) => {
                let _ = context.state.finish_session(&session_id, "failed", None);
                return Err(error);
            }
        };
        let pid = child.id();
        #[cfg(unix)]
        {
            let _ = unsafe { libc::setpgid(pid as i32, pid as i32) };
        }
        let pgid = pid;
        if let Err(error) = context.state.update_session_identity(
            &session_id,
            pid,
            pgid,
            &process_birth_identity(pid),
        ) {
            terminate_child(&mut child);
            let _ = child.wait();
            let _ = context.state.finish_session(&session_id, "failed", None);
            return Err(error);
        }
        let status = match child.wait() {
            Ok(status) => status,
            Err(error) => {
                let _ = context.state.finish_session(&session_id, "failed", None);
                return Err(error.into());
            }
        };
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
        drop(_task_lock);
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
        let checkpoint_result = Self::checkpoint_task(context, &post_run_record, None);
        let checkpoint = if checkpoint_result.is_ok() {
            "created"
        } else {
            "not-created"
        };
        let mut post_checks = serde_json::Value::Null;
        let mut post_processing_error = checkpoint_result.err().map(|error| error.to_string());
        if arguments.require_post_checks {
            match Self::execute_checks(context, &record.id) {
                Ok((results, required_failure)) => {
                    post_checks = serde_json::json!({
                        "required_failure": required_failure,
                        "results": results,
                    });
                    if required_failure {
                        post_processing_error =
                            Some("a required post-run check failed or became stale".to_owned());
                    }
                }
                Err(error) => {
                    post_processing_error = Some(error.to_string());
                    post_checks = serde_json::json!({ "error": error.to_string() });
                }
            }
        }
        let result = serde_json::json!({
            "session_id": session_id,
            "child_exit_code": exit_code,
            "post_run_checkpoint": checkpoint,
            "post_checks": post_checks,
        });
        if arguments.require_post_checks {
            if let Some(error) = post_processing_error {
                return Err(AppError::diagnostic(
                    "AGT-0750",
                    error,
                    ErrorKind::StateInconsistent,
                ));
            }
        }
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
        let mut record = context.state.task(selector)?;
        refresh_head(context, &mut record)?;
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
        let mut record = context.state.task(selector)?;
        if context.state.sessions_for_task(&record.id)? > 0 {
            return Err(AppError::diagnostic(
                "AGT-0711",
                "active session prevents removal",
                ErrorKind::LockConflict,
            ));
        }
        if context
            .state
            .has_incomplete_operation_for_task(&record.id)?
        {
            return Err(AppError::diagnostic(
                "AGT-0782",
                "incomplete operation prevents removal",
                ErrorKind::RecoveryRequired,
            ));
        }
        let managed_root = Path::new(&context.manifest.worktree_root).canonicalize()?;
        let candidate = record.path.canonicalize()?;
        if !candidate.starts_with(&managed_root) {
            return Err(AppError::diagnostic(
                "AGT-0712",
                "worktree is outside the managed root",
                ErrorKind::StateInconsistent,
            ));
        }
        refresh_head(context, &mut record)?;
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
        if context
            .state
            .has_incomplete_operation_for_task(&record.id)?
        {
            return Err(AppError::diagnostic(
                "AGT-0783",
                "incomplete operation prevents archive",
                ErrorKind::RecoveryRequired,
            ));
        }
        let _task_lock = task_lock(&context.manifest, &record.id)?;
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
        if !matches!(record.lifecycle, Lifecycle::Archived | Lifecycle::Landed) {
            return Err(AppError::diagnostic(
                "AGT-0759",
                "branch deletion requires an archived or landed task",
                ErrorKind::StateInconsistent,
            ));
        }
        if context.state.sessions_for_task(&record.id)? > 0 {
            return Err(AppError::diagnostic(
                "AGT-0760",
                "active session prevents branch deletion",
                ErrorKind::LockConflict,
            ));
        }
        if context
            .state
            .has_incomplete_operation_for_task(&record.id)?
        {
            return Err(AppError::diagnostic(
                "AGT-0761",
                "incomplete operation prevents branch deletion",
                ErrorKind::RecoveryRequired,
            ));
        }
        if record.path.exists() {
            return Err(AppError::diagnostic(
                "AGT-0717",
                "remove the managed worktree before deleting its branch",
                ErrorKind::StateInconsistent,
            ));
        }
        let _task_lock = task_lock(&context.manifest, &record.id)?;
        let _repo_lock = repo_lock(&context.manifest)?;
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
        session: Option<&str>,
        plan: bool,
        apply: bool,
        plan_fingerprint: Option<&str>,
        json: bool,
    ) -> Result<i32, AppError> {
        if operation.is_some() && session.is_some() {
            return Err(AppError::diagnostic(
                "AGT-0756",
                "doctor accepts either --operation or --session, not both",
                ErrorKind::Usage,
            ));
        }
        if plan && operation.is_none() && session.is_none() {
            return Err(AppError::diagnostic(
                "AGT-0755",
                "doctor --plan requires --operation or --session",
                ErrorKind::Usage,
            ));
        }
        if apply {
            if let Some(session_id) = session {
                let fingerprint = plan_fingerprint.ok_or_else(|| {
                    AppError::diagnostic(
                        "AGT-0722",
                        "doctor --apply requires --plan-fingerprint",
                        ErrorKind::Usage,
                    )
                })?;
                return Self::apply_session_plan(context, session_id, fingerprint, json);
            }
            let id = operation.ok_or_else(|| {
                AppError::diagnostic(
                    "AGT-0719",
                    "doctor --apply requires --operation",
                    ErrorKind::Usage,
                )
            })?;
            let fingerprint = plan_fingerprint.ok_or_else(|| {
                AppError::diagnostic(
                    "AGT-0722",
                    "doctor --apply requires --plan-fingerprint",
                    ErrorKind::Usage,
                )
            })?;
            return Self::apply_doctor_plan(context, id, fingerprint, json);
        }
        let operations = context.state.incomplete_operations()?;
        let selected = operations.into_iter().filter(|item| operation.map(|id| id == item.id).unwrap_or(true)).map(|item| {
            let fingerprint = doctor_fingerprint(&item);
            serde_json::json!({ "operation_id": item.id, "kind": item.kind, "status": item.status, "expected": item.expected, "observed": item.observed, "plan_fingerprint": fingerprint, "action": "inspect Git facts; no automatic rollback or deletion" })
        }).collect::<Vec<_>>();
        let sessions = context
            .state
            .active_sessions()?
            .into_iter()
            .filter(|item| session.map(|id| id == item.id).unwrap_or(true))
            .map(|item| session_plan(&item))
            .collect::<Result<Vec<_>, _>>()?;
        output(
            json,
            "doctor",
            None,
            &serde_json::json!({ "read_only": true, "operations": selected, "sessions": sessions }),
        )
    }

    fn apply_session_plan(
        context: &Context,
        id: &str,
        fingerprint: &str,
        json: bool,
    ) -> Result<i32, AppError> {
        let session = context.state.session(id)?;
        let _task_lock = task_lock(&context.manifest, &session.task_id)?;
        let plan = session_plan(&session)?;
        if plan["plan_fingerprint"].as_str() != Some(fingerprint) {
            return Err(AppError::diagnostic(
                "AGT-0723",
                "doctor session plan fingerprint is stale",
                ErrorKind::RecoveryRequired,
            ));
        }
        if plan["state"] != "orphaned" {
            return Err(AppError::diagnostic(
                "AGT-0761",
                "session is not a verified orphan; no process will be signaled",
                ErrorKind::RecoveryRequired,
            ));
        }
        let terminated = terminate_orphaned_session(&session);
        context.state.finish_session(id, "orphaned", None)?;
        output(
            json,
            "doctor",
            None,
            &serde_json::json!({
                "applied": true,
                "session_id": id,
                "state": "orphaned",
                "child_signal_attempted": terminated,
            }),
        )
    }

    fn apply_doctor_plan(
        context: &Context,
        id: &str,
        fingerprint: &str,
        json: bool,
    ) -> Result<i32, AppError> {
        let operation = context.state.operation(id)?;
        let Some(task_id) = operation.task_id.clone() else {
            return Err(AppError::diagnostic(
                "AGT-0724",
                "operation has no task scope; manual intervention required",
                ErrorKind::RecoveryRequired,
            ));
        };
        let _task_lock = task_lock(&context.manifest, &task_id)?;
        let operation = context.state.operation(id)?;
        if doctor_fingerprint(&operation) != fingerprint {
            return Err(AppError::diagnostic(
                "AGT-0723",
                "doctor plan fingerprint is stale",
                ErrorKind::RecoveryRequired,
            ));
        }
        let expected: serde_json::Value = serde_json::from_str(&operation.expected)?;
        let record = context.state.task(&task_id)?;
        match operation.kind.as_str() {
            "checkpoint" => {
                let observed: serde_json::Value = serde_json::from_str(&operation.observed)?;
                let field = |name: &str| {
                    observed[name].as_str().ok_or_else(|| {
                        AppError::diagnostic(
                            "AGT-0762",
                            format!("checkpoint recovery lacks {name}"),
                            ErrorKind::RecoveryRequired,
                        )
                    })
                };
                let checkpoint_id = field("checkpoint_id")?;
                let metadata_oid = field("metadata_oid")?;
                let immutable = field("immutable_ref")?;
                let latest = observed["latest_ref"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!(
                            "refs/agentree/checkpoints/{}/{}/latest",
                            context.manifest.repository_id, task_id
                        )
                    });
                let immutable_oid = context.git.text(
                    &context.facts.root,
                    InternalGitProfile::Discovery,
                    &args2(&["rev-parse", "--verify", immutable]),
                )?;
                if immutable_oid != metadata_oid {
                    return Err(AppError::diagnostic(
                        "AGT-0763",
                        "checkpoint immutable ref does not match the recorded metadata OID",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                match context.git.text(
                    &context.facts.root,
                    InternalGitProfile::Discovery,
                    &args2(&["rev-parse", "--verify", &latest]),
                ) {
                    Ok(latest_oid) if latest_oid != metadata_oid => {
                        return Err(AppError::diagnostic(
                            "AGT-0763",
                            "checkpoint latest ref does not match the recorded metadata OID",
                            ErrorKind::RecoveryRequired,
                        ));
                    }
                    Ok(_) => {}
                    Err(_) => {
                        context.git.run(
                            &context.facts.root,
                            InternalGitProfile::RepairReadOnly,
                            &args(&["update-ref", &latest, metadata_oid]),
                        )?;
                    }
                }
                if !context
                    .state
                    .checkpoints(&task_id)?
                    .iter()
                    .any(|checkpoint| checkpoint.id == checkpoint_id)
                {
                    context.state.save_checkpoint(&CheckpointRecord {
                        id: checkpoint_id.to_owned(),
                        task_id: task_id.to_owned(),
                        head_oid: field("head_oid")?.to_owned(),
                        index_tree_oid: field("index_tree_oid")?.to_owned(),
                        worktree_tree_oid: field("worktree_tree_oid")?.to_owned(),
                        metadata_oid: metadata_oid.to_owned(),
                        message: None,
                        config_hash: field("config_hash")?.to_owned(),
                    })?;
                }
                context.state.update_operation(
                    id,
                    OperationStatus::Completed,
                    &serde_json::json!({
                        "phase": "reconciled",
                        "checkpoint_id": checkpoint_id,
                        "metadata_oid": metadata_oid,
                        "latest_ref": latest,
                    })
                    .to_string(),
                )?;
            }
            "land" => {
                let _repo_lock = repo_lock(&context.manifest)?;
                let target_ref = expected["target_ref"].as_str().ok_or_else(|| {
                    AppError::diagnostic(
                        "AGT-0725",
                        "land operation lacks target ref",
                        ErrorKind::RecoveryRequired,
                    )
                })?;
                let task_oid = expected["task_oid"].as_str().ok_or_else(|| {
                    AppError::diagnostic(
                        "AGT-0726",
                        "land operation lacks task oid",
                        ErrorKind::RecoveryRequired,
                    )
                })?;
                let observed = context.git.text(
                    &context.facts.root,
                    InternalGitProfile::Discovery,
                    &args2(&["rev-parse", target_ref]),
                )?;
                if observed != task_oid {
                    return Err(AppError::diagnostic(
                        "AGT-0727",
                        "target is not at the recorded successful task OID; rollback is forbidden",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                let observed_state: serde_json::Value =
                    serde_json::from_str(&operation.observed).unwrap_or_default();
                if let Some(landing_path) = observed_state["landing_path"].as_str() {
                    let landing_path = PathBuf::from(landing_path);
                    let landing_root = PathBuf::from(&context.manifest.state_dir).join("landing");
                    let canonical_landing = landing_path.canonicalize().map_err(|_| {
                        AppError::diagnostic(
                            "AGT-0772",
                            "land recovery worktree path cannot be verified",
                            ErrorKind::RecoveryRequired,
                        )
                    })?;
                    if !canonical_landing.starts_with(&landing_root) {
                        return Err(AppError::diagnostic(
                            "AGT-0773",
                            "land recovery path is outside the managed landing root",
                            ErrorKind::RecoveryRequired,
                        ));
                    }
                    let landing_facts = repository::discover(&context.git, &landing_path)?;
                    if !task::content_state(&context.git, &landing_facts, &landing_path)?
                        .mutation_pristine()
                    {
                        let _ = context.state.update_operation(
                            id,
                            OperationStatus::CleanupPending,
                            "{\"phase\":\"cleanup_pending\",\"reason\":\"landing_worktree_dirty\"}",
                        );
                        return Err(AppError::diagnostic(
                            "AGT-0774",
                            "land target updated but temporary landing worktree is dirty",
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
                    if landing_path.exists() {
                        return Err(AppError::diagnostic(
                            "AGT-0775",
                            "temporary landing worktree cleanup did not complete",
                            ErrorKind::RecoveryRequired,
                        ));
                    }
                }
                context
                    .state
                    .update_task_lifecycle(&task_id, Lifecycle::Landed, None, None)?;
                context.state.update_operation(id, OperationStatus::Completed, &serde_json::json!({ "phase": "finalized_after_target_update", "target_oid": observed }).to_string())?;
            }
            "remove_worktree" => {
                if record.path.exists() {
                    return Err(AppError::diagnostic(
                        "AGT-0728",
                        "owned worktree still exists; doctor will not delete it",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                context
                    .state
                    .update_task_lifecycle(&task_id, Lifecycle::Archived, None, None)?;
                context.state.update_operation(
                    id,
                    OperationStatus::Completed,
                    "{\"phase\":\"finalized_after_worktree_absence\"}",
                )?;
            }
            "create_task" => {
                if !record.path.exists() {
                    return Err(AppError::diagnostic(
                        "AGT-0778",
                        "created worktree is missing; recovery will not recreate it implicitly",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                let expected_path = expected["path"].as_str().ok_or_else(|| {
                    AppError::diagnostic(
                        "AGT-0779",
                        "create operation lacks path",
                        ErrorKind::RecoveryRequired,
                    )
                })?;
                if record.path.to_str() != Some(expected_path) {
                    return Err(AppError::diagnostic(
                        "AGT-0780",
                        "created worktree path does not match the journal",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                task::facts_match(&context.git, &record)?;
                let facts = task_facts(&context.git, &record, &context.facts)?;
                task::ensure_mutation_pristine(&context.git, &facts, &record.path)?;
                let marker = task::marker_path(&context.git, &record.path)?;
                if marker.exists() {
                    verify_marker(context, &record)?;
                } else {
                    repository::durable_replace(
                        &marker,
                        &serde_json::to_vec(&serde_json::json!({
                            "schema_version": 1,
                            "repository_id": context.manifest.repository_id,
                            "task_id": record.id.clone(),
                            "branch": record.branch.clone(),
                            "worktree": record.path.clone(),
                        }))?,
                    )?;
                }
                let config = match context.state.config(&record.id) {
                    Ok(config) => config,
                    Err(_) => {
                        let config =
                            config::snapshot(&context.git, &context.facts.root, &record.base_oid)?;
                        context.state.save_config(&record.id, &config)?;
                        config
                    }
                };
                if config.hash != record.config_hash {
                    return Err(AppError::diagnostic(
                        "AGT-0781",
                        "created task config snapshot does not match the journal",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                context.state.update_task_lifecycle(
                    &record.id,
                    Lifecycle::Active,
                    Some(&record.head_oid),
                    None,
                )?;
                context.state.update_operation(
                    id,
                    OperationStatus::Completed,
                    "{\"phase\":\"finalized_after_fact_match\"}",
                )?;
            }
            "restore_checkpoint" => {
                let expected: serde_json::Value = serde_json::from_str(&operation.expected)?;
                if !record.path.exists() {
                    return Err(AppError::diagnostic(
                        "AGT-0764",
                        "restored worktree is missing; recovery will not recreate it implicitly",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                let expected_branch = expected["branch"].as_str().ok_or_else(|| {
                    AppError::diagnostic(
                        "AGT-0765",
                        "restore operation lacks branch",
                        ErrorKind::RecoveryRequired,
                    )
                })?;
                if record.branch != expected_branch {
                    return Err(AppError::diagnostic(
                        "AGT-0766",
                        "restore branch does not match the journal",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                task::facts_match(&context.git, &record)?;
                let facts = task_facts(&context.git, &record, &context.facts)?;
                let restored_state = task::content_state(&context.git, &facts, &record.path)?;
                if !restored_state.nonignored_residue.is_empty()
                    || !restored_state.ignored_residue.is_empty()
                    || !restored_state.visibility_flags.is_empty()
                    || restored_state.in_progress
                {
                    return Err(AppError::diagnostic(
                        "AGT-0767",
                        "restored worktree facts are not safe to reconcile",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                let marker = task::marker_path(&context.git, &record.path)?;
                if marker.exists() {
                    verify_marker(context, &record)?;
                } else {
                    repository::durable_replace(
                        &marker,
                        &serde_json::to_vec(&serde_json::json!({
                            "schema_version": 1,
                            "repository_id": context.manifest.repository_id,
                            "task_id": record.id.clone(),
                            "branch": record.branch.clone(),
                            "worktree": record.path.clone(),
                            "restored_from": expected["restore_from"].clone(),
                        }))?,
                    )?;
                }
                let config = match context.state.config(&task_id) {
                    Ok(config) => config,
                    Err(_) => {
                        let restore_from = expected["restore_from"].as_str().ok_or_else(|| {
                            AppError::diagnostic(
                                "AGT-0776",
                                "restore operation lacks source checkpoint",
                                ErrorKind::RecoveryRequired,
                            )
                        })?;
                        let source_task_id = context
                            .state
                            .tasks()?
                            .into_iter()
                            .find(|task| {
                                context
                                    .state
                                    .checkpoints(&task.id)
                                    .map(|checkpoints| {
                                        checkpoints
                                            .iter()
                                            .any(|checkpoint| checkpoint.id == restore_from)
                                    })
                                    .unwrap_or(false)
                            })
                            .map(|task| task.id)
                            .ok_or_else(|| {
                                AppError::diagnostic(
                                    "AGT-0777",
                                    "restore source task cannot be identified",
                                    ErrorKind::RecoveryRequired,
                                )
                            })?;
                        let source_config = context.state.config(&source_task_id)?;
                        context.state.save_config(&task_id, &source_config)?;
                        source_config
                    }
                };
                if config.hash != expected["config_hash"].as_str().unwrap_or_default() {
                    return Err(AppError::diagnostic(
                        "AGT-0768",
                        "restored config snapshot does not match the journal",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                context.state.update_task_lifecycle(
                    &task_id,
                    Lifecycle::Active,
                    Some(&record.head_oid),
                    None,
                )?;
                context.state.update_operation(
                    id,
                    OperationStatus::Completed,
                    "{\"phase\":\"reconciled\"}",
                )?;
            }
            "archive" => {
                if record.lifecycle != Lifecycle::Archived {
                    return Err(AppError::diagnostic(
                        "AGT-0729",
                        "archive operation is not reflected in state",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                context.state.update_operation(
                    id,
                    OperationStatus::Completed,
                    "{\"phase\":\"finalized\"}",
                )?;
            }
            "delete_branch" => {
                let _repo_lock = repo_lock(&context.manifest)?;
                let branch = expected["branch"].as_str().ok_or_else(|| {
                    AppError::diagnostic(
                        "AGT-0730",
                        "delete operation lacks branch",
                        ErrorKind::RecoveryRequired,
                    )
                })?;
                if context
                    .git
                    .run(
                        &context.facts.root,
                        InternalGitProfile::Discovery,
                        &args2(&["show-ref", "--verify", branch]),
                    )
                    .is_ok()
                {
                    return Err(AppError::diagnostic(
                        "AGT-0731",
                        "owned branch still exists; doctor will not delete it",
                        ErrorKind::RecoveryRequired,
                    ));
                }
                context.state.update_operation(
                    id,
                    OperationStatus::Completed,
                    "{\"phase\":\"finalized_after_ref_absence\"}",
                )?;
            }
            _ => {
                return Err(AppError::diagnostic(
                    "AGT-0732",
                    "operation requires manual intervention",
                    ErrorKind::RecoveryRequired,
                ))
            }
        }
        output(
            json,
            "doctor",
            Some(id),
            &serde_json::json!({ "applied": true, "operation_id": id, "rollback": false }),
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
            CheckpointCommand::Legacy(arguments) => {
                let selector = arguments.first().ok_or_else(|| {
                    AppError::diagnostic(
                        "AGT-0756",
                        "checkpoint requires a task selector",
                        ErrorKind::Usage,
                    )
                })?;
                let mut message = None;
                let mut index = 1;
                while index < arguments.len() {
                    let argument = arguments[index].to_string_lossy();
                    if argument == "-m" || argument == "--message" {
                        index += 1;
                        let value = arguments.get(index).ok_or_else(|| {
                            AppError::diagnostic(
                                "AGT-0757",
                                "checkpoint message is missing",
                                ErrorKind::Usage,
                            )
                        })?;
                        message = Some(value.to_string_lossy().into_owned());
                    } else {
                        return Err(AppError::diagnostic(
                            "AGT-0758",
                            format!("unknown checkpoint argument: {argument}"),
                            ErrorKind::Usage,
                        ));
                    }
                    index += 1;
                }
                let record = context.state.task(&selector.to_string_lossy())?;
                let checkpoint = Self::checkpoint_task(context, &record, message.as_deref())?;
                output(
                    json,
                    "checkpoint",
                    Some(&checkpoint.id),
                    &serde_json::json!(checkpoint),
                )
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
        ensure_index_unlocked(&facts.index_path)?;
        let state = task::content_state(&context.git, &facts, &record.path)?;
        if state.in_progress || !state.visibility_flags.is_empty() {
            return Err(AppError::diagnostic(
                "AGT-0721",
                "checkpoint does not support in-progress or visibility-suppressing index state",
                ErrorKind::Unsupported,
            ));
        }
        let _task_lock = task_lock(&context.manifest, &record.id)?;
        let operation = context.state.create_operation(
            OperationKind::Checkpoint,
            Some(&record.id),
            &serde_json::json!({ "head": record.head_oid, "index": facts.index_path }).to_string(),
        )?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            "{\"phase\":\"capture_started\"}",
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
        ensure_index_unlocked(&facts.index_path)?;
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
        let latest = format!(
            "refs/agentree/checkpoints/{}/{}/latest",
            context.manifest.repository_id, record.id
        );
        let mut observation = serde_json::json!({
            "phase": "objects_created",
            "checkpoint_id": checkpoint_id.clone(),
            "task_id": record.id.clone(),
            "head_oid": record.head_oid.clone(),
            "index_tree_oid": index_tree.clone(),
            "worktree_tree_oid": tree_one.clone(),
            "metadata_oid": metadata_oid.clone(),
            "config_hash": record.config_hash.clone(),
            "immutable_ref": immutable.clone(),
            "latest_ref": latest.clone(),
        });
        context.state.update_operation(
            &operation.id,
            OperationStatus::Verifying,
            &observation.to_string(),
        )?;
        context.git.run(
            &facts.root,
            InternalGitProfile::RepairReadOnly,
            &args(&["update-ref", &immutable, &metadata_oid]),
        )?;
        failpoint("checkpoint.after_anchor_ref");
        observation["phase"] = serde_json::Value::String("immutable_ref_published".to_owned());
        observation["immutable_ref"] = serde_json::Value::String(immutable.clone());
        context.state.update_operation(
            &operation.id,
            OperationStatus::Verifying,
            &observation.to_string(),
        )?;
        context.git.run(
            &facts.root,
            InternalGitProfile::RepairReadOnly,
            &args(&["update-ref", &latest, &metadata_oid]),
        )?;
        observation["phase"] = serde_json::Value::String("latest_ref_published".to_owned());
        observation["latest_ref"] = serde_json::Value::String(latest.clone());
        context.state.update_operation(
            &operation.id,
            OperationStatus::Verifying,
            &observation.to_string(),
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
        let operation = context.state.create_operation(
            OperationKind::RestoreCheckpoint,
            Some(&task_id),
            &serde_json::json!({
                "restore_from": checkpoint_id,
                "base_oid": source.head_oid,
                "branch": branch,
                "path": path,
                "index_tree_oid": source.index_tree_oid,
                "worktree_tree_oid": source.worktree_tree_oid,
                "config_hash": source_task.config_hash,
            })
            .to_string(),
        )?;
        let _task_lock = task_lock(&context.manifest, &task_id)?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            &serde_json::json!({ "phase": "branch_creation_started", "restore_from": checkpoint_id }).to_string(),
        )?;
        context.git.run(
            &context.facts.root,
            InternalGitProfile::WorktreeManagement,
            &args2(&["update-ref", &branch, &source.head_oid]),
        )?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            &serde_json::json!({ "phase": "branch_created", "restore_from": checkpoint_id })
                .to_string(),
        )?;
        context.git.run(
            &context.facts.root,
            InternalGitProfile::WorktreeManagement,
            &[
                OsString::from("worktree"),
                OsString::from("add"),
                path.as_os_str().to_owned(),
                OsString::from(branch.trim_start_matches("refs/heads/")),
            ],
        )?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            &serde_json::json!({ "phase": "worktree_added", "restore_from": checkpoint_id })
                .to_string(),
        )?;
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
        failpoint("restore.after_index_replace");
        context.state.update_operation(
            &operation.id,
            OperationStatus::Verifying,
            &serde_json::json!({ "phase": "index_restored", "restore_from": checkpoint_id })
                .to_string(),
        )?;
        let facts = task_facts(&context.git, &record, &context.facts)?;
        let restored_state = task::content_state(&context.git, &facts, &path)?;
        if !restored_state.visibility_flags.is_empty()
            || restored_state.in_progress
            || !restored_state.ignored_residue.is_empty()
        {
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
        context.state.update_operation(
            &operation.id,
            OperationStatus::Verifying,
            &serde_json::json!({ "phase": "marker_written", "restore_from": checkpoint_id })
                .to_string(),
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

    fn overlap(context: &Context, arguments: OverlapArgs, json: bool) -> Result<i32, AppError> {
        let show_all = arguments.all || (!arguments.planned && !arguments.actual);
        let show_planned = arguments.planned || show_all;
        let show_actual = arguments.actual || show_all;
        let tasks = if arguments.tasks.is_empty() {
            context.state.tasks()?
        } else {
            arguments
                .tasks
                .iter()
                .map(|selector| context.state.task(selector))
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut changes = BTreeMap::new();
        let mut scopes = BTreeMap::new();
        let mut reports = Vec::new();
        for record in &tasks {
            let paths = task::all_change_paths(&context.git, record)?;
            let violations = task::scope_violations(&record.scopes, &paths)?;
            scopes.insert(record.id.clone(), record.scopes.clone());
            changes.insert(record.id.clone(), paths);
            reports.push(serde_json::json!({
                "task_id": record.id,
                "planned_scopes": if show_planned { serde_json::json!(record.scopes) } else { serde_json::Value::Null },
                "actual_paths": if show_actual { serde_json::json!(changes[&record.id]) } else { serde_json::Value::Null },
                "scope_violations": if show_actual { serde_json::json!(violations) } else { serde_json::Value::Null },
            }));
        }
        let mut pairs = Vec::new();
        let mut planned_pairs = Vec::new();
        let ids: Vec<_> = changes.keys().cloned().collect();
        for (index, left) in ids.iter().enumerate() {
            for right in ids.iter().skip(index + 1) {
                if show_actual {
                    let intersection: Vec<_> = changes[left]
                        .intersection(&changes[right])
                        .cloned()
                        .collect();
                    if !intersection.is_empty() {
                        pairs.push(serde_json::json!({ "left": left, "right": right, "paths": intersection, "semantic_conflict": false }));
                    }
                }
                if show_planned {
                    let intersection: Vec<_> = scopes[left]
                        .iter()
                        .filter(|scope| scopes[right].contains(scope))
                        .cloned()
                        .collect();
                    if !intersection.is_empty() {
                        planned_pairs.push(serde_json::json!({ "left": left, "right": right, "scopes": intersection, "semantic_conflict": false }));
                    }
                }
            }
        }
        output(
            json,
            "overlap",
            None,
            &serde_json::json!({ "tasks": reports, "pairs": pairs, "planned_pairs": planned_pairs, "mode": if show_all { "all" } else if show_planned { "planned" } else { "actual" }, "heuristic": true, "semantic_conflict_detection": false }),
        )
    }

    fn check(context: &Context, selector: &str, json: bool) -> Result<i32, AppError> {
        let record = context.state.task(selector)?;
        let (results, required_failure) = Self::execute_checks(context, &record.id)?;
        if required_failure {
            return Err(AppError::diagnostic(
                "AGT-0726",
                "a required check failed or became stale",
                ErrorKind::StateInconsistent,
            ));
        }
        output(
            json,
            "check",
            None,
            &serde_json::json!({ "task_id": record.id, "results": results }),
        )
    }

    fn execute_checks(
        context: &Context,
        selector: &str,
    ) -> Result<(Vec<serde_json::Value>, bool), AppError> {
        let record = context.state.task(selector)?;
        if context.state.sessions_for_task(&record.id)? > 0 {
            return Err(AppError::diagnostic(
                "AGT-0726",
                "active session prevents check",
                ErrorKind::LockConflict,
            ));
        }
        let _task_lock = task_lock(&context.manifest, &record.id)?;
        let snapshot = context.state.config(&record.id)?;
        let definition_hash = hash_json(&snapshot.checks)?;
        let start_head = context.git.text(
            &record.path,
            InternalGitProfile::Discovery,
            &args(&["rev-parse", "HEAD"]),
        )?;
        let mut results = Vec::new();
        let mut required_failure = false;
        for definition in snapshot.checks {
            let process = run_bounded_check(&record.path, &definition)?;
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
            let passed =
                process.status.success() && !stale && !process.timed_out && !process.output_limited;
            if definition.required && !passed {
                required_failure = true;
            }
            context.state.save_check_run(&CheckRunRecord {
                id: crate::domain::Id::new("check-").0,
                task_id: record.id.clone(),
                head_oid: start_head.clone(),
                config_hash: snapshot.hash.clone(),
                definition_hash: definition_hash.clone(),
                status: if passed {
                    "passed".to_owned()
                } else if stale {
                    "stale".to_owned()
                } else {
                    "failed".to_owned()
                },
                command_json: serde_json::to_string(&definition.command)?,
            })?;
            results.push(serde_json::json!({
                "name": definition.name,
                "required": definition.required,
                "exit_code": process.status.code(),
                "passed": passed,
                "stale": stale,
                "timed_out": process.timed_out,
                "output_limited": process.output_limited,
                "head_oid": start_head.clone(),
                "stdout": String::from_utf8_lossy(&process.stdout),
                "stderr": String::from_utf8_lossy(&process.stderr),
            }));
        }
        Ok((results, required_failure))
    }

    fn fetch(context: &Context, remote: &str, json: bool) -> Result<i32, AppError> {
        if remote.is_empty()
            || remote.starts_with('-')
            || remote.starts_with('.')
            || remote.ends_with('.')
            || remote.contains('/')
            || remote.contains("..")
            || remote.contains(char::is_whitespace)
        {
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
        context.git.require_options(
            &context.facts.root,
            InternalGitProfile::Fetch,
            "fetch",
            &[
                "--[no-]prune",
                "--[no-]tags",
                "--[no-]recurse-submodules",
                "--[no-]write-fetch-head",
            ],
        )?;
        let operation = context.state.create_operation(
            OperationKind::Fetch,
            None,
            &serde_json::json!({ "remote": remote }).to_string(),
        )?;
        let refs_before = ref_snapshot(&context.git, &context.facts.root)?;
        let fetch_head = context.facts.git_dir.join("FETCH_HEAD");
        let fetch_head_before = fs::read(&fetch_head).ok();
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            "{\"phase\":\"fetch_started\"}",
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
        if let Err(error) = context
            .git
            .run(&context.facts.root, InternalGitProfile::Fetch, &args)
        {
            let _ = context.state.update_operation(
                &operation.id,
                OperationStatus::Failed,
                &serde_json::json!({ "phase": "fetch_failed", "error": error.to_string() })
                    .to_string(),
            );
            return Err(error);
        }
        let refs_after = ref_snapshot(&context.git, &context.facts.root)?;
        let allowed_prefix = format!("refs/remotes/{remote}/");
        let mut changed_refs = Vec::new();
        let keys = refs_before
            .keys()
            .chain(refs_after.keys())
            .collect::<std::collections::BTreeSet<_>>();
        for reference in keys {
            if !reference.starts_with(&allowed_prefix)
                && refs_before.get(reference) != refs_after.get(reference)
            {
                changed_refs.push(reference.clone());
            }
        }
        if !changed_refs.is_empty() || fs::read(&fetch_head).ok() != fetch_head_before {
            let observed = serde_json::json!({
                "phase": "unexpected_side_effect",
                "changed_refs": changed_refs,
                "fetch_head_changed": true,
            });
            let _ = context.state.update_operation(
                &operation.id,
                OperationStatus::ManualIntervention,
                &observed.to_string(),
            );
            return Err(AppError::diagnostic(
                "AGT-0769",
                "fetch changed a ref or FETCH_HEAD outside the validated destination",
                ErrorKind::RecoveryRequired,
            ));
        }
        context.state.update_operation(
            &operation.id,
            OperationStatus::Verifying,
            "{\"phase\":\"refs_verified\"}",
        )?;
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
        let mut record = context.state.task(&arguments.task)?;
        if arguments.r#continue || arguments.abort {
            return Self::sync_continuation(
                context,
                &record,
                arguments.r#continue,
                arguments.abort,
                json,
            );
        }
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
        if record.lifecycle != Lifecycle::Active {
            return Err(AppError::diagnostic(
                "AGT-0729",
                "sync requires an active task",
                ErrorKind::StateInconsistent,
            ));
        }
        context.git.require_options(
            &record.path,
            InternalGitProfile::Sync,
            "rebase",
            &["--[no-]update-refs", "--[no-]autostash"],
        )?;
        let _task_lock = task_lock(&context.manifest, &record.id)?;
        refresh_head(context, &mut record)?;
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
        if continue_rebase == abort {
            return Err(AppError::diagnostic(
                "AGT-0730",
                "choose exactly one of --continue or --abort",
                ErrorKind::Usage,
            ));
        }
        if !matches!(record.lifecycle, Lifecycle::Active | Lifecycle::Conflicted) {
            return Err(AppError::diagnostic(
                "AGT-0729",
                "sync continuation requires an active or conflicted task",
                ErrorKind::StateInconsistent,
            ));
        }
        let _task_lock = task_lock(&context.manifest, &record.id)?;
        let rebase_state = rebase_state_dir(&context.git, &record.path)?.ok_or_else(|| {
            AppError::diagnostic(
                "AGT-0731",
                "no rebase is in progress for this task",
                ErrorKind::StateInconsistent,
            )
        })?;
        let operation = context
            .state
            .incomplete_operation_for_task_kind(&record.id, OperationKind::Sync.as_str())?
            .ok_or_else(|| {
                AppError::diagnostic(
                    "AGT-0732",
                    "rebase continuation has no incomplete sync operation",
                    ErrorKind::RecoveryRequired,
                )
            })?;
        let expected: serde_json::Value = serde_json::from_str(&operation.expected)?;
        let target_oid = expected["target_oid"].as_str().ok_or_else(|| {
            AppError::diagnostic(
                "AGT-0733",
                "sync operation lacks its target OID",
                ErrorKind::RecoveryRequired,
            )
        })?;
        task::rebase_facts_match(record, &rebase_state, target_oid)?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            &serde_json::json!({ "phase": "continuation_started" }).to_string(),
        )?;
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
                    if continue_rebase {
                        Some(target_oid)
                    } else {
                        None
                    },
                )?;
                context.state.update_operation(
                    &operation.id,
                    OperationStatus::Completed,
                    &serde_json::json!({ "phase": if continue_rebase { "rebase_continued" } else { "rebase_aborted" }, "head": head }).to_string(),
                )?;
                output(
                    json,
                    "sync",
                    Some(&operation.id),
                    &serde_json::json!({ "task_id": record.id, "phase": command, "head_oid": head }),
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
                    &serde_json::json!({ "phase": "continuation_conflicted", "error": error.to_string() }).to_string(),
                );
                Err(error)
            }
        }
    }

    fn land(context: &Context, arguments: LandArgs, json: bool) -> Result<i32, AppError> {
        let mut record = context.state.task(&arguments.task)?;
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
        if arguments.into_current == arguments.onto.is_some() {
            return Err(AppError::diagnostic(
                "AGT-0731",
                "choose exactly one of --into-current or --onto",
                ErrorKind::Usage,
            ));
        }
        if record.lifecycle != Lifecycle::Active {
            return Err(AppError::diagnostic(
                "AGT-0738",
                "land requires an active task",
                ErrorKind::StateInconsistent,
            ));
        }
        let _task_lock = task_lock(&context.manifest, &record.id)?;
        let _repo_lock = repo_lock(&context.manifest)?;
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
        let config = context.state.config(&record.id)?;
        let definition_hash = hash_json(&config.checks)?;
        let required_count = config.checks.iter().filter(|check| check.required).count();
        if required_count > 0
            && !context.state.fresh_required_checks(
                &record.id,
                &task_oid,
                &record.config_hash,
                &definition_hash,
                required_count,
            )?
        {
            return Err(AppError::diagnostic(
                "AGT-0739",
                "required checks are missing or stale for the exact task HEAD",
                ErrorKind::StateInconsistent,
            ));
        }
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
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            "{\"phase\":\"safety_refs_created\"}",
        )?;
        if arguments.into_current {
            let current = repository::discover(&context.git, &std::env::current_dir()?)?;
            let current_branch_name = current
                .branch
                .as_deref()
                .map(|branch| branch.trim_start_matches("refs/heads/"));
            if current_branch_name != Some(target.trim_start_matches("refs/heads/"))
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
            failpoint("land.after_fast_forward");
            context.state.update_operation(
                &operation.id,
                OperationStatus::Verifying,
                &serde_json::json!({ "phase": "target_updated", "target_oid": task_oid })
                    .to_string(),
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
                OsString::from(target_ref.trim_start_matches("refs/heads/")),
            ],
        )?;
        context.state.update_operation(
            &operation.id,
            OperationStatus::Executing,
            &serde_json::json!({
                "phase": "landing_worktree_added",
                "landing_path": landing_path,
            })
            .to_string(),
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
        failpoint("land.after_fast_forward");
        context.state.update_operation(
            &operation.id,
            OperationStatus::Verifying,
            &serde_json::json!({ "phase": "target_updated", "target_oid": task_oid, "landing_path": landing_path }).to_string(),
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
        if raw.iter().any(is_denied_global) {
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
        let active_session = current_process_group()
            .map(|pgid| state.active_session_for_process_group(pgid))
            .transpose()?
            .flatten();
        if is_administrative_shim_command(&raw)
            && (std::env::var_os("AGENTREE_SESSION_ID").is_some() || active_session.is_some())
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

fn refresh_head(context: &Context, record: &mut TaskRecord) -> Result<(), AppError> {
    let observed = context.git.text(
        &record.path,
        InternalGitProfile::Discovery,
        &args2(&["rev-parse", "HEAD"]),
    )?;
    if observed != record.head_oid {
        context
            .state
            .update_task_lifecycle(&record.id, record.lifecycle, Some(&observed), None)?;
        record.head_oid = observed;
    }
    Ok(())
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

fn rebase_state_dir(git: &GitRunner, worktree: &Path) -> Result<Option<PathBuf>, AppError> {
    let merge_dir = resolve_git_path(git, worktree, "rebase-merge")?;
    if merge_dir.exists() {
        return Ok(Some(merge_dir));
    }
    let apply_dir = resolve_git_path(git, worktree, "rebase-apply")?;
    if apply_dir.exists() {
        return Ok(Some(apply_dir));
    }
    Ok(None)
}

fn sha256_file(path: &Path) -> Result<String, AppError> {
    let bytes = fs::read(path)?;
    Ok(sha256_hex(bytes))
}

fn ref_snapshot(git: &GitRunner, root: &Path) -> Result<BTreeMap<String, String>, AppError> {
    let output = git.run(
        root,
        InternalGitProfile::Discovery,
        &args(&["for-each-ref", "--format=%(refname)%00%(objectname)%00"]),
    )?;
    let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut snapshot = BTreeMap::new();
    for pair in fields.chunks(2) {
        if pair.len() < 2 || pair[0].is_empty() {
            continue;
        }
        let reference = String::from_utf8(pair[0].to_vec()).map_err(|_| {
            AppError::diagnostic(
                "AGT-0770",
                "Git returned a non-UTF-8 ref name",
                ErrorKind::Unsupported,
            )
        })?;
        let oid = String::from_utf8(pair[1].to_vec()).map_err(|_| {
            AppError::diagnostic(
                "AGT-0771",
                "Git returned a non-UTF-8 ref OID",
                ErrorKind::Unsupported,
            )
        })?;
        snapshot.insert(reference, oid);
    }
    Ok(snapshot)
}

fn ensure_index_unlocked(index: &Path) -> Result<(), AppError> {
    let lock = index.parent().unwrap_or(Path::new(".")).join(format!(
        "{}.lock",
        index.file_name().unwrap_or_default().to_string_lossy()
    ));
    if lock.exists() {
        return Err(AppError::diagnostic(
            "AGT-0748",
            "Git index is locked by another process",
            ErrorKind::LockConflict,
        ));
    }
    Ok(())
}

fn hash_json<T: Serialize>(value: &T) -> Result<String, AppError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(format!("sha256:{}", sha256_hex(bytes)))
}

fn process_birth_identity(pid: u32) -> String {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        if let Some(rest) = stat.rsplit_once(") ") {
            if let Some(start) = rest.1.split_whitespace().nth(19) {
                return format!("linux-starttime:{start}");
            }
        }
    }
    if let Ok(output) = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
    {
        return format!(
            "ps-start:{:.40}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    "unavailable".to_owned()
}

fn session_plan(session: &SessionRecord) -> Result<serde_json::Value, AppError> {
    let state = classify_session(session);
    let action = match state {
        "active" => "leave session untouched",
        "orphaned" if child_identity_verified(session) => {
            "mark orphaned and signal the verified child process group"
        }
        "orphaned" => "mark orphaned without signaling an unverified child process",
        _ => "manual intervention; process identity is inconclusive",
    };
    let fingerprint = hash_json(&serde_json::json!({
        "session_id": session.id,
        "task_id": session.task_id,
        "status": session.status,
        "pid": session.pid,
        "pgid": session.pgid,
        "birth_id": session.birth_id,
        "supervisor_pid": session.supervisor_pid,
        "supervisor_birth_id": session.supervisor_birth_id,
        "state": state,
    }))?;
    Ok(serde_json::json!({
        "session_id": session.id,
        "task_id": session.task_id,
        "status": session.status,
        "pid": session.pid,
        "pgid": session.pgid,
        "supervisor_pid": session.supervisor_pid,
        "state": state,
        "action": action,
        "plan_fingerprint": fingerprint,
    }))
}

fn child_identity_verified(session: &SessionRecord) -> bool {
    let (Some(pid), Some(expected)) = (session.pid, session.birth_id.as_deref()) else {
        return false;
    };
    expected != "unavailable" && process_birth_identity(pid) == expected
}

fn classify_session(session: &SessionRecord) -> &'static str {
    let Some(supervisor_pid) = session.supervisor_pid else {
        return "unknown";
    };
    if !process_exists(supervisor_pid) {
        return "orphaned";
    }
    let Some(expected) = session.supervisor_birth_id.as_deref() else {
        return "unknown";
    };
    if expected == "unavailable" {
        return "unknown";
    }
    if process_birth_identity(supervisor_pid) == expected {
        "active"
    } else {
        "orphaned"
    }
}

fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn terminate_orphaned_session(session: &SessionRecord) -> bool {
    let Some(pid) = session.pid else {
        return false;
    };
    if !child_identity_verified(session) {
        return false;
    }
    #[cfg(unix)]
    {
        let pgid = session.pgid.unwrap_or_default();
        let current_pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        if pgid > 1 && current_pgid == pgid as libc::pid_t {
            unsafe {
                libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
            }
            true
        } else {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            true
        }
    }
    #[cfg(not(unix))]
    {
        let _ = session;
        false
    }
}

fn spawn_session_child(
    context: &Context,
    record: &TaskRecord,
    arguments: &RunArgs,
    session_id: &str,
) -> Result<Child, AppError> {
    let session_root = PathBuf::from(&context.manifest.state_dir)
        .join("sessions")
        .join(session_id);
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
    command.env("AGENTREE_SESSION_ID", session_id);
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
    Ok(command.spawn()?)
}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        if pid > 0 {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

fn failpoint(name: &str) {
    if std::env::var_os("AGENTREE_FAILPOINT").is_some_and(|value| value == name) {
        eprintln!("agentree failpoint triggered: {name}");
        std::process::exit(90);
    }
}

struct BoundedCheckOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
    output_limited: bool,
}

fn run_bounded_check(
    worktree: &Path,
    definition: &config::CheckDefinition,
) -> Result<BoundedCheckOutput, AppError> {
    let mut command = Command::new(&definition.command[0]);
    command.args(definition.command.iter().skip(1));
    command.current_dir(worktree);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    sanitize_child_environment(&mut command);
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
    let stdout = child.stdout.take().ok_or_else(|| {
        AppError::diagnostic(
            "AGT-0751",
            "check stdout pipe was not created",
            ErrorKind::Io,
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        AppError::diagnostic(
            "AGT-0752",
            "check stderr pipe was not created",
            ErrorKind::Io,
        )
    })?;
    let output_limited = Arc::new(AtomicBool::new(false));
    let stdout_limited = Arc::clone(&output_limited);
    let stderr_limited = Arc::clone(&output_limited);
    let output_limit = definition.output_limit_bytes;
    let stdout_handle = thread::spawn(move || read_limited(stdout, output_limit, stdout_limited));
    let stderr_handle = thread::spawn(move || read_limited(stderr, output_limit, stderr_limited));
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(definition.timeout_seconds))
        .ok_or_else(|| {
            AppError::diagnostic(
                "AGT-0755",
                "check timeout cannot be represented by the system clock",
                ErrorKind::Usage,
            )
        })?;
    let mut timed_out = false;
    let status = loop {
        if output_limited.load(Ordering::Acquire) {
            terminate_child(&mut child);
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            terminate_child(&mut child);
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_handle.join().map_err(|_| {
        AppError::diagnostic("AGT-0753", "check stdout reader failed", ErrorKind::Io)
    })?;
    let stderr = stderr_handle.join().map_err(|_| {
        AppError::diagnostic("AGT-0754", "check stderr reader failed", ErrorKind::Io)
    })?;
    Ok(BoundedCheckOutput {
        status,
        stdout,
        stderr,
        timed_out,
        output_limited: output_limited.load(Ordering::Acquire),
    })
}

fn read_limited<R: Read>(mut reader: R, limit: usize, output_limited: Arc<AtomicBool>) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = limit.saturating_sub(output.len());
                output.extend_from_slice(&buffer[..read.min(remaining)]);
                if read > remaining {
                    output_limited.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }
    output
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

fn is_administrative_command(command: &CliCommand) -> bool {
    matches!(
        command,
        CliCommand::Init
            | CliCommand::Config { .. }
            | CliCommand::New(_)
            | CliCommand::Run(_)
            | CliCommand::Shell { .. }
            | CliCommand::Git { .. }
            | CliCommand::Remove { .. }
            | CliCommand::Archive { .. }
            | CliCommand::DeleteBranch { .. }
            | CliCommand::Doctor { .. }
            | CliCommand::Fetch { .. }
            | CliCommand::Sync(_)
            | CliCommand::Resolve { .. }
            | CliCommand::Land(_)
    )
}

fn is_administrative_shim_command(arguments: &[OsString]) -> bool {
    arguments.first().is_some_and(|argument| {
        matches!(
            argument.to_string_lossy().as_ref(),
            "new" | "remove" | "fetch" | "sync" | "land" | "doctor" | "agentree"
        )
    })
}

fn current_process_group() -> Option<u32> {
    #[cfg(unix)]
    {
        let pgid = unsafe { libc::getpgrp() };
        (pgid > 0).then_some(pgid as u32)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn enforce_session_guard(
    git: &GitRunner,
    facts: &RepositoryFacts,
    command: &CliCommand,
) -> Result<(), AppError> {
    if !is_administrative_command(command) {
        return Ok(());
    }
    let manifest = match repository::load_manifest(facts) {
        Ok(manifest) => manifest,
        Err(error) => {
            if std::env::var_os("AGENTREE_SESSION_ID").is_some() {
                return Err(AppError::diagnostic(
                    "AGT-0747",
                    "administrative Agentree command is denied inside a task session",
                    ErrorKind::SessionGuardDenied,
                ));
            }
            let _ = (git, error);
            return Ok(());
        }
    };
    let state = State::open(&PathBuf::from(&manifest.state_dir).join("state.sqlite3"))?;
    let active = current_process_group()
        .map(|pgid| state.active_session_for_process_group(pgid))
        .transpose()?
        .flatten();
    if active.is_some() || std::env::var_os("AGENTREE_SESSION_ID").is_some() {
        return Err(AppError::diagnostic(
            "AGT-0747",
            "administrative Agentree command is denied inside a verified task session",
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

fn command_name(command: &CliCommand) -> &'static str {
    match command {
        CliCommand::Init => "init",
        CliCommand::Config { .. } => "config",
        CliCommand::New(_) => "new",
        CliCommand::Status => "status",
        CliCommand::Context { .. } => "context",
        CliCommand::Diff { .. } => "diff",
        CliCommand::Run(_) => "run",
        CliCommand::Shell { .. } => "shell",
        CliCommand::Git { .. } => "git",
        CliCommand::Remove { .. } => "remove",
        CliCommand::Archive { .. } => "archive",
        CliCommand::DeleteBranch { .. } => "delete-branch",
        CliCommand::Doctor { .. } => "doctor",
        CliCommand::Checkpoint { .. } => "checkpoint",
        CliCommand::Overlap(_) => "overlap",
        CliCommand::Check { .. } => "check",
        CliCommand::Fetch { .. } => "fetch",
        CliCommand::Sync(_) => "sync",
        CliCommand::Resolve { .. } => "resolve",
        CliCommand::Land(_) => "land",
    }
}

fn print_error_json(command: &str, error: &AppError) {
    let envelope = JsonEnvelope::<serde_json::Value> {
        schema_version: 1,
        ok: false,
        command: command.to_owned(),
        operation_id: None,
        result: None,
        error: Some(error.render()),
    };
    if let Ok(value) = serde_json::to_string_pretty(&envelope) {
        println!("{value}");
    }
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

fn doctor_fingerprint(operation: &crate::state::OperationRecord) -> String {
    hash_json(&serde_json::json!({ "operation_id": operation.id, "kind": operation.kind, "status": operation.status, "task_id": operation.task_id, "expected": operation.expected, "observed": operation.observed })).unwrap_or_else(|_| "unavailable".to_owned())
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
    AppError::diagnostic("AGT-0700", error.to_string(), ErrorKind::Usage)
}
