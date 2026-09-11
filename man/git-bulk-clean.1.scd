git-bulk-clean(1)

# NAME

git-bulk-clean - parallel Git repository maintenance CLI/daemon

# SYNOPSIS

*git-bulk-clean* [--daemon] [--dry-run] [--list] [--version] [--help] [--generate-completions bash|zsh|fish]

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

Bare repositories are detected automatically. Worktree pruning, branch pruning,
and submodule operations, which require a working tree, are skipped for them.

Run git-bulk-clean only on repositories owned and managed by the current
operating-system user. Repository discovery is not a security scanner or
sandbox. Do not process repositories whose Git directory, configuration,
objects, worktrees, or configured helper programs can be modified by another
user or by untrusted automation.

# OPTIONS

*--daemon*
	Loop forever, sleeping _MAINTENANCE_INTERVAL_ seconds between cleanup
	cycles. Send SIGINT or SIGTERM to stop (see SIGNALS).

*--dry-run*
	Print repository-changing Git commands without running them. Read-only
	detection commands still run to validate repositories and determine bare
	status, mainline branches, and LFS configuration. Useful for auditing the
	maintenance plan before a changing run.

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
	Cutoff date passed to _git reflog expire --expire_. Values may contain
	ASCII letters, digits, dots, hyphens, and spaces. _never_ is accepted;
	_now_ and _all_ are rejected. Other Git date syntax, including ISO 8601
	timestamps containing colons, is replaced with the default after a warning.
	Default: _30.days.ago_.

*MAINTENANCE_AGGRESSIVE*
	Set to _true_ to use the aggressive repack strategy.
	Replaces the incremental-repack task with a full
	_git repack -a -d -f_ and replaces
	_git gc --auto_ with _git gc --aggressive --prune=all_.
	Significantly slower but produces smaller packs. Default: _false_.

*MAINTENANCE_INTERVAL*
	Number of seconds git-bulk-clean sleeps between cleanup cycles when
	running in daemon mode (--daemon). Zero or invalid values fall back to
	the default. Default: _86400_ (24 hours).

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

*MAINTENANCE_PRUNE_TAGS*
	Set to _true_ to pass _--prune-tags_ to _git fetch_, deleting local
	tags that no longer exist on the remote. *Warning:* this also removes
	tags created locally and never pushed. Default: _false_.

*MAINTENANCE_PRUNE_BRANCHES*
	Set to _true_ to delete local branches that have been merged into the
	mainline. The mainline is auto-detected from _origin/HEAD_; if that is
	not set, _main_ is tried, then _master_. If no mainline can be found,
	branch pruning is skipped for that repository. The currently checked-out
	branch and branches checked out in other worktrees are never deleted.
	Only runs on non-bare repositories. Default: _false_.

*MAINTENANCE_PRUNE_WORKTREES*
	Set to _true_ to delete linked worktree directories whose checked-out
	commit is merged into the mainline, or whose last commit is more than
	three days old. The primary worktree is never a candidate, and a
	worktree checked out to a branch listed in
	_MAINTENANCE_PROTECTED_BRANCHES_ is skipped. Never passes _--force_,
	so a worktree with uncommitted changes is left alone by git itself.
	Default: _false_.

*MAINTENANCE_PROTECTED_BRANCHES*
	Comma-separated list of branch names to never delete, even when
	_MAINTENANCE_PRUNE_BRANCHES=true_, and whose worktrees are never
	removed by _MAINTENANCE_PRUNE_WORKTREES_. The detected mainline branch
	is always protected regardless of this setting. Default: _(empty)_.

*MAINTENANCE_CREDENTIAL_HELPERS*
	Set to _true_ to let _git fetch_ and _git lfs prune_ use the credential
	helpers configured in git config, so HTTPS remotes that require
	authentication can be maintained unattended. When _false_, every helper
	is reset for the duration of each maintenance command and such remotes
	fail to fetch. Terminal prompts stay disabled either way. Default:
	_false_.

# CLEANUP PIPELINE

