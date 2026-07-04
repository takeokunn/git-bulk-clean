//! git-bulk-clean — parallel Git repository maintenance CLI/daemon.
//!
//! Built entirely on the standard library. The code favours making illegal
//! states unrepresentable: repository kinds, target shells, and the worker
//! count are modelled as dedicated types rather than raw `bool`/`String`/`usize`.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::env;
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ── constants ────────────────────────────────────────────────────────────────

const DEFAULT_WORKERS: NonZeroUsize = NonZeroUsize::new(5).unwrap();
const DEFAULT_REFLOG_EXPIRE: &str = "30.days.ago";
const DEFAULT_INTERVAL_SECS: u64 = 86400;
const MAX_WORKERS: NonZeroUsize = NonZeroUsize::new(256).unwrap();
const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── target shells ─────────────────────────────────────────────────────────────

/// A shell for which completion scripts can be generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// Every supported shell, in a stable order for help/error text.
    const ALL: [Shell; 3] = [Shell::Bash, Shell::Zsh, Shell::Fish];

    /// The lowercase name used on the command line and in completion output.
    fn as_str(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
        }
    }

    /// Parse a `--generate-completions` argument into a [`Shell`].
    fn parse(arg: &str) -> Result<Shell, String> {
        Shell::ALL
            .into_iter()
            .find(|s| s.as_str() == arg)
            .ok_or_else(|| {
                let names = Shell::ALL.map(Shell::as_str).join(", ");
                format!("unknown shell {arg:?}; supported: {names}")
            })
    }

    /// The static completion script emitted for this shell.
    fn completion_script(self) -> &'static str {
        match self {
            Shell::Bash => COMPLETION_BASH,
            Shell::Zsh => COMPLETION_ZSH,
            Shell::Fish => COMPLETION_FISH,
        }
    }
}

// ── repository kind ───────────────────────────────────────────────────────────

/// Whether a repository has a working tree (`Normal`) or not (`Bare`).
///
/// Phases that require a working tree (worktree prune, submodules, branch
/// pruning) are skipped for [`RepoKind::Bare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoKind {
    Normal,
    Bare,
}

impl RepoKind {
    fn from_is_bare(is_bare: bool) -> Self {
        if is_bare {
            RepoKind::Bare
        } else {
            RepoKind::Normal
        }
    }

    fn is_bare(self) -> bool {
        matches!(self, RepoKind::Bare)
    }

    /// Short label used by `--list` output (`norm` / `bare`).
    fn label(self) -> &'static str {
        match self {
            RepoKind::Normal => "norm",
            RepoKind::Bare => "bare",
        }
    }
}

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
    num_workers: NonZeroUsize,
    skip_submodules: bool,
    skip_lfs: bool,
    prune_tags: bool,
    prune_branches: bool,
    protected_branches: Vec<String>,
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
            reflog_expire: {
                let v = get("MAINTENANCE_REFLOG_EXPIRE")
                    .unwrap_or_else(|_| DEFAULT_REFLOG_EXPIRE.to_string());
                if is_valid_reflog_expire(&v) {
                    v
                } else {
                    eprintln!(
                        "[git-bulk-clean] warning: MAINTENANCE_REFLOG_EXPIRE {:?} rejected (dangerous value), using default",
                        v
                    );
                    DEFAULT_REFLOG_EXPIRE.to_string()
                }
            },
            aggressive: bool_var(&get, "MAINTENANCE_AGGRESSIVE", false),
            // 0 would make the daemon loop without sleeping; treat it as invalid
            interval_secs: get("MAINTENANCE_INTERVAL")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|&v| v > 0)
                .unwrap_or(DEFAULT_INTERVAL_SECS),
            num_workers: get("MAINTENANCE_WORKERS")
                .ok()
                .and_then(|v| v.trim().parse::<NonZeroUsize>().ok())
                .map(|n| n.min(MAX_WORKERS))
                .unwrap_or(DEFAULT_WORKERS),
            skip_submodules: bool_var(&get, "MAINTENANCE_SKIP_SUBMODULES", false),
            skip_lfs: bool_var(&get, "MAINTENANCE_SKIP_LFS", false),
            prune_tags: bool_var(&get, "MAINTENANCE_PRUNE_TAGS", false),
            prune_branches: bool_var(&get, "MAINTENANCE_PRUNE_BRANCHES", false),
            protected_branches: get("MAINTENANCE_PROTECTED_BRANCHES")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
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

fn is_valid_reflog_expire(v: &str) -> bool {
    let v = v.trim();
    // "never" safely disables expiry (git parses these case-insensitively)
    if v.eq_ignore_ascii_case("never") {
        return true;
    }
    // "now" and "all" expire everything immediately — dangerous
    if v.eq_ignore_ascii_case("now") || v.eq_ignore_ascii_case("all") {
        return false;
    }
    // Allow alphanumeric, '.', '-', space: covers "30.days.ago", "YYYY-MM-DD", "90 days ago"
    v.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ' ')
}

// Subset of Config needed by the cleanup pipeline. Shared across workers via Arc.
struct CleanOptions {
    reflog_expire: String,
    aggressive: bool,
    skip_submodules: bool,
    skip_lfs: bool,
    prune_tags: bool,
    prune_branches: bool,
    protected_branches: Vec<String>,
}

impl CleanOptions {
    fn from_config(cfg: &Config) -> Self {
        CleanOptions {
            reflog_expire: cfg.reflog_expire.clone(),
            aggressive: cfg.aggressive,
            skip_submodules: cfg.skip_submodules,
            skip_lfs: cfg.skip_lfs,
            prune_tags: cfg.prune_tags,
            prune_branches: cfg.prune_branches,
            protected_branches: cfg.protected_branches.clone(),
        }
    }
}

// ── logging ──────────────────────────────────────────────────────────────────

