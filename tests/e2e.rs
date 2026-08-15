use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::Duration,
};

use serde_json::Value;
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git must be available")
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(git(dir.path(), &["init", "-q"]).status.success());
    assert!(git(dir.path(), &["config", "user.name", "Agentree Test"])
        .status
        .success());
    assert!(git(
        dir.path(),
        &["config", "user.email", "agentree@example.invalid"]
    )
    .status
    .success());
    fs::write(dir.path().join("README"), "base\n").expect("write base");
    assert!(git(dir.path(), &["add", "README"]).status.success());
    assert!(git(dir.path(), &["commit", "-qm", "initial"])
        .status
        .success());
    dir
}

fn run(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentree"))
        .arg("--repository")
        .arg(repo)
        .args(args)
        .output()
        .expect("agentree must run")
}

fn run_with_env(repo: &Path, key: &str, value: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentree"))
        .arg("--repository")
        .arg(repo)
        .env(key, value)
        .args(args)
        .output()
        .expect("agentree must run")
}

fn run_in_with_env(repo: &Path, key: &str, value: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentree"))
        .current_dir(repo)
        .arg("--repository")
        .arg(repo)
        .env(key, value)
        .args(args)
        .output()
        .expect("agentree must run")
}

fn run_in(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentree"))
        .current_dir(repo)
        .arg("--repository")
        .arg(repo)
        .args(args)
        .output()
        .expect("agentree must run")
}

fn result_path(output: &Output) -> PathBuf {
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    PathBuf::from(value["result"]["worktree"].as_str().expect("worktree path"))
}

#[test]
fn tasks_have_separate_worktrees_and_indexes() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    let task_a = run(repo.path(), &["new", "alpha", "--json"]);
    let task_b = run(repo.path(), &["new", "beta", "--json"]);
    assert!(task_a.status.success());
    assert!(task_b.status.success());
    let worktree_a = result_path(&task_a);
    let worktree_b = result_path(&task_b);
    fs::write(worktree_a.join("only-alpha"), "alpha\n").expect("write alpha");
    assert!(git(&worktree_a, &["add", "only-alpha"]).status.success());
    let status_b = git(&worktree_b, &["status", "--porcelain=v2"]);
    assert!(status_b.status.success());
    assert!(
        status_b.stdout.is_empty(),
        "task B saw task A's index: {:?}",
        status_b.stdout
    );
}

#[test]
fn checkpoint_preserves_staged_and_unstaged_trees() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    let created = run(repo.path(), &["new", "checkpoint", "--json"]);
    let worktree = result_path(&created);
    fs::write(worktree.join("README"), "staged\n").expect("write staged");
    assert!(git(&worktree, &["add", "README"]).status.success());
    fs::write(worktree.join("README"), "unstaged\n").expect("write unstaged");
    let checkpoint = run(
        repo.path(),
        &["checkpoint", "create", "checkpoint", "--json"],
    );
    assert!(
        checkpoint.status.success(),
        "{}",
        String::from_utf8_lossy(&checkpoint.stderr)
    );
    let list = run(repo.path(), &["checkpoint", "list", "checkpoint", "--json"]);
    assert!(list.status.success());
    let envelope: Value = serde_json::from_slice(&list.stdout).expect("checkpoint list JSON");
    let items: Vec<Value> =
        serde_json::from_value(envelope["result"].clone()).expect("checkpoint list result");
    assert_eq!(items.len(), 1);
    assert_ne!(items[0]["index_tree_oid"], items[0]["worktree_tree_oid"]);
    let status = git(&worktree, &["status", "--porcelain"]);
    assert_eq!(String::from_utf8_lossy(&status.stdout).trim(), "MM README");
}

