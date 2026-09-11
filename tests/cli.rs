use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
            // Overrides the developer's global hooksPath/signing config so
            // setup does not depend on what is installed on this machine.
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
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

fn resolve_real_git() -> PathBuf {
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git not found on PATH")
}

// Intercepts every git invocation, appends its argv and GIT_CONFIG_* env to
// `log` as one record delimited by a "---" line, then execs the real git so
// the maintenance run still completes normally.
fn git_stub(dir: &Path, real_git: &Path, log: &Path) {
    let path = dir.join("git");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\n{{\n  printf '%s\\n' \"$*\"\n  env | grep '^GIT_CONFIG_'\n  printf -- '---\\n'\n}} >> \"{log}\"\nexec \"{real_git}\" \"$@\"\n",
            log = log.display(),
            real_git = real_git.display(),
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

// A git stub that never execs the real binary: it records its own pid to
// `marker` (so the test can tell it has actually started, and later confirm
// the pid is gone rather than merely that a file was created) and then
// sleeps, standing in for a long-running `git fetch` the daemon must be able
// to kill on SIGTERM/SIGINT.
fn sleeping_git_stub(dir: &Path, marker: &Path) {
    let path = dir.join("git");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\necho $$ > \"{marker}\"\nexec sleep 30\n",
            marker = marker.display(),
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn git_stub_records(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .split("---\n")
        .map(str::to_string)
        .filter(|record| !record.trim().is_empty())
        .collect()
}

fn find_git_stub_record<'a>(records: &'a [String], argv: &str) -> &'a str {
    records
        .iter()
        .find(|record| record.lines().next() == Some(argv))
        .unwrap_or_else(|| panic!("no recorded git invocation {argv:?} among {records:?}"))
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

#[test]
fn credential_helpers_env_var_lets_only_remote_phases_use_configured_helpers() {
    let repo = temp_repo();
    let stubs = temp_dir("credential_helpers_on");
    let real_git = resolve_real_git();
    let log = stubs.join("git-stub.log");
    git_stub(&stubs, &real_git, &log);

    let status = binary()
        .env("PATH", path_with(&stubs))
        .env("MAINTENANCE_REPOS", &repo)
        .env("MAINTENANCE_SKIP_LFS", "true")
        .env("MAINTENANCE_CREDENTIAL_HELPERS", "true")
        .status()
        .unwrap();
    assert!(status.success());

    let records = git_stub_records(&log);
    let fetch = find_git_stub_record(&records, "fetch --all --prune");
    assert!(
        !fetch.contains("credential.helper"),
        "fetch must not reset credential.helper when opted in: {fetch}"
    );
    let pack_refs = find_git_stub_record(&records, "pack-refs --all");
    assert!(
        pack_refs.contains("credential.helper"),
        "a local phase must still reset credential.helper: {pack_refs}"
    );

    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(stubs);
}

#[test]
fn credential_helpers_env_var_defaults_to_resetting_helper_on_fetch() {
    let repo = temp_repo();
    let stubs = temp_dir("credential_helpers_off");
    let real_git = resolve_real_git();
    let log = stubs.join("git-stub.log");
    git_stub(&stubs, &real_git, &log);

    let status = binary()
        .env("PATH", path_with(&stubs))
        .env("MAINTENANCE_REPOS", &repo)
        .env("MAINTENANCE_SKIP_LFS", "true")
        .status()
        .unwrap();
    assert!(status.success());

    let records = git_stub_records(&log);
    let fetch = find_git_stub_record(&records, "fetch --all --prune");
    assert!(
        fetch.contains("credential.helper"),
        "fetch must reset credential.helper by default: {fetch}"
    );

    let _ = fs::remove_dir_all(repo);
    let _ = fs::remove_dir_all(stubs);
}

// Ensures the daemon child and, best-effort, the stub it may have spawned
// are cleaned up even when an assertion above panics mid-test — otherwise a
// failing assertion leaks a sleeping `sleep 30` process and two temp
// directories instead of just failing the one test.
struct KillOnDrop {
    child: Child,
    repo: PathBuf,
    stubs: PathBuf,
    marker: PathBuf,
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(contents) = fs::read_to_string(&self.marker) {
            if let Ok(stub_pid) = contents.trim().parse::<u32>() {
                let _ = Command::new("kill")
                    .args(["-TERM", &stub_pid.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _ = fs::remove_dir_all(&self.repo);
        let _ = fs::remove_dir_all(&self.stubs);
    }
}

fn signal_stops_daemon_and_child(signal: &str) {
    let repo = temp_repo();
    let stubs = temp_dir("signal_stub");
    let marker = stubs.join("stub.pid");
    sleeping_git_stub(&stubs, &marker);

    let child = binary()
        .env("PATH", path_with(&stubs))
        .env("MAINTENANCE_REPOS", &repo)
        .spawn()
        .unwrap();
    let daemon_pid = child.id();
    let mut guard = KillOnDrop {
        child,
        repo,
        stubs,
        marker: marker.clone(),
    };

    // Wait for the stub to actually be running before signalling — sending
    // the signal before the child git process exists would prove nothing
    // about stopping it.
    let start_deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(Instant::now() < start_deadline, "git stub never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    let stub_pid: u32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();

    assert!(
        Command::new("kill")
            .args([signal, &daemon_pid.to_string()])
            .status()
            .unwrap()
            .success()
    );

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if guard.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < exit_deadline,
            "daemon did not exit within 2s of {signal}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // `ps` is not on PATH in the Nix build sandbox (only coreutils and the
    // package's declared inputs are), so liveness is checked with `kill -0`
    // instead: it sends no signal, only reports whether the pid still
    // resolves to a process. A brief window after the daemon's own exit is
    // expected before the kernel finishes delivering the forwarded SIGTERM
    // and reparenting/reaping the stub, so this polls for up to 500ms rather
    // than checking once. `.stderr(null)` suppresses the "No such process"
    // message `kill -0` prints once the pid is gone, which is the expected
    // and desired outcome here, not an error.
    let liveness_deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let still_running = Command::new("kill")
            .args(["-0", &stub_pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        if !still_running.success() {
            break;
        }
        assert!(
            Instant::now() < liveness_deadline,
            "git stub (pid {stub_pid}) is still running 500ms after the daemon exited"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sigterm_kills_git_child_and_daemon_exits_promptly() {
    signal_stops_daemon_and_child("-TERM");
}

#[test]
fn sigint_kills_git_child_and_daemon_exits_promptly() {
    signal_stops_daemon_and_child("-INT");
}