fn format_timestamp(secs: u64) -> String {
    let (h, m, s) = ((secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

fn log(msg: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    eprintln!("[git-bulk-clean {}] {msg}", format_timestamp(secs));
}

fn sanitize_for_log(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

// ── repo collection ──────────────────────────────────────────────────────────

struct RepoInfo {
    path: String,
    kind: RepoKind,
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

// Returns None if `dir` is not a git repository. A single git call covers the
// validity check, the bare/normal distinction, and the canonical git dir used
// to deduplicate repos reached via different spellings (trailing slash,
// symlink, subdirectory of a working tree).
fn probe_repo(dir: &str) -> Option<(RepoKind, String)> {
    Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .args(["rev-parse", "--is-bare-repository", "--absolute-git-dir"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            // Output lines follow argument order: is-bare flag, then git dir
            let mut lines = s.lines();
            let is_bare = lines.next()?.trim() == "true";
            let git_dir = lines.next()?.trim().to_string();
            Some((RepoKind::from_is_bare(is_bare), git_dir))
        })
}

fn collect_repos(cfg: &Config) -> Vec<RepoInfo> {
    let mut candidates: Vec<String> = {
        let mut seen: HashSet<String> = cfg.repos.iter().cloned().collect();
        if cfg.ghq_enable {
            seen.extend(ghq_repos());
        }
        seen.into_iter().collect()
    };
    // Sort before deduplication so which spelling of a duplicate survives is
    // deterministic, and the final list stays ordered by path.
    candidates.sort();

    let mut seen_git_dirs: HashSet<String> = HashSet::new();
    let mut repos: Vec<RepoInfo> = Vec::new();
    for path in candidates {
        if !Path::new(&path).is_dir() {
            continue;
        }
        let Some((kind, git_dir)) = probe_repo(&path) else {
            continue;
        };
        // Two workers running gc on the same repo concurrently fight over
        // git's locks, so drop paths that resolve to an already-seen git dir.
        if seen_git_dirs.insert(git_dir) {
            repos.push(RepoInfo { path, kind });
        }
    }
    repos
}

// ── git command helpers ───────────────────────────────────────────────────────

fn git(dir: &str, args: &[&str]) -> bool {
    match Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "credential.helper")
        .env("GIT_CONFIG_VALUE_1", "")
        .env("GIT_CONFIG_KEY_2", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_2", "false")
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
                log(&format!(
                    "{}: `git {}` — {}",
                    sanitize_for_log(dir),
                    args.join(" "),
                    sanitize_for_log(msg)
                ));
            }
            false
        }
        Err(e) => {
            log(&format!(
                "{}: `git {}` — {e}",
                sanitize_for_log(dir),
                args.join(" ")
            ));
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
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .args(["config", "--local", "--get-regexp", "filter\\.lfs\\."])
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── cleanup phases ────────────────────────────────────────────────────────────

fn phase_fetch(dir: &str, prune_tags: bool) -> bool {
    // --prune-tags removes local tags absent from the remote — including tags
    // the user created and never pushed — so it stays opt-in.
    if prune_tags {
        git(dir, &["fetch", "--all", "--prune", "--prune-tags"])
    } else {
        git(dir, &["fetch", "--all", "--prune"])
    }
}

fn phase_refs(dir: &str, reflog_expire: &str) -> bool {
    let ok = git(dir, &["pack-refs", "--all"]);
    let ok = ok & git(dir, &["worktree", "prune"]);
    let ok = ok
        & git(
            dir,
            &[
                "reflog",
                "expire",
                &format!("--expire={reflog_expire}"),
                "--all",
            ],
        );
    let ok = ok & git(dir, &["rerere", "gc"]);
    ok & git(dir, &["notes", "prune"])
}

fn local_branch_exists(dir: &str, name: &str) -> bool {
    // Fully qualified so a tag with the same name cannot shadow the branch
    Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ])
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// Returns None when no mainline can be determined; branch pruning is skipped
// then instead of failing every cycle against a nonexistent branch.
fn detect_mainline(dir: &str) -> Option<String> {
    // Try origin's default branch via symbolic-ref (e.g. "origin/main" → "main")
    let out = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // "origin/main" → "main"
            let branch = match s.split_once('/') {
                Some((_, b)) => b.to_string(),
                None => s,
            };
            // A name starting with '-' would be parsed as a flag downstream
            if !branch.is_empty() && !branch.starts_with('-') {
                return Some(branch);
            }
        }
    }
    // Fallback: "main", then "master" — but only if the branch actually exists
    ["main", "master"]
        .into_iter()
        .find(|name| local_branch_exists(dir, name))
        .map(str::to_string)
}

fn phase_branches(dir: &str, protected_branches: &[String]) -> bool {
    let Some(mainline) = detect_mainline(dir) else {
        log(&format!(
            "{}: no mainline branch found; skipping branch pruning",
            sanitize_for_log(dir)
        ));
        return true;
    };
    let out = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "credential.helper")
        .env("GIT_CONFIG_VALUE_1", "")
        .env("GIT_CONFIG_KEY_2", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_2", "false")
        .args(["branch", "--merged", &mainline])
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr);
            let msg = msg.trim();
            if !msg.is_empty() {
                log(&format!(
                    "{}: `git branch --merged {}` — {}",
                    sanitize_for_log(dir),
                    sanitize_for_log(&mainline),
                    sanitize_for_log(msg)
                ));
            }
            return false;
        }
        Err(e) => {
            log(&format!(
                "{}: `git branch --merged {}` — {e}",
                sanitize_for_log(dir),
                sanitize_for_log(&mainline)
            ));
            return false;
        }
    };
    let candidates: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        // '*' marks the checked-out branch and '+' one checked out in another
        // worktree — git refuses to delete either, so skip both up front
        .filter(|l| !l.starts_with('*') && !l.starts_with('+'))
        .map(|l| l.trim().to_string())
        .filter(|b| {
            !b.is_empty()
                && !b.starts_with('(') // skip "(HEAD detached at ...)"
                && !b.starts_with('-') // never let a ref name be parsed as a flag
                && b != &mainline
                && !protected_branches.contains(b)
        })
        .collect();
    if candidates.is_empty() {
        true
    } else {
        // "--" so no branch name can ever be interpreted as an option
        let mut args = vec!["branch", "-d", "--"];
        args.extend(candidates.iter().map(|s| s.as_str()));
        git(dir, &args)
    }
}

fn phase_objects_normal(dir: &str) -> bool {
    // loose-objects + incremental-repack run unconditionally (gc --auto only runs
    // above its internal thresholds), then gc finalises pruning
    let ok = git(dir, &["maintenance", "run", "--task=loose-objects"]);
    let ok = ok & git(dir, &["maintenance", "run", "--task=incremental-repack"]);
    ok & git(dir, &["gc", "--auto"])
}