#[test]
fn remove_refuses_ignored_and_empty_residue_but_archive_keeps_it() {
    let repo = fixture();
    assert!(
        git(repo.path(), &["config", "core.excludesFile", "/dev/null"])
            .status
            .success()
    );
    fs::write(repo.path().join(".git/info/exclude"), "cache/\n").expect("exclude");
    assert!(run(repo.path(), &["init"]).status.success());
    let created = run(repo.path(), &["new", "dirty", "--json"]);
    let worktree = result_path(&created);
    fs::create_dir_all(worktree.join("cache/empty")).expect("empty ignored directory");
    fs::write(worktree.join("cache/secret"), "keep\n").expect("ignored file");
    let removed = run(repo.path(), &["remove", "dirty"]);
    assert!(!removed.status.success());
    assert!(worktree.join("cache/secret").exists());
    let archived = run(repo.path(), &["archive", "dirty"]);
    assert!(
        archived.status.success(),
        "{}",
        String::from_utf8_lossy(&archived.stderr)
    );
    assert!(worktree.join("cache/secret").exists());
}

#[test]
fn strict_shim_denies_history_reset_inside_session() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "guard"]).status.success());
    let result = run(
        repo.path(),
        &["run", "guard", "--", "/bin/sh", "-c", "git reset --hard"],
    );
    assert!(!result.status.success());
    let status = String::from_utf8_lossy(&result.stderr);
    assert!(status.contains("denied"), "unexpected diagnostic: {status}");
}

#[test]
fn land_into_current_is_fast_forward_only() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    let created = run(repo.path(), &["new", "land", "--json"]);
    let worktree = result_path(&created);
    fs::write(worktree.join("landed"), "landed\n").expect("write task");
    assert!(git(&worktree, &["add", "landed"]).status.success());
    assert!(git(&worktree, &["commit", "-qm", "land"]).status.success());
    let landed = run_in(repo.path(), &["land", "land", "--into-current"]);
    assert!(
        landed.status.success(),
        "{}",
        String::from_utf8_lossy(&landed.stderr)
    );
    let target = String::from_utf8(git(repo.path(), &["rev-parse", "HEAD"]).stdout).expect("oid");
    let task_head = String::from_utf8(git(&worktree, &["rev-parse", "HEAD"]).stdout).expect("oid");
    assert_eq!(target.trim(), task_head.trim());
}

#[test]
fn sync_rebases_only_the_task_against_an_exact_target() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "sync"]).status.success());
    let task_path = run(repo.path(), &["context", "sync", "--json"]);
    let context: Value = serde_json::from_slice(&task_path.stdout).expect("context JSON");
    let worktree = PathBuf::from(context["result"]["worktree"].as_str().expect("worktree"));
    let committed = run(
        repo.path(),
        &[
            "run",
            "sync",
            "--",
            "/bin/sh",
            "-c",
            "printf task > task.txt; git add task.txt; git commit -m task",
        ],
    );
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    fs::write(repo.path().join("target.txt"), "target\n").expect("target change");
    assert!(git(repo.path(), &["add", "target.txt"]).status.success());
    assert!(git(repo.path(), &["commit", "-qm", "target"])
        .status
        .success());
    let synced = run(repo.path(), &["sync", "sync", "--onto", "main"]);
    assert!(
        synced.status.success(),
        "{}",
        String::from_utf8_lossy(&synced.stderr)
    );
    assert!(worktree.join("target.txt").exists());
    assert!(git(repo.path(), &["diff", "--quiet", "--", "target.txt"])
        .status
        .success());
}

fn start_conflicting_sync(repo: &Path) -> PathBuf {
    assert!(run(repo, &["init"]).status.success());
    let created = run(repo, &["new", "conflict", "--json"]);
    assert!(created.status.success());
    let worktree = result_path(&created);
    fs::write(worktree.join("conflict.txt"), "task\n").expect("task conflict");
    assert!(git(&worktree, &["add", "conflict.txt"]).status.success());
    assert!(git(&worktree, &["commit", "-qm", "task-conflict"])
        .status
        .success());
    fs::write(repo.join("conflict.txt"), "main\n").expect("main conflict");
    assert!(git(repo, &["add", "conflict.txt"]).status.success());
    assert!(git(repo, &["commit", "-qm", "main-conflict"])
        .status
        .success());
    let started = run(repo, &["sync", "conflict", "--onto", "main"]);
    assert!(!started.status.success());
    assert!(String::from_utf8_lossy(&started.stderr).contains("conflict"));
    worktree
}