Each repository is processed through the following phases in order. All
phases are attempted even if an earlier one fails — errors are logged and
the repository is counted as failed, but processing continues with the
remaining phases and repositories.

. *git fetch --all --prune* — fetch from all remotes, removing stale remote-tracking branches. Adds _--prune-tags_ when _MAINTENANCE_PRUNE_TAGS=true_. Uses the configured credential helpers only when _MAINTENANCE_CREDENTIAL_HELPERS=true_.
. *git pack-refs --all* — pack loose reference files into a single packed-refs file.
. *git worktree prune* — remove stale worktree administrative files. Skipped for bare repositories.
. *git worktree remove -- <stale>* — delete worktree directories merged into the mainline or idle for three days. Only runs when _MAINTENANCE_PRUNE_WORKTREES=true_.
. *git reflog expire --expire=<MAINTENANCE_REFLOG_EXPIRE> --all* — expire old reflog entries according to the configured cutoff.
. *git rerere gc* — clean up stale conflict-resolution records from the rerere cache. Always runs; no-op on repositories that do not use rerere.
. *git notes prune* — remove note refs whose annotated objects no longer exist. Always runs.
. *git branch -d -- <merged>* — delete local branches fully merged into the detected mainline. Only runs when _MAINTENANCE_PRUNE_BRANCHES=true_ and the repository is not bare.
. *git maintenance run --task=loose-objects* — consolidate loose objects unconditionally (unlike gc --auto, this does not check thresholds).
. *git maintenance run --task=incremental-repack* (normal) or *git repack -a -d -f* (aggressive) — pack objects for efficient storage. In normal mode, incremental repacking is skipped when no pack file is available after the loose-objects task.
. *git gc --auto* (normal) or *git gc --aggressive --prune=all* (aggressive) — run garbage collection.
. *git maintenance run --task=commit-graph* — write or update the commit-graph file for faster graph traversal.
. *git submodule sync --recursive* and *git submodule foreach git gc --auto* — synchronise submodule URLs and compact their object stores. Skipped when _.gitmodules_ is absent, the repository is bare, or _MAINTENANCE_SKIP_SUBMODULES=true_.
. *git lfs prune* — remove locally cached LFS objects that are no longer referenced. Skipped when _filter.lfs_ is not configured or _MAINTENANCE_SKIP_LFS=true_. Uses the configured credential helpers only when _MAINTENANCE_CREDENTIAL_HELPERS=true_.

# EXIT STATUS

*0*
	All repositories were cleaned successfully.

*1*
	One or more repositories encountered errors during processing.

*2*
	The command line contains an invalid option or argument.

# SIGNALS

On startup, git-bulk-clean places itself in its own process group. On
SIGINT or SIGTERM, it forwards SIGTERM to that whole group — _kill(0,
SIGTERM)_ — before exiting, so a git child that is mid-fetch or mid-gc when
the signal arrives is stopped along with the daemon rather than left
running unattended. Sending SIGTERM to the daemon's own pid is therefore
enough to stop every git process it spawned; no separate cleanup of child
processes is needed.

# SECURITY

Maintenance can permanently remove reflog entries, unreachable objects, stale
worktree metadata, LFS cache objects, local tags when tag pruning is enabled,
and merged local branches when branch pruning is enabled. Keep independent
backups of irreplaceable work and inspect both *--list* and *--dry-run* before
the first changing run.

Dry-run prevents maintenance commands from changing repositories, but it is
not a sandbox: Git is still invoked for read-only repository, mainline, and LFS
detection, the filesystem is checked for submodule metadata, and repository-local
configuration may select helper programs. Do not target
shared, other-user-writable, or otherwise untrusted repositories. With
_MAINTENANCE_CREDENTIAL_HELPERS=true_, each repository's git config also
selects the credential helper that runs during fetch and LFS prune, and a
helper value beginning with *!* is executed as a shell command, so enable
it only for repositories whose configuration you control. See the
project's SECURITY.md for vulnerability reporting and supported versions.

# EXAMPLES

Preview changing commands across all ghq-managed repositories while allowing
read-only detection commands to run:

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
