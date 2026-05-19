use std::collections::HashSet;
use std::env;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ── constants ────────────────────────────────────────────────────────────────

const NUM_WORKERS: usize = 5;
const DEFAULT_REFLOG_EXPIRE: &str = "30.days.ago";
const DEFAULT_INTERVAL_SECS: u64 = 86400;
const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── config ───────────────────────────────────────────────────────────────────

struct Config {
    repos: Vec<String>,
    ghq_enable: bool,
    reflog_expire: String,
    aggressive: bool,
    interval_secs: u64,
    skip_submodules: bool,
    skip_lfs: bool,
}

impl Config {
    fn from_env() -> Self {
        Self::from_vars(|k| env::var(k))
    }

    fn from_vars<F>(get: F) -> Self
    where
        F: Fn(&str) -> Result<String, env::VarError>,
    {
        let repos = get("MAINTENANCE_REPOS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Config {
            repos,
            ghq_enable: bool_var(&get, "MAINTENANCE_GHQ_ENABLE", false),
            reflog_expire: get("MAINTENANCE_REFLOG_EXPIRE")
                .unwrap_or_else(|_| DEFAULT_REFLOG_EXPIRE.to_string()),
            aggressive: bool_var(&get, "MAINTENANCE_AGGRESSIVE", false),
            interval_secs: get("MAINTENANCE_INTERVAL")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(DEFAULT_INTERVAL_SECS),
            skip_submodules: bool_var(&get, "MAINTENANCE_SKIP_SUBMODULES", false),
            skip_lfs: bool_var(&get, "MAINTENANCE_SKIP_LFS", false),
        }
    }
}

fn bool_var<F>(get: &F, key: &str, default: bool) -> bool
where
    F: Fn(&str) -> Result<String, env::VarError>,
{
    get(key)
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(default)
}

// ── logging ──────────────────────────────────────────────────────────────────

fn log(msg: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = ((secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    eprintln!("[git-bulk-clean {h:02}:{m:02}:{s:02}] {msg}");
}

// ── repo collection ──────────────────────────────────────────────────────────

fn ghq_repos() -> Vec<String> {
    Command::new("ghq")
        .args(["list", "-p"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_else(|| {
            log("warning: ghq list failed or ghq not found");
            vec![]
        })
}

fn collect_repos(cfg: &Config) -> Vec<String> {
    let mut seen: HashSet<String> = cfg.repos.iter().cloned().collect();
    if cfg.ghq_enable {
        seen.extend(ghq_repos());
    }
    let mut repos: Vec<String> = seen
        .into_iter()
        .filter(|p| Path::new(p).is_dir())
        .collect();
    repos.sort();
    repos
}

// ── git command helpers ───────────────────────────────────────────────────────

fn git(dir: &str, args: &[&str]) -> bool {
    match Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr);
            let msg = msg.trim();
            if !msg.is_empty() {
                log(&format!("{dir}: `git {}` — {msg}", args.join(" ")));
            }
            false
        }
        Err(e) => {
            log(&format!("{dir}: `git {}` — {e}", args.join(" ")));
            false
        }
    }
}

fn git_lfs(dir: &str, args: &[&str]) -> bool {
    match Command::new("git-lfs")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr);
            let msg = msg.trim();
            if !msg.is_empty() {
                log(&format!("{dir}: `git lfs {}` — {msg}", args.join(" ")));
            }
            false
        }
        Err(e) => {
            log(&format!("{dir}: `git lfs {}` — {e}", args.join(" ")));
            false
        }
    }
}

// ── repo feature detection ────────────────────────────────────────────────────

fn has_submodules(dir: &str) -> bool {
    Path::new(dir).join(".gitmodules").exists()
}