#[test]
fn sync_abort_recovers_a_real_rebase_conflict() {
    let repo = fixture();
    let worktree = start_conflicting_sync(repo.path());
    let aborted = run(repo.path(), &["sync", "conflict", "--abort", "--json"]);
    assert!(
        aborted.status.success(),
        "{}",
        String::from_utf8_lossy(&aborted.stderr)
    );
    let envelope: Value = serde_json::from_slice(&aborted.stdout).expect("abort JSON");
    assert_eq!(envelope["ok"], true);
    assert!(git(&worktree, &["symbolic-ref", "--short", "HEAD"])
        .status
        .success());
    assert!(git(&worktree, &["diff", "--quiet"]).status.success());
    assert!(git(&worktree, &["diff", "--cached", "--quiet"])
        .status
        .success());
    let status = run(repo.path(), &["status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["result"][0]["state"], "active");
}

#[test]
fn sync_continue_recovers_a_resolved_rebase_conflict() {
    let repo = fixture();
    let worktree = start_conflicting_sync(repo.path());
    fs::write(worktree.join("conflict.txt"), "resolved\n").expect("resolve conflict");
    assert!(git(&worktree, &["add", "conflict.txt"]).status.success());
    let continued = run(repo.path(), &["sync", "conflict", "--continue", "--json"]);
    assert!(
        continued.status.success(),
        "{}",
        String::from_utf8_lossy(&continued.stderr)
    );
    let envelope: Value = serde_json::from_slice(&continued.stdout).expect("continue JSON");
    assert_eq!(envelope["ok"], true);
    assert_eq!(
        fs::read_to_string(worktree.join("conflict.txt")).expect("resolved content"),
        "resolved\n"
    );
    assert!(git(&worktree, &["symbolic-ref", "--short", "HEAD"])
        .status
        .success());
    let status = run(repo.path(), &["status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["result"][0]["state"], "active");
}

#[test]
fn temporary_landing_worktree_updates_only_an_unclaimed_target() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "temporary"]).status.success());
    let committed = run(
        repo.path(),
        &[
            "run",
            "temporary",
            "--",
            "/bin/sh",
            "-c",
            "printf landed > landed.txt; git add landed.txt; git commit -m landed",
        ],
    );
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    assert!(git(repo.path(), &["branch", "release"]).status.success());
    let landed = run(repo.path(), &["land", "temporary", "--onto", "release"]);
    assert!(
        landed.status.success(),
        "{}",
        String::from_utf8_lossy(&landed.stderr)
    );
    let release_head =
        String::from_utf8(git(repo.path(), &["rev-parse", "release"]).stdout).expect("oid");
    let task_context = run(repo.path(), &["context", "temporary", "--json"]);
    let context: Value = serde_json::from_slice(&task_context.stdout).expect("context JSON");
    let task_path = PathBuf::from(context["result"]["worktree"].as_str().expect("worktree"));
    let task_head = String::from_utf8(git(&task_path, &["rev-parse", "HEAD"]).stdout).expect("oid");
    assert_eq!(release_head.trim(), task_head.trim());
}

#[test]
fn checkpoint_restore_recreates_a_dirty_task_without_deleting_the_source() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "source"]).status.success());
    let created = run(
        repo.path(),
        &[
            "run",
            "source",
            "--",
            "/bin/sh",
            "-c",
            "printf staged > README; git add README; printf unstaged > README",
        ],
    );
    assert!(created.status.success());
    let list = run(repo.path(), &["checkpoint", "list", "source", "--json"]);
    let envelope: Value = serde_json::from_slice(&list.stdout).expect("checkpoint JSON");
    let checkpoint_id = envelope["result"][0]["id"].as_str().expect("checkpoint id");
    let restored = run(
        repo.path(),
        &[
            "checkpoint",
            "restore",
            checkpoint_id,
            "--to-new-task",
            "recovered",
            "--json",
        ],
    );
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    let restored_path = result_path(&restored);
    let status = git(&restored_path, &["status", "--porcelain"]);
    assert_eq!(String::from_utf8_lossy(&status.stdout).trim(), "MM README");
    let source = run(repo.path(), &["context", "source", "--json"]);
    assert!(source.status.success());
}

