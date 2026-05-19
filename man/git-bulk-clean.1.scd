git-bulk-clean(1)

# NAME

git-bulk-clean - parallel Git repository maintenance CLI/daemon

# SYNOPSIS

*git-bulk-clean* [--daemon] [--dry-run] [--list] [--version] [--help]
               [--generate-completions bash|zsh|fish]

# DESCRIPTION

git-bulk-clean traverses every configured Git repository and runs a full
housekeeping pipeline in parallel, keeping local clones lean and fast.

If you manage dozens or hundreds of Git repositories locally — especially
via ghq(1) — they slowly accumulate stale remote-tracking branches, loose
objects, oversized reflogs, and orphaned worktrees. git-bulk-clean
automates all housekeeping steps and can run as a long-lived daemon that
wakes up on a configurable interval.

Repositories are discovered from the _MAINTENANCE_REPOS_ environment
variable, from _ghq list -p_ (when _MAINTENANCE_GHQ_ENABLE=true_), or
both. Paths that do not exist or are not valid Git repositories are
silently ignored.

Bare repositories are detected automatically; phases that require a
working tree (worktree prune, submodule operations) are skipped for them.

# OPTIONS

*--daemon*
	Loop forever, sleeping _MAINTENANCE_INTERVAL_ seconds between cleanup
	cycles. Send SIGINT or SIGTERM to stop.

*--dry-run*
	Print every git command that would be executed without actually running
	any of them. Useful for auditing what git-bulk-clean will do before
	committing.

*--list*
	Print all discovered repositories with their type (_norm_ for a normal
	working-tree clone, _bare_ for a bare repository) and exit.

*--version*, *-V*
	Print the program version and exit.

*--help*, *-h*
	Print a short usage summary and exit.

*--generate-completions* _shell_
	Print the shell completion script for _shell_ (one of _bash_, _zsh_,
	or _fish_) to standard output and exit. Intended for shell
	configuration:

	```
	git-bulk-clean --generate-completions fish | source
	```

# ENVIRONMENT

*MAINTENANCE_REPOS*
	Comma-separated list of absolute paths to Git repositories. Each path
	is validated; invalid or non-existent paths are silently ignored.

*MAINTENANCE_GHQ_ENABLE*
	Set to _true_ to include all repositories returned by _ghq list -p_ in
	addition to those listed in _MAINTENANCE_REPOS_. Default: _false_.

*MAINTENANCE_REFLOG_EXPIRE*
	Cutoff date passed to _git reflog expire --expire_. Accepts any value
	that git understands as a date (e.g. _90.days.ago_, _2024-01-01_).
	Default: _30.days.ago_.

*MAINTENANCE_AGGRESSIVE*
	Set to _true_ to use the aggressive repack strategy.
	Replaces the incremental-repack task with a full
	_git repack -a -d -f --delta-base-offset_ and replaces
	_git gc --auto_ with _git gc --aggressive --prune=all_.
	Significantly slower but produces smaller packs. Default: _false_.

*MAINTENANCE_INTERVAL*
	Number of seconds git-bulk-clean sleeps between cleanup cycles when
	running in daemon mode (--daemon). Default: _86400_ (24 hours).

*MAINTENANCE_WORKERS*
	Number of parallel worker threads used to process repositories
	concurrently. Setting this to _0_ falls back to the default.
	Default: _5_.

*MAINTENANCE_SKIP_SUBMODULES*
	Set to _true_ to skip the submodule sync and gc phase even when a
	_.gitmodules_ file is present. Default: _false_.

*MAINTENANCE_SKIP_LFS*
	Set to _true_ to skip _git lfs prune_ even when the repository has
	_filter.lfs_ configured. Default: _false_.

# CLEANUP PIPELINE

Each repository is processed through the following phases in order. All
phases are attempted even if an earlier one fails — errors are logged and
the repository is counted as failed, but processing continues with the
remaining phases and repositories.

. *git fetch --all --prune --prune-tags* — fetch from all remotes, remove
  stale remote-tracking branches and tags.
. *git pack-refs --all* — pack loose reference files into a single
  packed-refs file.
. *git worktree prune* — remove stale worktree administrative files
  (skipped for bare repositories).
. *git reflog expire --expire=<MAINTENANCE_REFLOG_EXPIRE> --all* — expire
  old reflog entries according to the configured cutoff.
. *git maintenance run --task=loose-objects* — consolidate loose objects
  unconditionally (unlike gc --auto this does not check thresholds).
. *git maintenance run --task=incremental-repack* (normal mode) or
  *git repack -a -d -f --delta-base-offset* (aggressive mode) — pack
  objects for efficient storage.
. *git gc --auto* (normal mode) or *git gc --aggressive --prune=all*
  (aggressive mode) — run garbage collection.
. *git maintenance run --task=commit-graph* — write or update the
  commit-graph file for faster graph traversal.
. *git submodule sync --recursive* and *git submodule foreach git gc
  --auto* — synchronise submodule URLs and compact their object stores.
  Skipped when _.gitmodules_ is absent, the repository is bare, or
  _MAINTENANCE_SKIP_SUBMODULES=true_.
. *git lfs prune* — remove locally cached LFS objects that are no longer
  referenced. Skipped when _filter.lfs_ is not configured in the
  repository or _MAINTENANCE_SKIP_LFS=true_.

# EXIT STATUS

*0*
	All repositories were cleaned successfully.

*1*
	One or more repositories encountered errors during processing.

# EXAMPLES

Print every command that would run across all ghq-managed repositories
without executing anything:

```
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --dry-run
```

Show all discovered repositories before running:

```
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --list
```

Clean a specific set of repositories once using 8 parallel workers:

```
MAINTENANCE_REPOS=/path/to/repo1,/path/to/repo2 MAINTENANCE_WORKERS=8 git-bulk-clean
```

Run as a daemon that cleans all ghq repositories every 12 hours using the
aggressive GC strategy:

```
MAINTENANCE_GHQ_ENABLE=true MAINTENANCE_INTERVAL=43200 MAINTENANCE_AGGRESSIVE=true git-bulk-clean --daemon
```

# SEE ALSO

*git*(1), *ghq*(1)

# AUTHORS

takeokunn