fn phase_objects_aggressive(dir: &str) -> bool {
    // Full repack: -a packs everything, -d removes redundant packs, -f forces re-deltaing
    let ok = git(dir, &["repack", "-a", "-d", "-f"]);
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
        "[{n}/{total}] cleaning: {}{}",
        sanitize_for_log(dir),
        if repo.kind.is_bare() { " (bare)" } else { "" }
    ));

    if dry_run {
        if opts.prune_tags {
            log("  (dry-run) git fetch --all --prune --prune-tags");
        } else {
            log("  (dry-run) git fetch --all --prune");
        }
        log("  (dry-run) git pack-refs --all");
        log("  (dry-run) git worktree prune");
        log(&format!(
            "  (dry-run) git reflog expire --expire={} --all",
            opts.reflog_expire
        ));
        log("  (dry-run) git rerere gc");
        log("  (dry-run) git notes prune");
        if !repo.kind.is_bare() && opts.prune_branches {
            match detect_mainline(dir) {
                Some(mainline) => log(&format!(
                    "  (dry-run) git branch --merged {} | git branch -d -- (merged branches)",
                    sanitize_for_log(&mainline)
                )),
                None => log("  (dry-run) branch pruning skipped: no mainline branch found"),
            }
        }
        log("  (dry-run) git maintenance run --task=loose-objects");
        if opts.aggressive {
            log("  (dry-run) git repack -a -d -f");
            log("  (dry-run) git gc --aggressive --prune=all");
        } else {
            log("  (dry-run) git maintenance run --task=incremental-repack");
            log("  (dry-run) git gc --auto");
        }
        log("  (dry-run) git maintenance run --task=commit-graph");
        if !repo.kind.is_bare() && !opts.skip_submodules && has_submodules(dir) {
            log("  (dry-run) git submodule sync --recursive");
            log("  (dry-run) git submodule foreach --recursive git gc --auto");
        }
        if !opts.skip_lfs && has_lfs(dir) {
            log("  (dry-run) git lfs prune");
        }
        return true;
    }

    let ok = phase_fetch(dir, opts.prune_tags)
        & phase_refs(dir, &opts.reflog_expire)
        & if !repo.kind.is_bare() && opts.prune_branches {
            phase_branches(dir, &opts.protected_branches)
        } else {
            true
        }
        & if opts.aggressive {
            phase_objects_aggressive(dir)
        } else {
            phase_objects_normal(dir)
        }
        & phase_indices(dir);

    // Submodule cleanup requires a working tree — skip for bare repos
    let ok = if !repo.kind.is_bare() && !opts.skip_submodules && has_submodules(dir) {
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
        "[{n}/{total}] {}: done in {ms}ms ({})",
        sanitize_for_log(dir),
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

    let bare_count = repos.iter().filter(|r| r.kind.is_bare()).count();
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

    let handles: Vec<_> = (0..cfg.num_workers.get())
        .map(|id| {
            let rx = Arc::clone(&rx);
            let opts = Arc::clone(&opts);
            let failed_count = Arc::clone(&failed_count);
            let progress = Arc::clone(&progress);

            thread::spawn(move || {
                loop {
                    match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
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
                }
            })
        })
        .collect();

    for h in handles {
        if h.join().is_err() {
            log("a worker thread panicked; cycle may be incomplete");
        }
    }

    let failed = failed_count.load(Ordering::Relaxed);
    let succeeded = total - failed;
    log(&format!(
        "cycle complete — {succeeded}/{total} ok, {failed} failed"
    ));

    CycleStats { failed }
}

// ── cli ───────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Help,
    Version,
    GenerateCompletions(Shell),
    List,
    Run { daemon: bool, dry_run: bool },
}

fn parse_args(flags: &[String]) -> Result<CliAction, String> {
    // Reject unknown options first so a typo (e.g. "--deamon") cannot silently
    // fall through to a one-shot live run.
    let mut iter = flags.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "-h" | "--help" | "-V" | "--version" | "--list" | "--daemon" | "--dry-run" => {}
            "--generate-completions" => {
                iter.next(); // shell name validated below
            }
            other => return Err(format!("unknown option {other:?} (see --help)")),
        }
    }
    if flags.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(CliAction::Help);
    }
    if flags.iter().any(|a| a == "-V" || a == "--version") {
        return Ok(CliAction::Version);
    }
    if let Some(pos) = flags.iter().position(|a| a == "--generate-completions") {
        return match flags.get(pos + 1) {
            Some(arg) => Shell::parse(arg).map(CliAction::GenerateCompletions),
            None => Err(
                "--generate-completions requires a shell argument (bash, zsh, fish)".to_string(),
            ),
        };
    }
    if flags.iter().any(|a| a == "--list") {
        return Ok(CliAction::List);
    }
    Ok(CliAction::Run {
        daemon: flags.iter().any(|a| a == "--daemon"),
        dry_run: flags.iter().any(|a| a == "--dry-run"),
    })
}

fn help_text(prog: &str) -> String {
    format!(
        "Usage: {prog} [OPTIONS]

Options:
  --daemon                      Loop forever, sleeping MAINTENANCE_INTERVAL between cycles
  --dry-run                     Show what would run without executing git commands
  --list                        Print discovered repositories and exit
  --generate-completions SHELL  Print completion script (bash, zsh, fish) and exit
  --version                     Print version and exit
  -h, --help                    Print this help and exit

Environment variables:
  MAINTENANCE_REPOS              Comma-separated repo paths
  MAINTENANCE_GHQ_ENABLE         true → include all ghq-managed repos
  MAINTENANCE_REFLOG_EXPIRE      Reflog cutoff (default: {DEFAULT_REFLOG_EXPIRE})
  MAINTENANCE_AGGRESSIVE         true → full repack + gc --aggressive
  MAINTENANCE_INTERVAL           Daemon sleep interval in seconds (default: {DEFAULT_INTERVAL_SECS})
  MAINTENANCE_WORKERS            Parallel workers (default: {DEFAULT_WORKERS})
  MAINTENANCE_SKIP_SUBMODULES    true → skip submodule cleanup
  MAINTENANCE_SKIP_LFS           true → skip git-lfs prune
  MAINTENANCE_PRUNE_TAGS         true → also delete local tags missing from the remote (fetch --prune-tags)
  MAINTENANCE_PRUNE_BRANCHES     true → delete merged local branches (non-bare only)
  MAINTENANCE_PROTECTED_BRANCHES Comma-separated branch names to protect from deletion

Cleanup pipeline (per repo):
  1. git fetch --all --prune                         (--prune-tags if MAINTENANCE_PRUNE_TAGS)
  2. git pack-refs --all
  3. git worktree prune
  4. git reflog expire --expire=<REFLOG_EXPIRE> --all
  5. git rerere gc
  6. git notes prune
  7. git branch -d -- <merged>                       (if MAINTENANCE_PRUNE_BRANCHES, non-bare)
  8. git maintenance run --task=loose-objects
  9. git maintenance run --task=incremental-repack  (normal)
     git repack -a -d -f                  (aggressive)
 10. git gc --auto                                   (normal)
     git gc --aggressive --prune=all                (aggressive)
 11. git maintenance run --task=commit-graph
 12. git submodule sync + foreach gc                 (if .gitmodules, non-bare only)
 13. git lfs prune                                   (if LFS configured)
"
    )
}