#[test]
fn fetch_does_not_prune_or_write_outside_remote_tracking_refs() {
    let repo = fixture();
    let remote = tempfile::tempdir().expect("remote tempdir");
    assert!(git(remote.path(), &["init", "--bare", "-q"])
        .status
        .success());
    assert!(git(
        repo.path(),
        &[
            "remote",
            "add",
            "origin",
            remote.path().to_str().expect("remote path")
        ]
    )
    .status
    .success());
    assert!(git(repo.path(), &["push", "-q", "origin", "main"])
        .status
        .success());
    let head = String::from_utf8(git(repo.path(), &["rev-parse", "HEAD"]).stdout).expect("oid");
    assert!(git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/stale", head.trim()]
    )
    .status
    .success());
    assert!(git(repo.path(), &["config", "fetch.prune", "true"])
        .status
        .success());
    assert!(run(repo.path(), &["init"]).status.success());
    let fetched = run(repo.path(), &["fetch", "--remote", "origin"]);
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    let stale = git(
        repo.path(),
        &["show-ref", "--verify", "refs/remotes/origin/stale"],
    );
    assert!(
        stale.status.success(),
        "fetch unexpectedly pruned a stale ref"
    );
    let fetch_head = repo.path().join(".git/FETCH_HEAD");
    assert!(!fetch_head.exists(), "fetch unexpectedly wrote FETCH_HEAD");
}

#[test]
fn required_checks_are_bound_to_exact_head_and_make_readiness_derived() {
    let repo = fixture();
    fs::write(repo.path().join(".agentree.toml"), "checks = [{ name = \"unit\", command = [\"/bin/sh\", \"-c\", \"exit 0\"], required = true }]\n").expect("config");
    assert!(git(repo.path(), &["add", ".agentree.toml"])
        .status
        .success());
    assert!(git(repo.path(), &["commit", "-qm", "config"])
        .status
        .success());
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "checked"]).status.success());
    let checked = run(repo.path(), &["check", "checked", "--json"]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let status = run(repo.path(), &["status", "--json"]);
    let envelope: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(envelope["result"][0]["ready"], Value::Bool(true));
    let context = run(repo.path(), &["context", "checked", "--json"]);
    assert!(context.status.success());
    let context_value: Value = serde_json::from_slice(&context.stdout).expect("context JSON");
    assert_eq!(
        context_value["result"]["readiness"]["required_checks_fresh"],
        Value::Bool(true)
    );
    let worktree = PathBuf::from(
        context_value["result"]["worktree"]
            .as_str()
            .expect("worktree"),
    );
    fs::write(worktree.join("drift"), "changed\n").expect("drift");
    let stale = run(repo.path(), &["status", "--json"]);
    let stale_value: Value = serde_json::from_slice(&stale.stdout).expect("stale status JSON");
    assert_eq!(stale_value["result"][0]["ready"], Value::Bool(false));
}

#[test]
fn failed_session_spawn_does_not_poison_the_task() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "orphan"]).status.success());

    let failed = run(
        repo.path(),
        &[
            "run",
            "orphan",
            "--",
            "/definitely/missing-agentree-program",
        ],
    );
    assert!(!failed.status.success());

    let status = run(repo.path(), &["status", "--json"]);
    let envelope: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(envelope["result"][0]["session"], 0);

    let retry = run(repo.path(), &["run", "orphan", "--", "sh", "-c", "exit 0"]);
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
}

#[test]
fn required_post_checks_change_run_result() {
    let repo = fixture();
    fs::write(
        repo.path().join(".agentree.toml"),
        "[[checks]]\nname = \"must-fail\"\ncommand = [\"sh\", \"-c\", \"exit 7\"]\nrequired = true\n",
    )
    .expect("config");
    assert!(git(repo.path(), &["add", ".agentree.toml"])
        .status
        .success());
    assert!(git(repo.path(), &["commit", "-qm", "config"])
        .status
        .success());
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "post-check"]).status.success());

    let result = run(
        repo.path(),
        &[
            "--json",
            "run",
            "post-check",
            "--require-post-checks",
            "--",
            "sh",
            "-c",
            "exit 0",
        ],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("AGT-0750"));
    let envelope: Value = serde_json::from_slice(&result.stdout).expect("error JSON");
    assert_eq!(envelope["ok"], false);
    assert!(envelope["error"]
        .as_str()
        .unwrap_or_default()
        .contains("AGT-0750"));
}

