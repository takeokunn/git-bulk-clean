use std::collections::HashSet;
use std::env;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NUM_WORKERS: usize = 5;
const DEFAULT_REFLOG_EXPIRE: &str = "30.days.ago";
const DEFAULT_INTERVAL_SECS: u64 = 86400;
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Config {
    repos: Vec<String>,
    ghq_enable: bool,
    reflog_expire: String,
    aggressive: bool,
    interval_secs: u64,
}

impl Config {
    fn from_env() -> Self {
        Self::from_vars(|name| env::var(name))
    }

    fn from_vars<F>(get_var: F) -> Self
    where
        F: Fn(&str) -> Result<String, env::VarError>,
    {
        let repos = get_var("MAINTENANCE_REPOS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let ghq_enable = get_var("MAINTENANCE_GHQ_ENABLE")
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let reflog_expire = get_var("MAINTENANCE_REFLOG_EXPIRE")
            .unwrap_or_else(|_| DEFAULT_REFLOG_EXPIRE.to_string());

        let aggressive = get_var("MAINTENANCE_AGGRESSIVE")
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let interval_secs = get_var("MAINTENANCE_INTERVAL")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_INTERVAL_SECS);

        Config {
            repos,
            ghq_enable,
            reflog_expire,
            aggressive,
            interval_secs,
        }
    }
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = ((secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

fn log(msg: &str) {
    eprintln!("[git-bulk-clean {}] {msg}", timestamp());
}

fn collect_ghq_repos() -> Vec<String> {
    let output = Command::new("ghq")
        .args(["list", "-p"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Ok(_) => {
            log("warning: ghq list failed");
            vec![]
        }
        Err(e) => {
            log(&format!("warning: could not run ghq: {e}"));
            vec![]
        }
    }
}

fn collect_repos(config: &Config) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();

    for path in &config.repos {
        seen.insert(path.clone());
    }

    if config.ghq_enable {
        for path in collect_ghq_repos() {
            seen.insert(path);
        }
    }

    let mut repos: Vec<String> = seen
        .into_iter()
        .filter(|p| Path::new(p).is_dir())
        .collect();
    repos.sort();
    repos
}

fn run_git(dir: &str, args: &[&str]) -> bool {
    let result = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();

    match result {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            log(&format!(
                "{dir}: `git {}` failed: {stderr}",
                args.join(" ")
            ));
            false
        }
        Err(e) => {
            log(&format!("{dir}: `git {}` error: {e}", args.join(" ")));
            false
        }
    }
}

fn clean_repo(dir: &str, reflog_expire: &str, aggressive: bool, dry_run: bool) -> bool {
    log(&format!("cleaning: {dir}"));

    if dry_run {
        log(&format!("  (dry-run) git fetch --all --prune"));
        log(&format!("  (dry-run) git worktree prune"));
        log(&format!("  (dry-run) git reflog expire --expire={reflog_expire} --all"));
        if aggressive {
            log(&format!("  (dry-run) git gc --aggressive"));
        } else {
            log(&format!("  (dry-run) git gc --auto"));
        }
        log(&format!("  (dry-run) git maintenance run --task=commit-graph"));
        return true;
    }

    let ok = run_git(dir, &["fetch", "--all", "--prune"])
        & run_git(dir, &["worktree", "prune"])
        & run_git(
            dir,
            &[
                "reflog",
                "expire",
                &format!("--expire={reflog_expire}"),
                "--all",
            ],
        )
        & if aggressive {
            run_git(dir, &["gc", "--aggressive"])
        } else {
            run_git(dir, &["gc", "--auto"])
        }
        & run_git(dir, &["maintenance", "run", "--task=commit-graph"]);

    ok
}

struct CycleResult {
    #[allow(dead_code)]
    total: usize,
    #[allow(dead_code)]
    succeeded: usize,
    failed: usize,
}

fn run_cycle(config: &Config, dry_run: bool) -> CycleResult {
    let repos = collect_repos(config);
    let total = repos.len();

    if total == 0 {
        log("no repositories found");
        return CycleResult { total: 0, succeeded: 0, failed: 0 };
    }

    log(&format!("starting cycle: {total} repositories"));

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let rx = Arc::new(Mutex::new(rx));

    let succeeded = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));