fn has_lfs(dir: &str) -> bool {
    // git config --local filter.lfs.clean is set iff git-lfs was ever enabled here
    Command::new("git")
        .args(["config", "--local", "--get-regexp", "filter\\.lfs\\."])
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── cleanup phases ────────────────────────────────────────────────────────────

fn phase_fetch(dir: &str) -> bool {
    // --prune-tags removes tags deleted on the remote; --prune handles branches
    git(dir, &["fetch", "--all", "--prune", "--prune-tags"])
}

fn phase_refs(dir: &str, reflog_expire: &str) -> bool {
    // Pack all loose refs into packed-refs for faster ref lookup
    let ok = git(dir, &["pack-refs", "--all"]);
    let ok = ok & git(dir, &["worktree", "prune"]);
    ok & git(
        dir,
        &["reflog", "expire", &format!("--expire={reflog_expire}"), "--all"],
    )
}

fn phase_objects_normal(dir: &str) -> bool {
    // loose-objects and incremental-repack run unconditionally (unlike gc --auto
    // which skips when below thresholds), then gc finalises pruning
    let ok = git(dir, &["maintenance", "run", "--task=loose-objects"]);
    let ok = ok & git(dir, &["maintenance", "run", "--task=incremental-repack"]);
    ok & git(dir, &["gc", "--auto"])
}

fn phase_objects_aggressive(dir: &str) -> bool {
    // Full repack with delta re-computation, then aggressive gc
    // -f: ignore existing deltas; --delta-base-offset: smaller pack files
    let ok = git(dir, &["repack", "-a", "-d", "-f", "--delta-base-offset"]);
    ok & git(dir, &["gc", "--aggressive", "--prune=all"])
}

fn phase_indices(dir: &str) -> bool {
    git(dir, &["maintenance", "run", "--task=commit-graph"])
}

fn phase_submodules(dir: &str) -> bool {
    // Sync remote URLs then run gc on each submodule
    let ok = git(dir, &["submodule", "sync", "--recursive"]);
    ok & git(
        dir,
        &["submodule", "foreach", "--recursive", "git", "gc", "--auto"],
    )
}

fn phase_lfs(dir: &str) -> bool {
    // Remove LFS objects not referenced by any reachable commit
    git_lfs(dir, &["prune"])
}

// ── per-repo cleanup orchestration ───────────────────────────────────────────

fn clean_repo(dir: &str, cfg: &Config, dry_run: bool) -> bool {
    let t = Instant::now();
    log(&format!("cleaning: {dir}"));

    if dry_run {
        log(&format!("  (dry-run) git fetch --all --prune --prune-tags"));
        log(&format!("  (dry-run) git pack-refs --all"));
        log(&format!("  (dry-run) git worktree prune"));
        log(&format!(
            "  (dry-run) git reflog expire --expire={} --all",
            cfg.reflog_expire
        ));
        log(&format!("  (dry-run) git maintenance run --task=loose-objects"));
        log(&format!(
            "  (dry-run) git maintenance run --task=incremental-repack"
        ));
        if cfg.aggressive {
            log(&format!(
                "  (dry-run) git repack -a -d -f --delta-base-offset"
            ));
            log(&format!("  (dry-run) git gc --aggressive --prune=all"));
        } else {
            log(&format!("  (dry-run) git gc --auto"));
        }
        log(&format!(
            "  (dry-run) git maintenance run --task=commit-graph"
        ));
        if !cfg.skip_submodules && has_submodules(dir) {
            log(&format!("  (dry-run) git submodule sync --recursive"));
            log(&format!(
                "  (dry-run) git submodule foreach --recursive git gc --auto"
            ));
        }
        if !cfg.skip_lfs && has_lfs(dir) {
            log(&format!("  (dry-run) git lfs prune"));
        }
        return true;
    }

    let ok = phase_fetch(dir)
        & phase_refs(dir, &cfg.reflog_expire)
        & if cfg.aggressive {
            phase_objects_aggressive(dir)
        } else {
            phase_objects_normal(dir)
        }
        & phase_indices(dir);

    let ok = if !cfg.skip_submodules && has_submodules(dir) {
        ok & phase_submodules(dir)
    } else {
        ok
    };

    let ok = if !cfg.skip_lfs && has_lfs(dir) {
        ok & phase_lfs(dir)
    } else {
        ok
    };

    let ms = t.elapsed().as_millis();
    log(&format!(
        "{dir}: done in {ms}ms ({})",
        if ok { "ok" } else { "some errors" }
    ));
    ok
}

// ── worker pool ───────────────────────────────────────────────────────────────

struct CycleStats {
    #[allow(dead_code)]
    total: usize,
    failed: usize,
}

fn run_cycle(cfg: &Config, dry_run: bool) -> CycleStats {
    let repos = collect_repos(cfg);
    let total = repos.len();

    if total == 0 {
        log("no repositories found");
        return CycleStats { total: 0, failed: 0 };
    }

    log(&format!("starting cycle: {total} repositories, {NUM_WORKERS} workers"));

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let rx = Arc::new(Mutex::new(rx));
    let failed_count = Arc::new(AtomicUsize::new(0));

    for repo in repos {
        tx.send(repo).unwrap();
    }
    drop(tx);

    let reflog_expire = Arc::new(cfg.reflog_expire.clone());
    let aggressive = cfg.aggressive;
    let skip_submodules = cfg.skip_submodules;
    let skip_lfs = cfg.skip_lfs;

    let handles: Vec<_> = (0..NUM_WORKERS)
        .map(|id| {
            let rx = Arc::clone(&rx);
            let failed_count = Arc::clone(&failed_count);
            let reflog_expire = Arc::clone(&reflog_expire);

            thread::spawn(move || {
                let cfg = Config {
                    repos: vec![],
                    ghq_enable: false,
                    reflog_expire: (*reflog_expire).clone(),
                    aggressive,
                    interval_secs: DEFAULT_INTERVAL_SECS,
                    skip_submodules,
                    skip_lfs,
                };
                loop {
                    match rx.lock().unwrap().recv() {
                        Ok(dir) => {
                            if !clean_repo(&dir, &cfg, dry_run) {
                                failed_count.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(_) => {
                            log(&format!("worker {id}: done"));
                            break;
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker panicked");
    }

    let failed = failed_count.load(Ordering::Relaxed);
    let succeeded = total - failed;
    log(&format!(
        "cycle complete — {succeeded}/{total} ok, {failed} failed"
    ));

    CycleStats { total, failed }
}

// ── cli ───────────────────────────────────────────────────────────────────────

fn print_help(prog: &str) {
    eprintln!("Usage: {prog} [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --daemon      Loop forever, sleeping MAINTENANCE_INTERVAL between cycles");
    eprintln!("  --dry-run     Show what would run without executing git commands");
    eprintln!("  --version     Print version and exit");
    eprintln!("  -h, --help    Print this help and exit");
    eprintln!();
    eprintln!("Environment variables:");
    eprintln!("  MAINTENANCE_REPOS              Comma-separated repo paths");
    eprintln!("  MAINTENANCE_GHQ_ENABLE         true → include all ghq-managed repos");
    eprintln!("  MAINTENANCE_REFLOG_EXPIRE      Reflog cutoff (default: {DEFAULT_REFLOG_EXPIRE})");
    eprintln!("  MAINTENANCE_AGGRESSIVE         true → full repack + gc --aggressive");
    eprintln!("  MAINTENANCE_INTERVAL           Daemon sleep interval in seconds (default: {DEFAULT_INTERVAL_SECS})");
    eprintln!("  MAINTENANCE_SKIP_SUBMODULES    true → skip submodule cleanup");
    eprintln!("  MAINTENANCE_SKIP_LFS           true → skip git-lfs prune");
    eprintln!();
    eprintln!("Cleanup pipeline (per repo):");
    eprintln!("  1. git fetch --all --prune --prune-tags");
    eprintln!("  2. git pack-refs --all");
    eprintln!("  3. git worktree prune");
    eprintln!("  4. git reflog expire --expire=<REFLOG_EXPIRE> --all");
    eprintln!("  5. git maintenance run --task=loose-objects");
    eprintln!("  6. git maintenance run --task=incremental-repack  (normal)");
    eprintln!("     git repack -a -d -f --delta-base-offset        (aggressive)");
    eprintln!("  7. git gc --auto                                   (normal)");
    eprintln!("     git gc --aggressive --prune=all                 (aggressive)");
    eprintln!("  8. git maintenance run --task=commit-graph");
    eprintln!("  9. git submodule sync + foreach gc                 (if .gitmodules found)");
    eprintln!(" 10. git lfs prune                                   (if LFS configured)");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("git-bulk-clean");

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help(prog);
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        eprintln!("git-bulk-clean {VERSION}");
        return;
    }

    let daemon = args.iter().any(|a| a == "--daemon");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    if dry_run {
        log("dry-run mode — no git commands will be executed");
    }

    let cfg = Config::from_env();

    if daemon {
        log(&format!(
            "daemon mode — interval {}s",
            cfg.interval_secs
        ));
        loop {
            run_cycle(&cfg, dry_run);
            log(&format!("sleeping {}s", cfg.interval_secs));
            thread::sleep(Duration::from_secs(cfg.interval_secs));
        }
    } else {
        let stats = run_cycle(&cfg, dry_run);
        if stats.failed > 0 {
            std::process::exit(1);
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    fn make_config(vars: &[(&str, &str)]) -> Config {
        let map: HashMap<&str, &str> = vars.iter().cloned().collect();
        Config::from_vars(|k| {
            map.get(k)
                .map(|v| v.to_string())
                .ok_or(env::VarError::NotPresent)
        })
    }

    // dedup helpers used by collect_repos

    fn dedup_sorted(lists: Vec<Vec<String>>) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        for list in lists {
            seen.extend(list);
        }
        let mut v: Vec<_> = seen.into_iter().collect();
        v.sort();
        v
    }

    #[test]
    fn dedup_empty() {
        assert!(dedup_sorted(vec![vec![], vec![]]).is_empty());
    }

    #[test]
    fn dedup_no_overlap() {
        let got = dedup_sorted(vec![
            vec!["/a/1".into(), "/a/2".into()],
            vec!["/b/3".into()],
        ]);
        assert_eq!(got, ["/a/1", "/a/2", "/b/3"]);
    }

    #[test]
    fn dedup_full_overlap() {
        let got = dedup_sorted(vec![
            vec!["/x".into(), "/y".into()],
            vec!["/x".into(), "/y".into()],
        ]);
        assert_eq!(got, ["/x", "/y"]);
    }

    #[test]
    fn dedup_partial_overlap() {
        let got = dedup_sorted(vec![
            vec!["/a".into(), "/b".into()],
            vec!["/b".into(), "/c".into()],
        ]);
        assert_eq!(got, ["/a", "/b", "/c"]);
    }

    #[test]
    fn dedup_sorted_order() {
        let got = dedup_sorted(vec![vec!["/z".into(), "/a".into()], vec!["/m".into()]]);
        assert_eq!(got, ["/a", "/m", "/z"]);
    }

    // Config parsing

    #[test]
    fn config_defaults() {
        let cfg = make_config(&[]);
        assert!(cfg.repos.is_empty());
        assert!(!cfg.ghq_enable);
        assert_eq!(cfg.reflog_expire, DEFAULT_REFLOG_EXPIRE);
        assert!(!cfg.aggressive);
        assert_eq!(cfg.interval_secs, DEFAULT_INTERVAL_SECS);
        assert!(!cfg.skip_submodules);
        assert!(!cfg.skip_lfs);
    }

    #[test]
    fn config_all_vars() {
        let cfg = make_config(&[
            ("MAINTENANCE_REPOS", "/a, /b , /c"),
            ("MAINTENANCE_GHQ_ENABLE", "true"),
            ("MAINTENANCE_REFLOG_EXPIRE", "7.days.ago"),
            ("MAINTENANCE_AGGRESSIVE", "true"),
            ("MAINTENANCE_INTERVAL", "3600"),
            ("MAINTENANCE_SKIP_SUBMODULES", "true"),
            ("MAINTENANCE_SKIP_LFS", "true"),
        ]);
        assert_eq!(cfg.repos, ["/a", "/b", "/c"]);
        assert!(cfg.ghq_enable);
        assert_eq!(cfg.reflog_expire, "7.days.ago");
        assert!(cfg.aggressive);
        assert_eq!(cfg.interval_secs, 3600);
        assert!(cfg.skip_submodules);
        assert!(cfg.skip_lfs);
    }

    #[test]
    fn config_repos_filters_blank() {
        let cfg = make_config(&[("MAINTENANCE_REPOS", "/a,,  ,/b")]);
        assert_eq!(cfg.repos, ["/a", "/b"]);
    }

    #[test]
    fn config_interval_bad_value_falls_back() {
        let cfg = make_config(&[("MAINTENANCE_INTERVAL", "oops")]);
        assert_eq!(cfg.interval_secs, DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn config_bool_case_insensitive() {
        for val in ["true", "TRUE", "True"] {
            let cfg = make_config(&[("MAINTENANCE_GHQ_ENABLE", val)]);
            assert!(cfg.ghq_enable, "expected true for {val:?}");
        }
        for val in ["false", "FALSE", "0", "yes"] {
            let cfg = make_config(&[("MAINTENANCE_GHQ_ENABLE", val)]);
            assert!(!cfg.ghq_enable, "expected false for {val:?}");
        }
    }

    // collect_repos: dedup + missing-path filter

    #[test]
    fn collect_deduplicates_and_ignores_missing() {
        let tmp = env::temp_dir().join("git_bulk_clean_collect_test");
        let _ = fs::create_dir_all(&tmp);
        let real = tmp.to_string_lossy().to_string();

        let cfg = make_config(&[(
            "MAINTENANCE_REPOS",
            &format!("{real},{real},/no-such-path-xyz"),
        )]);
        let repos = collect_repos(&cfg);
        assert_eq!(repos, vec![real]);
        let _ = fs::remove_dir_all(&tmp);
    }

    // Feature detection

    #[test]
    fn has_submodules_detects_gitmodules() {
        let tmp = env::temp_dir().join("git_bulk_clean_submodule_test");
        let _ = fs::create_dir_all(&tmp);
        assert!(!has_submodules(tmp.to_str().unwrap()));
        fs::write(tmp.join(".gitmodules"), "[submodule]\n").unwrap();
        assert!(has_submodules(tmp.to_str().unwrap()));
        let _ = fs::remove_dir_all(&tmp);
    }

    // Timestamp format

    #[test]
    fn log_timestamp_format() {
        let secs: u64 = 3661; // 01:01:01
        let (h, m, s) = ((secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
        let ts = format!("{h:02}:{m:02}:{s:02}");
        assert_eq!(ts, "01:01:01");
    }
}