#[test]
fn session_guard_uses_process_group_after_environment_is_cleared() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "parent"]).status.success());
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_agentree"));
    let script = format!(
        "unset AGENTREE_SESSION_ID AGENTREE_TASK_ID; exec {} --repository {} new escaped",
        binary.display(),
        repo.path().display()
    );
    let result = run(repo.path(), &["run", "parent", "--", "sh", "-c", &script]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("AGT-0747"));

    let status = run(repo.path(), &["status", "--json"]);
    let envelope: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(envelope["result"].as_array().expect("tasks").len(), 1);
}

#[test]
fn removed_task_branch_can_be_deleted_with_expected_oid() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    let created = run(repo.path(), &["new", "deletable", "--json"]);
    let envelope: Value = serde_json::from_slice(&created.stdout).expect("new JSON");
    let branch = envelope["result"]["branch"].as_str().expect("branch");
    assert!(run(repo.path(), &["remove", "deletable"]).status.success());
    let status = run(repo.path(), &["status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["result"][0]["worktree_exists"], false);

    let deleted = run(
        repo.path(),
        &["delete-branch", "deletable", "--yes", "--json"],
    );
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    assert!(!git(repo.path(), &["show-ref", "--verify", branch])
        .status
        .success());
}

#[test]
fn checks_enforce_timeout_and_report_the_failure() {
    let repo = fixture();
    fs::write(
        repo.path().join(".agentree.toml"),
        "[[checks]]\nname = \"timeout\"\ncommand = [\"sh\", \"-c\", \"sleep 2\"]\ntimeout_seconds = 1\noutput_limit_bytes = 1024\nrequired = true\n",
    )
    .expect("config");
    assert!(git(repo.path(), &["add", ".agentree.toml"])
        .status
        .success());
    assert!(git(repo.path(), &["commit", "-qm", "config"])
        .status
        .success());
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "timeout"]).status.success());

    let result = run(repo.path(), &["check", "timeout", "--json"]);
    assert!(!result.status.success());
    let envelope: Value = serde_json::from_slice(&result.stdout).expect("check JSON");
    assert_eq!(envelope["ok"], false);
    assert!(envelope["error"]
        .as_str()
        .unwrap_or_default()
        .contains("AGT-0726"));
}

#[test]
fn sync_continuation_requires_an_existing_rebase() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "continuation"]).status.success());

    for argument in ["--continue", "--abort"] {
        let result = run(repo.path(), &["sync", "continuation", argument]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("AGT-0731"));
    }

    let status = run(repo.path(), &["status", "--json"]);
    let envelope: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(envelope["result"][0]["state"], "active");
}

#[test]
fn oversized_check_timeout_is_rejected_without_panic() {
    let repo = fixture();
    fs::write(
        repo.path().join(".agentree.toml"),
        "[[checks]]\nname = \"huge\"\ncommand = [\"sh\", \"-c\", \"exit 0\"]\ntimeout_seconds = 9223372036854775807\nrequired = true\n",
    )
    .expect("config");
    assert!(git(repo.path(), &["add", ".agentree.toml"])
        .status
        .success());
    assert!(git(repo.path(), &["commit", "-qm", "config"])
        .status
        .success());
    assert!(run(repo.path(), &["init"]).status.success());

    let result = run(repo.path(), &["--json", "new", "huge-timeout"]);
    assert_eq!(result.status.code(), Some(2));
    let envelope: Value = serde_json::from_slice(&result.stdout).expect("error JSON");
    assert_eq!(envelope["ok"], false);
    assert!(envelope["error"]
        .as_str()
        .unwrap_or_default()
        .contains("AGT-0409"));
}

#[test]
fn failed_fetch_is_terminal_and_does_not_poison_doctor() {
    let repo = fixture();
    assert!(git(
        repo.path(),
        &[
            "remote",
            "add",
            "origin",
            "/private/tmp/no-such-agentree-remote"
        ]
    )
    .status
    .success());
    assert!(run(repo.path(), &["init"]).status.success());

    let result = run(repo.path(), &["fetch", "--remote", "origin"]);
    assert!(!result.status.success());
    let doctor = run(repo.path(), &["doctor", "--json"]);
    let envelope: Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    assert!(envelope["result"]["operations"]
        .as_array()
        .expect("operations")
        .is_empty());
}

