# git-bulk-clean

A parallel Git repository maintenance CLI/daemon written in Rust.

Traverses all configured Git repositories and runs a comprehensive cleanup pipeline — fetch, pack, reflog expiry, object repack, commit-graph, submodule GC, and LFS pruning — using a configurable worker pool.

[![CI](https://github.com/takeokunn/git-bulk-clean/actions/workflows/main.yml/badge.svg)](https://github.com/takeokunn/git-bulk-clean/actions/workflows/main.yml)

## Features

- **5-worker parallel pool** — configurable via `MAINTENANCE_WORKERS`
- **ghq integration** — automatically includes all `ghq`-managed repositories
- **Bare repo aware** — detects bare repositories and skips inapplicable phases
- **Git LFS support** — runs `git lfs prune` when LFS is configured in a repo
- **Submodule support** — syncs and GCs submodules automatically
- **Daemon mode** — loops indefinitely with a configurable sleep interval
- **Dry-run mode** — prints every command that would run without executing
- **Zero external crates** — built entirely on Rust's standard library

## Cleanup pipeline

Each repository is processed through these steps in order:

| # | Command | Condition |
|---|---------|-----------|
| 1 | `git fetch --all --prune --prune-tags` | always |
| 2 | `git pack-refs --all` | always |
| 3 | `git worktree prune` | always |
| 4 | `git reflog expire --expire=<REFLOG_EXPIRE> --all` | always |
| 5 | `git maintenance run --task=loose-objects` | always |
| 6 | `git maintenance run --task=incremental-repack` | normal mode |
| 6 | `git repack -a -d -f --delta-base-offset` | aggressive mode |
| 7 | `git gc --auto` | normal mode |
| 7 | `git gc --aggressive --prune=all` | aggressive mode |
| 8 | `git maintenance run --task=commit-graph` | always |
| 9 | `git submodule sync` + `git submodule foreach gc` | `.gitmodules` present, non-bare |
| 10 | `git lfs prune` | LFS configured in repo |

## Installation

### Cargo

```sh
cargo install --git https://github.com/takeokunn/git-bulk-clean
```

### Nix (flakes)

```sh
nix run github:takeokunn/git-bulk-clean
```

Add to your `flake.nix` inputs:

```nix
inputs.git-bulk-clean.url = "github:takeokunn/git-bulk-clean";
```

### Home Manager

```nix
# flake.nix
inputs.git-bulk-clean.url = "github:takeokunn/git-bulk-clean";

# home.nix
imports = [ inputs.git-bulk-clean.homeManagerModules.default ];

services.git-maintenance = {
  enable = true;
  ghq.enable = true;
  repositories = [ "/path/to/extra/repo" ];
  interval = 86400;     # seconds between cycles (daemon mode)
  reflogExpire = "30.days.ago";
  aggressive = false;
};
```

This installs the binary and registers a `systemd` user service that runs the daemon at idle CPU and I/O priority.

## Usage

```sh
# One-shot: clean all configured repositories once
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean

# Preview what would run without executing
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --dry-run

# List discovered repositories
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --list

# Daemon mode: clean continuously
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --daemon
```

## Configuration

All configuration is via environment variables.

| Variable | Default | Description |
|----------|---------|-------------|
| `MAINTENANCE_REPOS` | _(empty)_ | Comma-separated list of repository paths |
| `MAINTENANCE_GHQ_ENABLE` | `false` | Include all `ghq`-managed repositories |
| `MAINTENANCE_REFLOG_EXPIRE` | `30.days.ago` | Passed to `git reflog expire --expire` |
| `MAINTENANCE_AGGRESSIVE` | `false` | Use full repack + `gc --aggressive` |
| `MAINTENANCE_INTERVAL` | `86400` | Seconds to sleep between cycles (daemon) |
| `MAINTENANCE_WORKERS` | `5` | Number of parallel worker threads |
| `MAINTENANCE_SKIP_SUBMODULES` | `false` | Skip submodule cleanup |
| `MAINTENANCE_SKIP_LFS` | `false` | Skip `git lfs prune` |

## Development

```sh
# Enter the dev shell (provides cargo, rustc, clippy, rustfmt, git, ghq)
nix develop

# Build and test
cargo build
cargo test

# Lint and format
cargo clippy
cargo fmt
```

## License

MIT — see [LICENSE](LICENSE).
