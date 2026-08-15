use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
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