#[test]
fn legacy_state_is_migrated_before_session_use() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    let database = repo.path().join(".git/agentree/state.sqlite3");
    let connection = rusqlite::Connection::open(&database).expect("state database");
    connection
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             ALTER TABLE sessions RENAME TO sessions_legacy;
             CREATE TABLE sessions(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), status TEXT NOT NULL, pid INTEGER, pgid INTEGER, exit_code INTEGER, started_at INTEGER NOT NULL DEFAULT (strftime('%s','now')), finished_at INTEGER);
             DROP TABLE sessions_legacy;
             UPDATE schema_meta SET version=0 WHERE id=1;
             PRAGMA foreign_keys=ON;",
        )
        .expect("legacy schema");
    assert!(run(repo.path(), &["new", "migrated"]).status.success());
    let result = run(
        repo.path(),
        &["run", "migrated", "--", "sh", "-c", "exit 0"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let version: i64 = connection
        .query_row("SELECT version FROM schema_meta WHERE id=1", [], |row| {
            row.get(0)
        })
        .expect("schema version");
    assert_eq!(version, 2);
}

#[cfg(unix)]
#[test]
fn doctor_reconciles_a_supervisor_orphan() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "orphaned"]).status.success());
    let child_file = repo.path().join("child.pid");
    let script = format!("echo $$ > {}; sleep 5", child_file.display());
    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_agentree"))
        .arg("--repository")
        .arg(repo.path())
        .args(["run", "orphaned", "--", "sh", "-c", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("supervisor");
    for _ in 0..50 {
        if child_file.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(child_file.exists(), "child did not start");
    unsafe {
        libc::kill(supervisor.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = supervisor.wait();
    thread::sleep(Duration::from_millis(100));

    let doctor = run(repo.path(), &["doctor", "--json"]);
    let envelope: Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    let session = &envelope["result"]["sessions"][0];
    assert_eq!(session["state"], "orphaned");
    let session_id = session["session_id"].as_str().expect("session id");
    let fingerprint = session["plan_fingerprint"].as_str().expect("fingerprint");
    let applied = run(
        repo.path(),
        &[
            "doctor",
            "--session",
            session_id,
            "--apply",
            "--plan-fingerprint",
            fingerprint,
        ],
    );
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let status = run(repo.path(), &["status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["result"][0]["session"], 0);
    let retry = run(
        repo.path(),
        &["run", "orphaned", "--", "sh", "-c", "exit 0"],
    );
    assert!(retry.status.success());
}

#[test]
fn doctor_reconciles_checkpoint_after_anchor_ref_failure() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "checkpoint"]).status.success());
    let failed = run_with_env(
        repo.path(),
        "AGENTREE_FAILPOINT",
        "checkpoint.after_anchor_ref",
        &["checkpoint", "create", "checkpoint"],
    );
    assert_eq!(failed.status.code(), Some(90));

    let doctor = run(repo.path(), &["doctor", "--json"]);
    let envelope: Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    let operation = envelope["result"]["operations"][0]["operation_id"]
        .as_str()
        .expect("operation id");
    let fingerprint = envelope["result"]["operations"][0]["plan_fingerprint"]
        .as_str()
        .expect("fingerprint");
    let applied = run(
        repo.path(),
        &[
            "doctor",
            "--operation",
            operation,
            "--apply",
            "--plan-fingerprint",
            fingerprint,
        ],
    );
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let checkpoints = run(repo.path(), &["checkpoint", "list", "checkpoint", "--json"]);
    let checkpoints: Value = serde_json::from_slice(&checkpoints.stdout).expect("checkpoint JSON");
    assert_eq!(
        checkpoints["result"].as_array().expect("checkpoints").len(),
        1
    );
}

#[test]
fn doctor_reconciles_task_creation_after_worktree_add_failure() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    let failed = run_with_env(
        repo.path(),
        "AGENTREE_FAILPOINT",
        "task_create.after_worktree_add",
        &["new", "interrupted"],
    );
    assert_eq!(failed.status.code(), Some(90));

    let doctor = run(repo.path(), &["doctor", "--json"]);
    let envelope: Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    let operation = envelope["result"]["operations"][0]["operation_id"]
        .as_str()
        .expect("operation id");
    let fingerprint = envelope["result"]["operations"][0]["plan_fingerprint"]
        .as_str()
        .expect("fingerprint");
    let applied = run(
        repo.path(),
        &[
            "doctor",
            "--operation",
            operation,
            "--apply",
            "--plan-fingerprint",
            fingerprint,
        ],
    );
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let status = run(repo.path(), &["status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert!(status["result"]
        .as_array()
        .expect("tasks")
        .iter()
        .any(|task| task["slug"] == "interrupted" && task["state"] == "active"));
}

#[test]
fn doctor_reconciles_restore_after_index_replacement_failure() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "source"]).status.success());
    let checkpoint = run(repo.path(), &["--json", "checkpoint", "source"]);
    let checkpoint: Value = serde_json::from_slice(&checkpoint.stdout).expect("checkpoint JSON");
    let checkpoint_id = checkpoint["result"]["id"].as_str().expect("checkpoint id");
    let failed = run_in_with_env(
        repo.path(),
        "AGENTREE_FAILPOINT",
        "restore.after_index_replace",
        &[
            "checkpoint",
            "restore",
            checkpoint_id,
            "--to-new-task",
            "recovered",
        ],
    );
    assert_eq!(failed.status.code(), Some(90));

    let doctor = run(repo.path(), &["doctor", "--json"]);
    let envelope: Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    let operation = envelope["result"]["operations"][0]["operation_id"]
        .as_str()
        .expect("operation id");
    let fingerprint = envelope["result"]["operations"][0]["plan_fingerprint"]
        .as_str()
        .expect("fingerprint");
    let applied = run(
        repo.path(),
        &[
            "doctor",
            "--operation",
            operation,
            "--apply",
            "--plan-fingerprint",
            fingerprint,
        ],
    );
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let status = run(repo.path(), &["status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["result"].as_array().expect("tasks").len(), 2);
    assert!(status["result"]
        .as_array()
        .expect("tasks")
        .iter()
        .any(|task| task["slug"] == "recovered" && task["state"] == "active"));
}

#[test]
fn doctor_reconciles_land_after_fast_forward_failure() {
    let repo = fixture();
    assert!(run(repo.path(), &["init"]).status.success());
    assert!(run(repo.path(), &["new", "land", "--json"])
        .status
        .success());
    let context = run(repo.path(), &["context", "land", "--json"]);
    let context: Value = serde_json::from_slice(&context.stdout).expect("context JSON");
    let worktree = PathBuf::from(context["result"]["worktree"].as_str().expect("worktree"));
    fs::write(worktree.join("landed"), "landed\n").expect("landed file");
    assert!(git(&worktree, &["add", "landed"]).status.success());
    assert!(git(&worktree, &["commit", "-qm", "land"]).status.success());

    let failed = run_in_with_env(
        repo.path(),
        "AGENTREE_FAILPOINT",
        "land.after_fast_forward",
        &["land", "land", "--into-current"],
    );
    assert_eq!(failed.status.code(), Some(90));

    let doctor = run(repo.path(), &["doctor", "--json"]);
    let envelope: Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    let operation = envelope["result"]["operations"][0]["operation_id"]
        .as_str()
        .expect("operation id");
    let fingerprint = envelope["result"]["operations"][0]["plan_fingerprint"]
        .as_str()
        .expect("fingerprint");
    let applied = run(
        repo.path(),
        &[
            "doctor",
            "--operation",
            operation,
            "--apply",
            "--plan-fingerprint",
            fingerprint,
        ],
    );
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let remaining = run(repo.path(), &["doctor", "--json"]);
    let remaining: Value = serde_json::from_slice(&remaining.stdout).expect("doctor JSON");
    assert!(remaining["result"]["operations"]
        .as_array()
        .expect("operations")
        .is_empty());
}