// Explicitly requested help/version go to stdout so they can be piped;
// errors and logs stay on stderr.
fn print_help(prog: &str) {
    print!("{}", help_text(prog));
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("git-bulk-clean");
    let flags: &[String] = args.get(1..).unwrap_or_default();

    match parse_args(flags) {
        Ok(CliAction::Help) => {
            print_help(prog);
        }
        Ok(CliAction::Version) => {
            println!("git-bulk-clean {VERSION}");
        }
        Ok(CliAction::GenerateCompletions(shell)) => {
            print!("{}", shell.completion_script());
        }
        Ok(CliAction::List) => {
            let cfg = Config::from_env();
            let repos = collect_repos(&cfg);
            if repos.is_empty() {
                log("no repositories found");
            }
            for repo in repos {
                println!("{}  {}", repo.kind.label(), repo.path);
            }
        }
        Ok(CliAction::Run { daemon, dry_run }) => {
            let cfg = Config::from_env();
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
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(2);
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
            ("MAINTENANCE_PRUNE_BRANCHES", "true"),
            ("MAINTENANCE_PROTECTED_BRANCHES", "main,release"),
        ]);
        assert_eq!(cfg.repos, ["/a", "/b", "/c"]);
        assert!(cfg.ghq_enable);
        assert_eq!(cfg.reflog_expire, "7.days.ago");
        assert!(cfg.aggressive);
        assert_eq!(cfg.interval_secs, 3600);
        assert_eq!(cfg.num_workers.get(), 8);
        assert!(cfg.skip_submodules);
        assert!(cfg.skip_lfs);
        assert!(cfg.prune_branches);
        assert_eq!(cfg.protected_branches, ["main", "release"]);
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
    fn config_interval_zero_falls_back() {
        // 0 would turn the daemon loop into a busy loop
        let cfg = make_config(&[("MAINTENANCE_INTERVAL", "0")]);
        assert_eq!(cfg.interval_secs, DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn config_prune_tags_defaults_false() {
        // fetch --prune-tags deletes unpushed local tags, so it must be opt-in
        let cfg = make_config(&[]);
        assert!(!cfg.prune_tags);
    }

    #[test]
    fn config_prune_tags_enabled() {
        let cfg = make_config(&[("MAINTENANCE_PRUNE_TAGS", "true")]);
        assert!(cfg.prune_tags);
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
        // Build a real, non-bare git repo so the test never relies on the
        // ambient checkout (the source is a plain copy in sandboxed builds).
        let tmp = make_temp_git_repo("collect_dedup");
        let repo_path = tmp.to_str().unwrap().to_string();
        let cfg = make_config(&[(
            "MAINTENANCE_REPOS",
            &format!("{repo_path},{repo_path},/no-such-path-xyz"),
        )]);
        let repos = collect_repos(&cfg);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].path, repo_path);
        assert!(!repos[0].kind.is_bare(), "temp repo should not be bare");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn probe_repo_project_is_non_bare() {
        let tmp = make_temp_git_repo("detect_kind_nonbare");
        let (kind, git_dir) = probe_repo(tmp.to_str().unwrap()).expect("should be a repo");
        assert_eq!(kind, RepoKind::Normal);
        assert!(
            git_dir.ends_with(".git"),
            "expected absolute git dir, got {git_dir:?}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn probe_repo_non_repo_returns_none() {
        // Create a temp dir that is not a git repo
        let tmp = env::temp_dir().join("git_bulk_clean_nonrepo_test");
        let _ = fs::create_dir_all(&tmp);
        let kind = probe_repo(tmp.to_str().unwrap());
        assert!(
            kind.is_none(),
            "plain directory should not be detected as a repo"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_deduplicates_same_repo_via_trailing_slash() {
        let tmp = make_temp_git_repo("collect_dedup_slash");
        let repo_path = tmp.to_str().unwrap().to_string();
        let cfg = make_config(&[("MAINTENANCE_REPOS", &format!("{repo_path},{repo_path}/"))]);
        let repos = collect_repos(&cfg);
        assert_eq!(
            repos.len(),
            1,
            "same repo via different spellings must be cleaned once"
        );
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
        assert_eq!(format_timestamp(3661), "01:01:01");
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn make_temp_git_repo(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = env::temp_dir().join(format!("git_bulk_clean_{}_{}", name, id));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t.com"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        fs::write(tmp.join("f"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        tmp
    }

    // ── format_timestamp ─────────────────────────────────────────────────────

    #[test]
    fn format_timestamp_zero_is_midnight() {
        assert_eq!(format_timestamp(0), "00:00:00");
    }

    #[test]
    fn format_timestamp_noon() {
        assert_eq!(format_timestamp(12 * 3600), "12:00:00");
    }

    #[test]
    fn format_timestamp_wraps_at_86400() {
        assert_eq!(format_timestamp(86400), "00:00:00");
        assert_eq!(format_timestamp(86400 + 3661), "01:01:01");
    }

    // ── bool_var ─────────────────────────────────────────────────────────────

    #[test]
    fn bool_var_absent_returns_default() {
        let get = |_k: &str| Err::<String, _>(env::VarError::NotPresent);
        assert!(!bool_var(&get, "X", false));
        assert!(bool_var(&get, "X", true));
    }

    #[test]
    fn bool_var_case_insensitive_true() {
        for val in ["true", "TRUE", "True", "tRuE"] {
            let v = val.to_string();
            let get = |_k: &str| Ok::<String, env::VarError>(v.clone());
            assert!(bool_var(&get, "X", false), "expected true for {val:?}");
        }
    }

    #[test]
    fn bool_var_non_true_values_are_false() {
        for val in ["false", "FALSE", "1", "yes", "on", "0", "no"] {
            let v = val.to_string();
            let get = |_k: &str| Ok::<String, env::VarError>(v.clone());
            assert!(!bool_var(&get, "X", false), "expected false for {val:?}");
        }
    }

    // ── CleanOptions ─────────────────────────────────────────────────────────

    #[test]
    fn clean_options_mirrors_config_fields() {
        let cfg = make_config(&[
            ("MAINTENANCE_REFLOG_EXPIRE", "90.days.ago"),
            ("MAINTENANCE_AGGRESSIVE", "true"),
            ("MAINTENANCE_SKIP_SUBMODULES", "true"),
            ("MAINTENANCE_SKIP_LFS", "true"),
            ("MAINTENANCE_PRUNE_BRANCHES", "true"),
            ("MAINTENANCE_PROTECTED_BRANCHES", "main,develop"),
        ]);
        let opts = CleanOptions::from_config(&cfg);
        assert_eq!(opts.reflog_expire, "90.days.ago");
        assert!(opts.aggressive);
        assert!(opts.skip_submodules);
        assert!(opts.skip_lfs);
        assert!(opts.prune_branches);
        assert_eq!(opts.protected_branches, ["main", "develop"]);
    }

    #[test]
    fn clean_options_defaults_are_off() {
        let opts = CleanOptions::from_config(&make_config(&[]));
        assert_eq!(opts.reflog_expire, DEFAULT_REFLOG_EXPIRE);
        assert!(!opts.aggressive);
        assert!(!opts.skip_submodules);
        assert!(!opts.skip_lfs);
    }

    // ── parse_args ───────────────────────────────────────────────────────────

    #[test]
    fn parse_args_empty_gives_run_defaults() {
        assert!(matches!(
            parse_args(&strs(&[])),
            Ok(CliAction::Run {
                daemon: false,
                dry_run: false
            })
        ));
    }

    #[test]
    fn parse_args_help_short() {
        assert!(matches!(parse_args(&strs(&["-h"])), Ok(CliAction::Help)));
    }

    #[test]
    fn parse_args_help_long() {
        assert!(matches!(
            parse_args(&strs(&["--help"])),
            Ok(CliAction::Help)
        ));
    }

    #[test]
    fn parse_args_version_short() {
        assert!(matches!(parse_args(&strs(&["-V"])), Ok(CliAction::Version)));
    }

    #[test]
    fn parse_args_version_long() {
        assert!(matches!(
            parse_args(&strs(&["--version"])),
            Ok(CliAction::Version)
        ));
    }

    #[test]
    fn parse_args_generate_completions_bash() {
        let Ok(CliAction::GenerateCompletions(s)) =
            parse_args(&strs(&["--generate-completions", "bash"]))
        else {
            panic!("expected GenerateCompletions");
        };
        assert_eq!(s, Shell::Bash);
    }

    #[test]
    fn parse_args_generate_completions_zsh() {
        let Ok(CliAction::GenerateCompletions(s)) =
            parse_args(&strs(&["--generate-completions", "zsh"]))
        else {
            panic!("expected GenerateCompletions");
        };
        assert_eq!(s, Shell::Zsh);
    }

    #[test]
    fn parse_args_generate_completions_fish() {
        let Ok(CliAction::GenerateCompletions(s)) =
            parse_args(&strs(&["--generate-completions", "fish"]))
        else {
            panic!("expected GenerateCompletions");
        };
        assert_eq!(s, Shell::Fish);
    }

    #[test]
    fn parse_args_generate_completions_unknown_shell() {
        let err = parse_args(&strs(&["--generate-completions", "powershell"])).unwrap_err();
        assert!(
            err.contains("powershell"),
            "error should name the unknown shell"
        );
    }

    #[test]
    fn parse_args_generate_completions_missing_arg() {
        assert!(parse_args(&strs(&["--generate-completions"])).is_err());
    }

    #[test]
    fn parse_args_list() {
        assert!(matches!(
            parse_args(&strs(&["--list"])),
            Ok(CliAction::List)
        ));
    }

    #[test]
    fn parse_args_daemon_only() {
        assert!(matches!(
            parse_args(&strs(&["--daemon"])),
            Ok(CliAction::Run {
                daemon: true,
                dry_run: false
            })
        ));
    }

    #[test]
    fn parse_args_dry_run_only() {
        assert!(matches!(
            parse_args(&strs(&["--dry-run"])),
            Ok(CliAction::Run {
                daemon: false,
                dry_run: true
            })
        ));
    }

    #[test]
    fn parse_args_unknown_flag_is_an_error() {
        for bad in ["--deamon", "--force", "-x", "--dry_run"] {
            let err = parse_args(&strs(&[bad])).unwrap_err();
            assert!(err.contains(bad), "error should name the unknown flag");
        }
    }

    #[test]
    fn parse_args_unknown_flag_rejected_even_with_valid_flags() {
        assert!(parse_args(&strs(&["--daemon", "--oops"])).is_err());
    }

    #[test]
    fn parse_args_daemon_and_dry_run() {
        assert!(matches!(
            parse_args(&strs(&["--daemon", "--dry-run"])),
            Ok(CliAction::Run {
                daemon: true,
                dry_run: true
            })
        ));
    }

    // ── help_text ────────────────────────────────────────────────────────────

    #[test]
    fn help_text_contains_all_flags_and_env_vars() {
        let text = help_text("git-bulk-clean");
        for flag in &[
            "--daemon",
            "--dry-run",
            "--list",
            "--version",
            "--help",
            "--generate-completions",
        ] {
            assert!(text.contains(flag), "help text missing flag {flag}");
        }
        for var in &[
            "MAINTENANCE_REPOS",
            "MAINTENANCE_GHQ_ENABLE",
            "MAINTENANCE_REFLOG_EXPIRE",
            "MAINTENANCE_AGGRESSIVE",
            "MAINTENANCE_INTERVAL",
            "MAINTENANCE_WORKERS",
            "MAINTENANCE_SKIP_SUBMODULES",
            "MAINTENANCE_SKIP_LFS",
            "MAINTENANCE_PRUNE_TAGS",
            "MAINTENANCE_PRUNE_BRANCHES",
            "MAINTENANCE_PROTECTED_BRANCHES",
        ] {
            assert!(text.contains(var), "help text missing env var {var}");
        }
    }

    // ── git() ────────────────────────────────────────────────────────────────

    #[test]
    fn git_fn_succeeds_on_valid_command() {
        let tmp = make_temp_git_repo("git_fn_status");
        assert!(git(tmp.to_str().unwrap(), &["status"]));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn git_fn_fails_on_invalid_subcommand() {
        assert!(!git(
            env!("CARGO_MANIFEST_DIR"),
            &["not-a-real-subcommand-xyz"]
        ));
    }

    // ── has_lfs ──────────────────────────────────────────────────────────────

    #[test]
    fn has_lfs_false_on_project_root() {
        assert!(!has_lfs(env!("CARGO_MANIFEST_DIR")));
    }

    #[test]
    fn has_lfs_true_when_filter_lfs_configured() {
        let tmp = make_temp_git_repo("has_lfs_true");
        Command::new("git")
            .args(["config", "filter.lfs.clean", "git-lfs clean -- %f"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        assert!(has_lfs(tmp.to_str().unwrap()));
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── phase functions ──────────────────────────────────────────────────────

    #[test]
    fn phase_fetch_on_repo_without_remotes_does_not_panic() {
        let tmp = make_temp_git_repo("phase_fetch");
        let _ = phase_fetch(tmp.to_str().unwrap(), false);
        let _ = phase_fetch(tmp.to_str().unwrap(), true);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_refs_on_fresh_repo_succeeds() {
        let tmp = make_temp_git_repo("phase_refs");
        assert!(phase_refs(tmp.to_str().unwrap(), "30.days.ago"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_objects_normal_on_fresh_repo_does_not_panic() {
        let tmp = make_temp_git_repo("phase_objects_normal");
        let _ = phase_objects_normal(tmp.to_str().unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_objects_aggressive_on_fresh_repo_succeeds() {
        let tmp = make_temp_git_repo("phase_objects_aggressive");
        assert!(phase_objects_aggressive(tmp.to_str().unwrap()));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_indices_on_fresh_repo_does_not_panic() {
        let tmp = make_temp_git_repo("phase_indices");
        let _ = phase_indices(tmp.to_str().unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_submodules_on_repo_without_submodules_does_not_panic() {
        let tmp = make_temp_git_repo("phase_submodules");
        let _ = phase_submodules(tmp.to_str().unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_lfs_on_repo_without_lfs_does_not_panic() {
        let tmp = make_temp_git_repo("phase_lfs");
        let _ = phase_lfs(tmp.to_str().unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── clean_repo ───────────────────────────────────────────────────────────

    #[test]
    fn clean_repo_dry_run_normal_returns_true() {
        let repo = RepoInfo {
            path: env!("CARGO_MANIFEST_DIR").to_string(),
            kind: RepoKind::Normal,
        };
        let opts = CleanOptions::from_config(&make_config(&[]));
        assert!(clean_repo(&repo, &opts, true, 1, 1));
    }

    #[test]
    fn clean_repo_dry_run_aggressive_returns_true() {
        let repo = RepoInfo {
            path: env!("CARGO_MANIFEST_DIR").to_string(),
            kind: RepoKind::Normal,
        };
        let opts = CleanOptions::from_config(&make_config(&[("MAINTENANCE_AGGRESSIVE", "true")]));
        assert!(clean_repo(&repo, &opts, true, 1, 1));
    }

    #[test]
    fn clean_repo_dry_run_bare_returns_true() {
        // Pretend the project root is a bare repo to exercise the bare branch
        let repo = RepoInfo {
            path: env!("CARGO_MANIFEST_DIR").to_string(),
            kind: RepoKind::Bare,
        };
        let opts = CleanOptions::from_config(&make_config(&[]));
        assert!(clean_repo(&repo, &opts, true, 1, 1));
    }

    #[test]
    fn clean_repo_live_on_temp_repo_does_not_panic() {
        let tmp = make_temp_git_repo("clean_repo_live");
        let repo = RepoInfo {
            path: tmp.to_str().unwrap().to_string(),
            kind: RepoKind::Normal,
        };
        let opts = CleanOptions::from_config(&make_config(&[("MAINTENANCE_SKIP_LFS", "true")]));
        let _ = clean_repo(&repo, &opts, false, 1, 1);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn clean_repo_live_with_prune_branches_does_not_panic() {
        let tmp = make_temp_git_repo("clean_repo_prune");
        let repo = RepoInfo {
            path: tmp.to_str().unwrap().to_string(),
            kind: RepoKind::Normal,
        };
        let opts = CleanOptions::from_config(&make_config(&[
            ("MAINTENANCE_PRUNE_BRANCHES", "true"),
            ("MAINTENANCE_SKIP_LFS", "true"),
        ]));
        let _ = clean_repo(&repo, &opts, false, 1, 1);
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── run_cycle ────────────────────────────────────────────────────────────

    #[test]
    fn run_cycle_no_repos_returns_zero_failures() {
        let stats = run_cycle(&make_config(&[]), false);
        assert_eq!(stats.failed, 0);
    }

    #[test]
    fn run_cycle_dry_run_returns_zero_failures() {
        let cfg = make_config(&[("MAINTENANCE_REPOS", env!("CARGO_MANIFEST_DIR"))]);
        let stats = run_cycle(&cfg, true);
        assert_eq!(stats.failed, 0);
    }

    #[test]
    fn run_cycle_live_on_temp_repo_does_not_panic() {
        let tmp = make_temp_git_repo("run_cycle_live");
        let cfg = make_config(&[
            ("MAINTENANCE_REPOS", tmp.to_str().unwrap()),
            ("MAINTENANCE_SKIP_LFS", "true"),
        ]);
        let _ = run_cycle(&cfg, false);
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── is_valid_reflog_expire ───────────────────────────────────────────────

    #[test]
    fn reflog_expire_valid_values_accepted() {
        for v in [
            "never",
            "30.days.ago",
            "7.days.ago",
            "1.year.ago",
            "2024-01-01",
            "90 days ago",
        ] {
            assert!(is_valid_reflog_expire(v), "expected valid for {v:?}");
        }
    }

    #[test]
    fn reflog_expire_dangerous_values_rejected() {
        // git parses these case-insensitively, so the guard must too
        for v in ["now", "all", "NOW", "All", " now "] {
            assert!(!is_valid_reflog_expire(v), "expected invalid for {v:?}");
        }
    }

    #[test]
    fn reflog_expire_never_accepted_case_insensitively() {
        for v in ["never", "NEVER", "Never"] {
            assert!(is_valid_reflog_expire(v), "expected valid for {v:?}");
        }
    }

    #[test]
    fn reflog_expire_invalid_chars_rejected() {
        assert!(!is_valid_reflog_expire("; rm -rf /"));
        assert!(!is_valid_reflog_expire("$(whoami)"));
        assert!(!is_valid_reflog_expire("1970-01-01T00:00:00Z")); // colons not allowed
    }

    #[test]
    fn config_reflog_expire_bad_value_falls_back() {
        let cfg = make_config(&[("MAINTENANCE_REFLOG_EXPIRE", "now")]);
        assert_eq!(cfg.reflog_expire, DEFAULT_REFLOG_EXPIRE);
    }

    // ── sanitize_for_log ─────────────────────────────────────────────────────

    #[test]
    fn sanitize_log_str_passes_normal_paths() {
        assert_eq!(
            sanitize_for_log("/home/user/repos/project"),
            "/home/user/repos/project"
        );
    }

    #[test]
    fn sanitize_log_str_replaces_newline() {
        assert_eq!(
            sanitize_for_log("path\nfake-log-entry"),
            "path?fake-log-entry"
        );
    }

    #[test]
    fn sanitize_log_str_replaces_ansi_escape() {
        assert_eq!(sanitize_for_log("\x1b[31mred\x1b[0m"), "?[31mred?[0m");
    }

    #[test]
    fn sanitize_log_str_replaces_carriage_return() {
        assert_eq!(sanitize_for_log("path\r\nwindows"), "path??windows");
    }

    // ── MAX_WORKERS clamp ────────────────────────────────────────────────────

    #[test]
    fn config_workers_above_max_is_clamped() {
        let cfg = make_config(&[("MAINTENANCE_WORKERS", "10000")]);
        assert_eq!(cfg.num_workers, MAX_WORKERS);
    }

    #[test]
    fn config_workers_at_max_is_accepted() {
        let cfg = make_config(&[("MAINTENANCE_WORKERS", &MAX_WORKERS.to_string())]);
        assert_eq!(cfg.num_workers, MAX_WORKERS);
    }

    // ── MAINTENANCE_PRUNE_BRANCHES / MAINTENANCE_PROTECTED_BRANCHES ──────────

    #[test]
    fn config_prune_branches_defaults_false() {
        let cfg = make_config(&[]);
        assert!(!cfg.prune_branches);
        assert!(cfg.protected_branches.is_empty());
    }

    #[test]
    fn config_prune_branches_enabled() {
        let cfg = make_config(&[("MAINTENANCE_PRUNE_BRANCHES", "true")]);
        assert!(cfg.prune_branches);
    }

    #[test]
    fn config_protected_branches_parsed() {
        let cfg = make_config(&[("MAINTENANCE_PROTECTED_BRANCHES", "main, develop , release")]);
        assert_eq!(cfg.protected_branches, ["main", "develop", "release"]);
    }

    #[test]
    fn config_protected_branches_filters_blank() {
        let cfg = make_config(&[("MAINTENANCE_PROTECTED_BRANCHES", "main,,develop")]);
        assert_eq!(cfg.protected_branches, ["main", "develop"]);
    }

    #[test]
    fn clean_options_mirrors_prune_branches_fields() {
        let cfg = make_config(&[
            ("MAINTENANCE_PRUNE_BRANCHES", "true"),
            ("MAINTENANCE_PROTECTED_BRANCHES", "main,develop"),
        ]);
        let opts = CleanOptions::from_config(&cfg);
        assert!(opts.prune_branches);
        assert_eq!(opts.protected_branches, ["main", "develop"]);
    }

    // ── detect_mainline ──────────────────────────────────────────────────────

    #[test]
    fn detect_mainline_falls_back_to_master_when_no_main() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = env::temp_dir().join(format!("git_bulk_clean_mainline_master_{}", id));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        // Force "master" so this test is unconditional regardless of init.defaultBranch
        Command::new("git")
            .args(["init", "-b", "master"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t.com"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        fs::write(tmp.join("f"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        assert_eq!(
            detect_mainline(tmp.to_str().unwrap()),
            Some("master".to_string())
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_mainline_none_when_no_main_or_master() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = env::temp_dir().join(format!("git_bulk_clean_mainline_none_{}", id));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        Command::new("git")
            .args(["init", "-b", "trunk"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        assert_eq!(detect_mainline(tmp.to_str().unwrap()), None);
        // With no mainline, branch pruning must be skipped, not failed
        assert!(phase_branches(tmp.to_str().unwrap(), &[]));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_mainline_falls_back_to_main_when_main_exists() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = env::temp_dir().join(format!("git_bulk_clean_mainline_main_{}", id));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        // Force "main" so there is no origin/HEAD but "main" exists locally
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t.com"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        fs::write(tmp.join("f"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        assert_eq!(
            detect_mainline(tmp.to_str().unwrap()),
            Some("main".to_string())
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── phase_branches ───────────────────────────────────────────────────────

    #[test]
    fn phase_branches_no_merged_branches_succeeds() {
        // A fresh repo with only one commit on master has no other merged branches
        let tmp = make_temp_git_repo("phase_branches_none");
        assert!(phase_branches(tmp.to_str().unwrap(), &[]));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_branches_deletes_merged_branch() {
        let tmp = make_temp_git_repo("phase_branches_delete");
        let dir = tmp.to_str().unwrap();
        // Detect the default branch name
        let mainline = detect_mainline(dir).unwrap();
        // Create a feature branch, commit, merge back, then run phase_branches
        Command::new("git")
            .args(["checkout", "-b", "feature/x"])
            .current_dir(dir)
            .output()
            .unwrap();
        fs::write(tmp.join("feature.txt"), "feature").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "feature"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", &mainline])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["merge", "feature/x", "--no-ff", "-m", "merge"])
            .current_dir(dir)
            .output()
            .unwrap();
        // feature/x is now merged into mainline
        let result = phase_branches(dir, &[]);
        assert!(result);
        // Verify feature/x is gone
        let branches = Command::new("git")
            .args(["branch"])
            .current_dir(dir)
            .output()
            .unwrap();
        let branch_list = String::from_utf8_lossy(&branches.stdout);
        assert!(
            !branch_list.contains("feature/x"),
            "merged branch should have been deleted"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_branches_skips_protected_branch() {
        let tmp = make_temp_git_repo("phase_branches_protected");
        let dir = tmp.to_str().unwrap();
        let mainline = detect_mainline(dir).unwrap();
        // Create a feature branch and merge it
        Command::new("git")
            .args(["checkout", "-b", "protected-branch"])
            .current_dir(dir)
            .output()
            .unwrap();
        fs::write(tmp.join("p.txt"), "p").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "p"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", &mainline])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["merge", "protected-branch", "--no-ff", "-m", "merge"])
            .current_dir(dir)
            .output()
            .unwrap();
        // Run with protected-branch in the protected list
        let protected = vec!["protected-branch".to_string()];
        phase_branches(dir, &protected);
        // Verify protected-branch still exists
        let branches = Command::new("git")
            .args(["branch"])
            .current_dir(dir)
            .output()
            .unwrap();
        let branch_list = String::from_utf8_lossy(&branches.stdout);
        assert!(
            branch_list.contains("protected-branch"),
            "protected branch should not have been deleted"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_branches_never_passes_flag_like_names() {
        // A ref literally named "-D" can be created via update-ref (bypassing
        // git branch's name validation). It must never reach `git branch -d`
        // as an argument, where it would be parsed as the force-delete flag.
        let tmp = make_temp_git_repo("phase_branches_flag_name");
        let dir = tmp.to_str().unwrap();
        Command::new("git")
            .args(["update-ref", "refs/heads/-D", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(phase_branches(dir, &[]));
        let branches = Command::new("git")
            .args(["for-each-ref", "--format=%(refname)", "refs/heads/"])
            .current_dir(dir)
            .output()
            .unwrap();
        let refs = String::from_utf8_lossy(&branches.stdout);
        assert!(
            refs.contains("refs/heads/-D"),
            "flag-like branch must be skipped, not deleted or misparsed"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn phase_branches_skips_currently_checked_out_branch() {
        // HEAD on a merged feature branch: git cannot delete it, so it must
        // be filtered out and the phase must still succeed.
        let tmp = make_temp_git_repo("phase_branches_checked_out");
        let dir = tmp.to_str().unwrap();
        let mainline = detect_mainline(dir).unwrap();
        Command::new("git")
            .args(["checkout", "-b", "feature/current"])
            .current_dir(dir)
            .output()
            .unwrap();
        fs::write(tmp.join("c.txt"), "c").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "c"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", &mainline])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["merge", "feature/current", "--no-ff", "-m", "merge"])
            .current_dir(dir)
            .output()
            .unwrap();
        // Go back to the (now merged) feature branch before pruning
        Command::new("git")
            .args(["checkout", "feature/current"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(phase_branches(dir, &[]));
        let branches = Command::new("git")
            .args(["branch"])
            .current_dir(dir)
            .output()
            .unwrap();
        let branch_list = String::from_utf8_lossy(&branches.stdout);
        assert!(
            branch_list.contains("feature/current"),
            "checked-out branch must survive pruning"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn help_text_contains_new_env_vars() {
        let text = help_text("git-bulk-clean");
        assert!(
            text.contains("MAINTENANCE_PRUNE_BRANCHES"),
            "help text missing MAINTENANCE_PRUNE_BRANCHES"
        );
        assert!(
            text.contains("MAINTENANCE_PROTECTED_BRANCHES"),
            "help text missing MAINTENANCE_PROTECTED_BRANCHES"
        );
    }

    // ── Shell ────────────────────────────────────────────────────────────────

    #[test]
    fn shell_parse_accepts_supported_shells() {
        assert_eq!(Shell::parse("bash"), Ok(Shell::Bash));
        assert_eq!(Shell::parse("zsh"), Ok(Shell::Zsh));
        assert_eq!(Shell::parse("fish"), Ok(Shell::Fish));
    }

    #[test]
    fn shell_parse_rejects_unknown_shell() {
        let err = Shell::parse("powershell").unwrap_err();
        assert!(err.contains("powershell"), "error should name the shell");
        assert!(err.contains("bash, zsh, fish"), "error should list choices");
    }

    #[test]
    fn shell_as_str_roundtrips_through_parse() {
        for shell in Shell::ALL {
            assert_eq!(Shell::parse(shell.as_str()), Ok(shell));
        }
    }

    #[test]
    fn shell_completion_script_is_shell_specific() {
        assert!(Shell::Bash.completion_script().contains("complete -F"));
        assert!(Shell::Zsh.completion_script().contains("#compdef"));
        assert!(Shell::Fish.completion_script().contains("complete -c"));
    }

    // ── RepoKind ─────────────────────────────────────────────────────────────

    #[test]
    fn repo_kind_from_is_bare_maps_both_variants() {
        assert_eq!(RepoKind::from_is_bare(true), RepoKind::Bare);
        assert_eq!(RepoKind::from_is_bare(false), RepoKind::Normal);
    }

    #[test]
    fn repo_kind_is_bare_and_label() {
        assert!(RepoKind::Bare.is_bare());
        assert!(!RepoKind::Normal.is_bare());
        assert_eq!(RepoKind::Bare.label(), "bare");
        assert_eq!(RepoKind::Normal.label(), "norm");
    }

    // ── NonZeroUsize workers ─────────────────────────────────────────────────

    #[test]
    fn config_workers_non_numeric_falls_back() {
        let cfg = make_config(&[("MAINTENANCE_WORKERS", "not-a-number")]);
        assert_eq!(cfg.num_workers, DEFAULT_WORKERS);
    }

    #[test]
    fn config_workers_valid_value_is_parsed() {
        let cfg = make_config(&[("MAINTENANCE_WORKERS", "3")]);
        assert_eq!(cfg.num_workers.get(), 3);
    }
}