    for repo in repos {
        tx.send(repo).expect("channel send");
    }
    drop(tx);

    let reflog_expire = Arc::new(config.reflog_expire.clone());
    let aggressive = config.aggressive;

    let mut handles = Vec::with_capacity(NUM_WORKERS);
    for worker_id in 0..NUM_WORKERS {
        let rx = Arc::clone(&rx);
        let reflog_expire = Arc::clone(&reflog_expire);
        let succeeded = Arc::clone(&succeeded);
        let failed = Arc::clone(&failed);

        let handle = thread::spawn(move || loop {
            let repo = {
                let guard = rx.lock().expect("mutex lock");
                guard.recv()
            };
            match repo {
                Ok(dir) => {
                    if clean_repo(&dir, &reflog_expire, aggressive, dry_run) {
                        succeeded.fetch_add(1, Ordering::Relaxed);
                    } else {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(_) => {
                    log(&format!("worker {worker_id}: done"));
                    break;
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("worker thread panicked");
    }

    let succeeded = succeeded.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    log(&format!(
        "cycle complete: {succeeded}/{total} succeeded, {failed} failed"
    ));

    CycleResult { total, succeeded, failed }
}

fn print_help(prog: &str) {
    eprintln!("Usage: {prog} [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --daemon      Run continuously, sleeping MAINTENANCE_INTERVAL seconds between cycles");
    eprintln!("  --dry-run     Print what would be done without executing any git commands");
    eprintln!("  --version     Print version and exit");
    eprintln!("  -h, --help    Print this help message and exit");
    eprintln!();
    eprintln!("Environment variables:");
    eprintln!("  MAINTENANCE_REPOS          Comma-separated list of repository paths");
    eprintln!("  MAINTENANCE_GHQ_ENABLE     Set to 'true' to include all ghq-managed repos");
    eprintln!("  MAINTENANCE_REFLOG_EXPIRE  Reflog expiry (default: {DEFAULT_REFLOG_EXPIRE})");
    eprintln!("  MAINTENANCE_AGGRESSIVE     Set to 'true' to run 'git gc --aggressive'");
    eprintln!("  MAINTENANCE_INTERVAL       Daemon sleep interval in seconds (default: {DEFAULT_INTERVAL_SECS})");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("git-bulk-clean");

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help(prog);
        return;
    }

    if args.iter().any(|a| a == "--version" || a == "-V") {
        eprintln!("git-bulk-clean {VERSION}");
        return;
    }

    let daemon_mode = args.iter().any(|a| a == "--daemon");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    if dry_run {
        log("dry-run mode: no git commands will be executed");
    }

    let config = Config::from_env();

    if daemon_mode {
        log(&format!(
            "daemon mode started (interval={}s)",
            config.interval_secs
        ));
        loop {
            run_cycle(&config, dry_run);
            log(&format!(
                "sleeping {}s until next cycle",
                config.interval_secs
            ));
            thread::sleep(Duration::from_secs(config.interval_secs));
        }
    } else {
        let result = run_cycle(&config, dry_run);
        if result.failed > 0 {
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::collections::HashSet;

    fn dedup_and_sort(lists: Vec<Vec<String>>) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        for list in lists {
            for item in list {
                seen.insert(item);
            }
        }
        let mut result: Vec<String> = seen.into_iter().collect();
        result.sort();
        result
    }

    fn make_config(vars: &[(&str, &str)]) -> Config {
        let map: HashMap<&str, &str> = vars.iter().cloned().collect();
        Config::from_vars(|name| {
            map.get(name)
                .map(|v| v.to_string())
                .ok_or(env::VarError::NotPresent)
        })
    }

    #[test]
    fn test_dedup_empty() {
        let result = dedup_and_sort(vec![vec![], vec![]]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_dedup_no_overlap() {
        let result = dedup_and_sort(vec![
            vec!["/a/repo1".to_string(), "/a/repo2".to_string()],
            vec!["/b/repo3".to_string()],
        ]);
        assert_eq!(result, vec!["/a/repo1", "/a/repo2", "/b/repo3"]);
    }

    #[test]
    fn test_dedup_full_overlap() {
        let result = dedup_and_sort(vec![
            vec!["/repo".to_string(), "/repo2".to_string()],
            vec!["/repo".to_string(), "/repo2".to_string()],
        ]);
        assert_eq!(result, vec!["/repo", "/repo2"]);
    }

    #[test]
    fn test_dedup_partial_overlap() {
        let result = dedup_and_sort(vec![
            vec!["/a".to_string(), "/b".to_string()],
            vec!["/b".to_string(), "/c".to_string()],
        ]);
        assert_eq!(result, vec!["/a", "/b", "/c"]);
    }

    #[test]
    fn test_dedup_sorted_output() {
        let result = dedup_and_sort(vec![
            vec!["/z".to_string(), "/a".to_string()],
            vec!["/m".to_string()],
        ]);
        assert_eq!(result, vec!["/a", "/m", "/z"]);
    }

    #[test]
    fn test_config_defaults() {
        let config = make_config(&[]);
        assert!(config.repos.is_empty());
        assert!(!config.ghq_enable);
        assert_eq!(config.reflog_expire, DEFAULT_REFLOG_EXPIRE);
        assert!(!config.aggressive);
        assert_eq!(config.interval_secs, DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn test_config_all_values() {
        let config = make_config(&[
            ("MAINTENANCE_REPOS", "/repo/a, /repo/b , /repo/c"),
            ("MAINTENANCE_GHQ_ENABLE", "true"),
            ("MAINTENANCE_REFLOG_EXPIRE", "7.days.ago"),
            ("MAINTENANCE_AGGRESSIVE", "true"),
            ("MAINTENANCE_INTERVAL", "3600"),
        ]);
        assert_eq!(config.repos, vec!["/repo/a", "/repo/b", "/repo/c"]);
        assert!(config.ghq_enable);
        assert_eq!(config.reflog_expire, "7.days.ago");
        assert!(config.aggressive);
        assert_eq!(config.interval_secs, 3600);
    }

    #[test]
    fn test_config_repos_empty_strings_filtered() {
        let config = make_config(&[("MAINTENANCE_REPOS", "/a,,  ,/b")]);
        assert_eq!(config.repos, vec!["/a", "/b"]);
    }

    #[test]
    fn test_config_interval_invalid_falls_back_to_default() {
        let config = make_config(&[("MAINTENANCE_INTERVAL", "not_a_number")]);
        assert_eq!(config.interval_secs, DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn test_config_ghq_case_insensitive() {
        let config = make_config(&[("MAINTENANCE_GHQ_ENABLE", "TRUE")]);
        assert!(config.ghq_enable);
        let config2 = make_config(&[("MAINTENANCE_GHQ_ENABLE", "True")]);
        assert!(config2.ghq_enable);
        let config3 = make_config(&[("MAINTENANCE_GHQ_ENABLE", "false")]);
        assert!(!config3.ghq_enable);
    }

    #[test]
    fn test_collect_repos_deduplicates_and_filters_missing() {
        use std::fs;
        let tmp = env::temp_dir().join("git_bulk_clean_test");
        let _ = fs::create_dir_all(&tmp);

        let real_path = tmp.to_string_lossy().to_string();
        // same path twice + nonexistent
        let config = make_config(&[(
            "MAINTENANCE_REPOS",
            &format!("{real_path},{real_path},/does-not-exist-xyz"),
        )]);
        let repos = collect_repos(&config);
        assert_eq!(repos, vec![real_path]);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_timestamp_format() {
        let ts = timestamp();
        // HH:MM:SS
        assert_eq!(ts.len(), 8);
        assert_eq!(&ts[2..3], ":");
        assert_eq!(&ts[5..6], ":");
    }
}
