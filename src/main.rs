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

const DEFAULT_WORKERS: usize = 5;
const DEFAULT_REFLOG_EXPIRE: &str = "30.days.ago";
const DEFAULT_INTERVAL_SECS: u64 = 86400;
const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── shell completions ─────────────────────────────────────────────────────────

const COMPLETION_BASH: &str = r#"# bash completion for git-bulk-clean

_git_bulk_clean() {
    local cur prev
    _init_completion 2>/dev/null || {
        cur="${COMP_WORDS[COMP_CWORD]}"
        prev="${COMP_WORDS[COMP_CWORD-1]}"
    }

    if [[ "${prev}" == "--generate-completions" ]]; then
        COMPREPLY=($(compgen -W "bash zsh fish" -- "${cur}"))
        return 0
    fi

    COMPREPLY=($(compgen -W "
        --daemon
        --dry-run
        --list
        --version
        -V
        --help
        -h
        --generate-completions
    " -- "${cur}"))
}

complete -F _git_bulk_clean git-bulk-clean
"#;

const COMPLETION_ZSH: &str = r#"#compdef git-bulk-clean

_git_bulk_clean() {
    local -a opts
    opts=(
        '--daemon[loop forever, sleeping MAINTENANCE_INTERVAL between cycles]'
        '--dry-run[show what would run without executing git commands]'
        '--list[print discovered repositories and exit]'
        '--version[print version and exit]'
        '-V[print version and exit]'
        '--help[print this help and exit]'
        '-h[print this help and exit]'
        '--generate-completions[print shell completion script]:shell:(bash zsh fish)'
    )

    _arguments -s $opts
}

_git_bulk_clean "$@"
"#;

const COMPLETION_FISH: &str = r#"# fish completion for git-bulk-clean

complete -c git-bulk-clean -f

complete -c git-bulk-clean -l daemon                -d 'Loop forever, sleeping MAINTENANCE_INTERVAL between cycles'
complete -c git-bulk-clean -l dry-run               -d 'Show what would run without executing git commands'
complete -c git-bulk-clean -l list                  -d 'Print discovered repositories and exit'
complete -c git-bulk-clean -l version               -d 'Print version and exit'
complete -c git-bulk-clean -s V                     -d 'Print version and exit'
complete -c git-bulk-clean -l help                  -d 'Print this help and exit'
complete -c git-bulk-clean -s h                     -d 'Print this help and exit'
complete -c git-bulk-clean -l generate-completions  -r -a 'bash zsh fish' -d 'Print shell completion script for given shell'
"#;

// ── config ───────────────────────────────────────────────────────────────────

struct Config {
    repos: Vec<String>,
    ghq_enable: bool,
    reflog_expire: String,
    aggressive: bool,
    interval_secs: u64,
    num_workers: usize,
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
            num_workers: get("MAINTENANCE_WORKERS")
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(DEFAULT_WORKERS),
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

// Subset of Config needed by the cleanup pipeline. Shared across workers via Arc.
struct CleanOptions {
    reflog_expire: String,
    aggressive: bool,
    skip_submodules: bool,
    skip_lfs: bool,
}

impl CleanOptions {
    fn from_config(cfg: &Config) -> Self {
        CleanOptions {
            reflog_expire: cfg.reflog_expire.clone(),
            aggressive: cfg.aggressive,
            skip_submodules: cfg.skip_submodules,
            skip_lfs: cfg.skip_lfs,
        }
    }
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

struct RepoInfo {
    path: String,
    is_bare: bool,
}

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

// Returns None if dir is not a git repo, Some(true) if bare, Some(false) if normal.
// A single git call covers both the validity check and the bare/normal distinction.
fn detect_repo_kind(dir: &str) -> Option<bool> {
    Command::new("git")
        .args(["rev-parse", "--is-bare-repository"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "true")
}

fn collect_repos(cfg: &Config) -> Vec<RepoInfo> {
    let mut seen: HashSet<String> = cfg.repos.iter().cloned().collect();
    if cfg.ghq_enable {
        seen.extend(ghq_repos());
    }
    let mut repos: Vec<RepoInfo> = seen
        .into_iter()
        .filter(|p| Path::new(p).is_dir())
        .filter_map(|p| detect_repo_kind(&p).map(|is_bare| RepoInfo { path: p, is_bare }))
        .collect();
    repos.sort_by(|a, b| a.path.cmp(&b.path));
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

// ── repo feature detection ────────────────────────────────────────────────────

fn has_submodules(dir: &str) -> bool {
    Path::new(dir).join(".gitmodules").exists()
}

fn has_lfs(dir: &str) -> bool {
    // filter.lfs.clean is set iff git-lfs was ever initialised in this repo
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
    // --prune-tags also removes tags deleted on the remote (--prune covers branches)
    git(dir, &["fetch", "--all", "--prune", "--prune-tags"])
}

fn phase_refs(dir: &str, reflog_expire: &str) -> bool {
    let ok = git(dir, &["pack-refs", "--all"]);
    let ok = ok & git(dir, &["worktree", "prune"]);
    ok & git(
        dir,
        &["reflog", "expire", &format!("--expire={reflog_expire}"), "--all"],
    )
}

fn phase_objects_normal(dir: &str) -> bool {
    // loose-objects + incremental-repack run unconditionally (gc --auto only runs
    // above its internal thresholds), then gc finalises pruning
    let ok = git(dir, &["maintenance", "run", "--task=loose-objects"]);
    let ok = ok & git(dir, &["maintenance", "run", "--task=incremental-repack"]);
    ok & git(dir, &["gc", "--auto"])
}

fn phase_objects_aggressive(dir: &str) -> bool {
    // Full repack: -f ignores existing deltas, --delta-base-offset shrinks pack size
    let ok = git(dir, &["repack", "-a", "-d", "-f", "--delta-base-offset"]);
    ok & git(dir, &["gc", "--aggressive", "--prune=all"])
}

fn phase_indices(dir: &str) -> bool {
    git(dir, &["maintenance", "run", "--task=commit-graph"])
}

fn phase_submodules(dir: &str) -> bool {
    let ok = git(dir, &["submodule", "sync", "--recursive"]);
    ok & git(
        dir,
        &["submodule", "foreach", "--recursive", "git", "gc", "--auto"],
    )
}

fn phase_lfs(dir: &str) -> bool {
    // Route through git's exec-path so git-lfs is resolved the same way the
    // user's shell would find it (handles PATH-independent Nix setups)
    git(dir, &["lfs", "prune"])
}

// ── per-repo cleanup orchestration ───────────────────────────────────────────

fn clean_repo(repo: &RepoInfo, opts: &CleanOptions, dry_run: bool, n: usize, total: usize) -> bool {
    let dir = &repo.path;
    let t = Instant::now();
    log(&format!(
        "[{n}/{total}] cleaning: {dir}{}",
        if repo.is_bare { " (bare)" } else { "" }
    ));

    if dry_run {
        log(&format!("  (dry-run) git fetch --all --prune --prune-tags"));
        log(&format!("  (dry-run) git pack-refs --all"));
        log(&format!("  (dry-run) git worktree prune"));
        log(&format!(
            "  (dry-run) git reflog expire --expire={} --all",
            opts.reflog_expire
        ));
        log(&format!("  (dry-run) git maintenance run --task=loose-objects"));
        if opts.aggressive {
            log(&format!("  (dry-run) git repack -a -d -f --delta-base-offset"));
            log(&format!("  (dry-run) git gc --aggressive --prune=all"));
        } else {
            log(&format!(
                "  (dry-run) git maintenance run --task=incremental-repack"
            ));
            log(&format!("  (dry-run) git gc --auto"));
        }
        log(&format!(
            "  (dry-run) git maintenance run --task=commit-graph"
        ));
        if !repo.is_bare && !opts.skip_submodules && has_submodules(dir) {
            log(&format!("  (dry-run) git submodule sync --recursive"));
            log(&format!(
                "  (dry-run) git submodule foreach --recursive git gc --auto"
            ));
        }
        if !opts.skip_lfs && has_lfs(dir) {
            log(&format!("  (dry-run) git lfs prune"));
        }
        return true;
    }

    let ok = phase_fetch(dir)
        & phase_refs(dir, &opts.reflog_expire)
        & if opts.aggressive {
            phase_objects_aggressive(dir)
        } else {
            phase_objects_normal(dir)
        }
        & phase_indices(dir);

    // Submodule cleanup requires a working tree — skip for bare repos
    let ok = if !repo.is_bare && !opts.skip_submodules && has_submodules(dir) {
        ok & phase_submodules(dir)
    } else {
        ok
    };

    let ok = if !opts.skip_lfs && has_lfs(dir) {
        ok & phase_lfs(dir)
    } else {
        ok
    };

    let ms = t.elapsed().as_millis();
    log(&format!(
        "[{n}/{total}] {dir}: done in {ms}ms ({})",
        if ok { "ok" } else { "some errors" }
    ));
    ok
}

// ── worker pool ───────────────────────────────────────────────────────────────

struct CycleStats {
    failed: usize,
}

fn run_cycle(cfg: &Config, dry_run: bool) -> CycleStats {
    let repos = collect_repos(cfg);
    let total = repos.len();

    if total == 0 {
        log("no repositories found");
        return CycleStats { failed: 0 };
    }

    let bare_count = repos.iter().filter(|r| r.is_bare).count();
    log(&format!(
        "starting cycle: {total} repositories ({bare_count} bare), {} workers",
        cfg.num_workers
    ));

    let (tx, rx) = std::sync::mpsc::channel::<RepoInfo>();
    let rx = Arc::new(Mutex::new(rx));
    let failed_count = Arc::new(AtomicUsize::new(0));
    let progress = Arc::new(AtomicUsize::new(0));
    let opts = Arc::new(CleanOptions::from_config(cfg));

    for repo in repos {
        tx.send(repo).expect("channel send");
    }
    drop(tx);

    let handles: Vec<_> = (0..cfg.num_workers)
        .map(|id| {
            let rx = Arc::clone(&rx);
            let opts = Arc::clone(&opts);
            let failed_count = Arc::clone(&failed_count);
            let progress = Arc::clone(&progress);

            thread::spawn(move || loop {
                match rx.lock().unwrap().recv() {
                    Ok(repo) => {
                        let n = progress.fetch_add(1, Ordering::Relaxed) + 1;
                        if !clean_repo(&repo, &opts, dry_run, n, total) {
                            failed_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        log(&format!("worker {id}: done"));
                        break;
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

    CycleStats { failed }
}

// ── cli ───────────────────────────────────────────────────────────────────────

fn print_help(prog: &str) {
    eprintln!("Usage: {prog} [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --daemon                      Loop forever, sleeping MAINTENANCE_INTERVAL between cycles");
    eprintln!("  --dry-run                     Show what would run without executing git commands");
    eprintln!("  --list                        Print discovered repositories and exit");
    eprintln!("  --generate-completions SHELL  Print completion script (bash, zsh, fish) and exit");
    eprintln!("  --version                     Print version and exit");
    eprintln!("  -h, --help                    Print this help and exit");
    eprintln!();
    eprintln!("Environment variables:");
    eprintln!("  MAINTENANCE_REPOS              Comma-separated repo paths");
    eprintln!("  MAINTENANCE_GHQ_ENABLE         true → include all ghq-managed repos");
    eprintln!("  MAINTENANCE_REFLOG_EXPIRE      Reflog cutoff (default: {DEFAULT_REFLOG_EXPIRE})");
    eprintln!("  MAINTENANCE_AGGRESSIVE         true → full repack + gc --aggressive");
    eprintln!("  MAINTENANCE_INTERVAL           Daemon sleep interval in seconds (default: {DEFAULT_INTERVAL_SECS})");
    eprintln!("  MAINTENANCE_WORKERS            Parallel workers (default: {DEFAULT_WORKERS})");
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
    eprintln!("  9. git submodule sync + foreach gc                 (if .gitmodules, non-bare only)");
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

    if let Some(pos) = args.iter().position(|a| a == "--generate-completions") {
        match args.get(pos + 1).map(String::as_str) {
            Some("bash") => { print!("{COMPLETION_BASH}"); return; }
            Some("zsh")  => { print!("{COMPLETION_ZSH}"); return; }
            Some("fish") => { print!("{COMPLETION_FISH}"); return; }
            Some(other) => {
                eprintln!("error: unknown shell {other:?}; supported: bash, zsh, fish");
                std::process::exit(2);
            }
            None => {
                eprintln!("error: --generate-completions requires a shell argument (bash, zsh, fish)");
                std::process::exit(2);
            }
        }
    }

    let cfg = Config::from_env();

    if args.iter().any(|a| a == "--list") {
        let repos = collect_repos(&cfg);
        if repos.is_empty() {
            log("no repositories found");
        }
        for repo in repos {
            println!("{}  {}", if repo.is_bare { "bare" } else { "norm" }, repo.path);
        }
        return;
    }

    let daemon = args.iter().any(|a| a == "--daemon");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    if dry_run {
        log("dry-run mode — no git commands will be executed");
    }

    if daemon {
        log(&format!(
            "daemon mode — interval {}s, {} workers",
            cfg.interval_secs, cfg.num_workers
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

    #[test]
    fn config_defaults() {
        let cfg = make_config(&[]);
        assert!(cfg.repos.is_empty());
        assert!(!cfg.ghq_enable);
        assert_eq!(cfg.reflog_expire, DEFAULT_REFLOG_EXPIRE);
        assert!(!cfg.aggressive);
        assert_eq!(cfg.interval_secs, DEFAULT_INTERVAL_SECS);
        assert_eq!(cfg.num_workers, DEFAULT_WORKERS);
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
            ("MAINTENANCE_WORKERS", "8"),
            ("MAINTENANCE_SKIP_SUBMODULES", "true"),
            ("MAINTENANCE_SKIP_LFS", "true"),
        ]);
        assert_eq!(cfg.repos, ["/a", "/b", "/c"]);
        assert!(cfg.ghq_enable);
        assert_eq!(cfg.reflog_expire, "7.days.ago");
        assert!(cfg.aggressive);
        assert_eq!(cfg.interval_secs, 3600);
        assert_eq!(cfg.num_workers, 8);
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
    fn config_workers_zero_falls_back() {
        let cfg = make_config(&[("MAINTENANCE_WORKERS", "0")]);
        assert_eq!(cfg.num_workers, DEFAULT_WORKERS);
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

    #[test]
    fn collect_deduplicates_and_validates_git_repo() {
        // CARGO_MANIFEST_DIR is the project root — a real, non-bare git repo
        let repo_path = env!("CARGO_MANIFEST_DIR").to_string();
        let cfg = make_config(&[(
            "MAINTENANCE_REPOS",
            &format!("{repo_path},{repo_path},/no-such-path-xyz"),
        )]);
        let repos = collect_repos(&cfg);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].path, repo_path);
        assert!(!repos[0].is_bare, "project repo should not be bare");
    }

    #[test]
    fn detect_repo_kind_project_is_non_bare() {
        let kind = detect_repo_kind(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(kind, Some(false));
    }

    #[test]
    fn detect_repo_kind_non_repo_returns_none() {
        // Create a temp dir that is not a git repo
        let tmp = env::temp_dir().join("git_bulk_clean_nonrepo_test");
        let _ = fs::create_dir_all(&tmp);
        let kind = detect_repo_kind(tmp.to_str().unwrap());
        assert!(kind.is_none(), "plain directory should not be detected as a repo");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn has_submodules_detects_gitmodules() {
        let tmp = env::temp_dir().join("git_bulk_clean_submodule_test");
        let _ = fs::create_dir_all(&tmp);
        assert!(!has_submodules(tmp.to_str().unwrap()));
        fs::write(tmp.join(".gitmodules"), "[submodule]\n").unwrap();
        assert!(has_submodules(tmp.to_str().unwrap()));
        let _ = fs::remove_dir_all(&tmp);
    }

    const ALL_FLAGS: &[&str] = &[
        "--daemon",
        "--dry-run",
        "--list",
        "--version",
        "--help",
        "--generate-completions",
    ];

    #[test]
    fn completion_bash_contains_all_flags() {
        for flag in ALL_FLAGS {
            assert!(
                COMPLETION_BASH.contains(flag),
                "bash completion missing {flag}"
            );
        }
    }

    #[test]
    fn completion_zsh_contains_all_flags() {
        for flag in ALL_FLAGS {
            assert!(
                COMPLETION_ZSH.contains(flag),
                "zsh completion missing {flag}"
            );
        }
    }

    #[test]
    fn completion_fish_contains_all_flags() {
        // fish uses `-l <name>` syntax, not `--<name>`
        const FISH_FLAGS: &[&str] = &[
            "-l daemon",
            "-l dry-run",
            "-l list",
            "-l version",
            "-l help",
            "-l generate-completions",
        ];
        for flag in FISH_FLAGS {
            assert!(
                COMPLETION_FISH.contains(flag),
                "fish completion missing {flag}"
            );
        }
    }

    #[test]
    fn completion_bash_handles_generate_completions_subarg() {
        assert!(COMPLETION_BASH.contains("bash"));
        assert!(COMPLETION_BASH.contains("zsh"));
        assert!(COMPLETION_BASH.contains("fish"));
    }

    #[test]
    fn completion_zsh_generate_completions_has_shell_choices() {
        assert!(COMPLETION_ZSH.contains("bash"));
        assert!(COMPLETION_ZSH.contains("zsh"));
        assert!(COMPLETION_ZSH.contains("fish"));
    }

    #[test]
    fn completion_fish_generate_completions_requires_argument() {
        assert!(COMPLETION_FISH.contains("-r") || COMPLETION_FISH.contains("--require-parameter"));
    }

    #[test]
    fn log_timestamp_format() {
        let secs: u64 = 3661; // 01:01:01
        let (h, m, s) = ((secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
        assert_eq!(format!("{h:02}:{m:02}:{s:02}"), "01:01:01");
    }
}
