use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_git-bulk-clean"))
}

fn create_temp_dir(name: &str) -> PathBuf {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    for _ in 0..100 {
        let path = std::env::temp_dir().join(format!(
            "git_bulk_clean_cli_{name}_{}_{}_{}",
            std::process::id(),
            timestamp,
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("failed to create temporary directory: {error}"),
        }
    }
    panic!("failed to allocate a unique temporary directory")
}

fn temp_repo() -> PathBuf {
    let path = create_temp_dir("repo");
    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(&path)
            .status()
            .unwrap()
            .success()
    );
    path
}

fn temp_dir(name: &str) -> PathBuf {
    create_temp_dir(name)
}

fn ghq_stub(dir: &Path, body: &str) {
    let path = dir.join("ghq");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn path_with(dir: &Path) -> String {
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[test]
fn help_exits_zero() {
    assert_eq!(binary().arg("--help").status().unwrap().code(), Some(0));
}

#[test]
fn invalid_argument_exits_two() {
    assert_eq!(
        binary().arg("--not-a-real-option").status().unwrap().code(),
        Some(2)
    );
}

#[test]
fn failed_maintenance_exits_one() {
    let repo = temp_repo();
    assert!(
        Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "/definitely/missing/git/repository"
            ])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    let status = binary()
        .env("MAINTENANCE_REPOS", &repo)
        .env("MAINTENANCE_SKIP_LFS", "true")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn failed_ghq_discovery_exits_one() {
    let stubs = temp_dir("failed_ghq");
    ghq_stub(&stubs, "exit 23");

    let output = binary()
        .env("PATH", path_with(&stubs))
        .env("MAINTENANCE_GHQ_ENABLE", "true")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("repository discovery failed"));
    let _ = fs::remove_dir_all(stubs);
}

#[test]
fn list_with_successful_ghq_exits_zero() {
    let repo = temp_repo();
    let stubs = temp_dir("successful_ghq");
    ghq_stub(&stubs, &format!("printf '%s\\n' '{}'", repo.display()));

    let output = binary()
        .arg("--list")
        .env("PATH", path_with(&stubs))
        .env("MAINTENANCE_GHQ_ENABLE", "true")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("norm  {}\n", repo.canonicalize().unwrap().display())
    );
    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(stubs);
}

#[test]
fn parent_git_config_injection_does_not_break_repository_detection() {
    let repo = temp_repo();
    let output = binary()
        .arg("--list")
        .env("MAINTENANCE_REPOS", &repo)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "true")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("norm  {}\n", repo.canonicalize().unwrap().display())
    );
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn one_repo_dry_run_output_contract() {
    let repo = temp_repo();
    let output = binary()
        .arg("--dry-run")
        .env("MAINTENANCE_REPOS", &repo)
        .env("MAINTENANCE_SKIP_LFS", "true")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("starting cycle: 1 repositories (0 bare), 1 worker"));
    assert!(stderr.contains("[1/1] cleaning:"));
    assert!(stderr.contains("git worktree prune"));
    assert!(stderr.contains("incremental-repack (if a pack is available)"));
    assert!(stderr.contains("cycle complete"));
    assert!(stderr.contains("1/1 ok, 0 failed"));
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn prune_worktrees_dry_run_output_contract() {
    let repo = temp_repo();
    let output = binary()
        .arg("--dry-run")
        .env("MAINTENANCE_REPOS", &repo)
        .env("MAINTENANCE_SKIP_LFS", "true")
        .env("MAINTENANCE_PRUNE_WORKTREES", "true")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("git worktree remove -- (worktrees merged into mainline or idle 3+ days)")
    );
    assert!(stderr.contains("1/1 ok, 0 failed"));
    let _ = fs::remove_dir_all(repo);
}

#[test]
fn prune_worktrees_disabled_by_default_dry_run_output_contract() {
    let repo = temp_repo();
    let output = binary()
        .arg("--dry-run")
        .env("MAINTENANCE_REPOS", &repo)
        .env("MAINTENANCE_SKIP_LFS", "true")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("git worktree remove --"));
    let _ = fs::remove_dir_all(repo);
}
