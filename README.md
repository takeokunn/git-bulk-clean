# git-bulk-clean

> Parallel Git repository maintenance CLI and daemon — written in Rust, zero external dependencies.

[![CI](https://github.com/takeokunn/git-bulk-clean/actions/workflows/main.yml/badge.svg)](https://github.com/takeokunn/git-bulk-clean/actions/workflows/main.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)

---

If you manage dozens (or hundreds) of Git repositories locally — especially via [ghq](https://github.com/x-puri/ghq) — they slowly accumulate stale remote-tracking branches, loose objects, oversized reflogs, and orphaned worktrees.  
`git-bulk-clean` automatically traverses every repository and runs the full git housekeeping pipeline in parallel, keeping your local clones lean and fast.

## Contents

- [Features](#features)
- [Demo](#demo)
- [How it works](#how-it-works)
- [Cleanup pipeline](#cleanup-pipeline)
- [Installation](#installation)
- [Usage](#usage)
- [Configuration](#configuration)
- [Home Manager integration](#home-manager-integration)
- [Development](#development)
- [License](#license)

---

## Features

| | |
|---|---|
| **Parallel execution** | Configurable worker pool (default 5) processes repos concurrently |
| **ghq integration** | `MAINTENANCE_GHQ_ENABLE=true` automatically includes every `ghq` repo |
| **Bare repo aware** | Detects bare repositories; skips worktree/submodule phases that don't apply |
| **Submodule support** | Auto-detects `.gitmodules` and runs `sync` + `gc` recursively |
| **Git LFS support** | Auto-detects `filter.lfs` config and runs `git lfs prune` |
| **Daemon mode** | `--daemon` loops indefinitely with a configurable sleep interval |
| **Dry-run mode** | `--dry-run` shows every command without executing anything |
| **Repo discovery** | `--list` prints all discovered repositories with bare/normal indicator |
| **Zero dependencies** | Built entirely on Rust's standard library — no `Cargo.lock` bloat |
| **Nix-native** | Ships a flake with `buildRustPackage`, `wrapProgram`, dev shell, and Home Manager module |

---

## Demo

### Dry-run — see what would happen

```console
$ MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --dry-run
[git-bulk-clean 14:11:16] dry-run mode — no git commands will be executed
[git-bulk-clean 14:11:16] starting cycle: 1 repositories (0 bare), 5 workers
[git-bulk-clean 14:11:16] [1/1] cleaning: ~/ghq/github.com/takeokunn/git-bulk-clean
[git-bulk-clean 14:11:16]   (dry-run) git fetch --all --prune --prune-tags
[git-bulk-clean 14:11:16]   (dry-run) git pack-refs --all
[git-bulk-clean 14:11:16]   (dry-run) git worktree prune
[git-bulk-clean 14:11:16]   (dry-run) git reflog expire --expire=30.days.ago --all
[git-bulk-clean 14:11:16]   (dry-run) git maintenance run --task=loose-objects
[git-bulk-clean 14:11:16]   (dry-run) git maintenance run --task=incremental-repack
[git-bulk-clean 14:11:16]   (dry-run) git gc --auto
[git-bulk-clean 14:11:16]   (dry-run) git maintenance run --task=commit-graph
[git-bulk-clean 14:11:16] worker 0: done
[git-bulk-clean 14:11:16] worker 1: done
[git-bulk-clean 14:11:16] cycle complete — 1/1 ok, 0 failed
```

### List — inspect discovered repositories

```console
$ MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --list
norm  ~/ghq/github.com/foo/bar
norm  ~/ghq/github.com/foo/baz
bare  ~/ghq/github.com/foo/infra.git
```

### One-shot — clean everything once

```console
$ MAINTENANCE_GHQ_ENABLE=true git-bulk-clean
[git-bulk-clean 09:00:01] starting cycle: 42 repositories (2 bare), 5 workers
[git-bulk-clean 09:00:01] [1/42] cleaning: ~/ghq/github.com/foo/api
[git-bulk-clean 09:00:01] [2/42] cleaning: ~/ghq/github.com/foo/bar
[git-bulk-clean 09:00:03] [1/42] ~/ghq/github.com/foo/api: done in 2341ms (ok)
...
[git-bulk-clean 09:00:47] cycle complete — 42/42 ok, 0 failed
```

---

## How it works

```
main thread
  │
  ├─ collect_repos()          reads MAINTENANCE_REPOS + ghq list -p
  │    └─ detect_repo_kind()  one git call per path: validates git repo + bare flag
  │
  ├─ mpsc::channel ──────────────────────────────────────────────────────────
  │    sends RepoInfo { path, is_bare } for every discovered repo
  │
  ├─ worker 0 ─┐
  ├─ worker 1  │  each worker loops: lock receiver → recv → clean_repo → repeat
  ├─ worker 2  │  exits when channel is exhausted (Err on recv)
  ├─ worker 3  │
  └─ worker 4 ─┘
       │
       └─ clean_repo()
            phase_fetch         git fetch --all --prune --prune-tags
            phase_refs          pack-refs / worktree prune / reflog expire
            phase_objects       loose-objects / incremental-repack / gc
            phase_indices       commit-graph
            phase_submodules    (if .gitmodules && !bare)
            phase_lfs           (if filter.lfs configured)
```

The receiver is wrapped in `Arc<Mutex<Receiver<RepoInfo>>>` so all five workers safely share a single channel without copying the task list.

---

## Cleanup pipeline

Each repository runs through these phases in order. All phases are attempted even if an earlier one fails — errors are logged and the repo is counted as failed, but processing continues.

| # | Command | When |
|---|---------|------|
| 1 | `git fetch --all --prune --prune-tags` | always |
| 2 | `git pack-refs --all` | always |
| 3 | `git worktree prune` | always |
| 4 | `git reflog expire --expire=<REFLOG_EXPIRE> --all` | always |
| 5 | `git maintenance run --task=loose-objects` | always |
| 6 | `git maintenance run --task=incremental-repack` | normal mode |
| 6 | `git repack -a -d -f` | aggressive mode |
| 7 | `git gc --auto` | normal mode |
| 7 | `git gc --aggressive --prune=all` | aggressive mode |
| 8 | `git maintenance run --task=commit-graph` | always |
| 9 | `git submodule sync --recursive` + `foreach git gc --auto` | `.gitmodules` exists, non-bare |
| 10 | `git lfs prune` | `filter.lfs` configured in repo |

> **Why run `loose-objects` and `incremental-repack` before `gc`?**  
> `git gc --auto` only triggers when internal thresholds are exceeded.  
> The `maintenance` tasks run unconditionally, ensuring objects are always consolidated
> regardless of repo activity level.

---

## Installation

### Cargo

```sh
cargo install --git https://github.com/takeokunn/git-bulk-clean
```

### Nix — one-off run

```sh
nix run github:takeokunn/git-bulk-clean -- --help
```

### Nix — persistent install

```nix
# flake.nix
inputs.git-bulk-clean.url = "github:takeokunn/git-bulk-clean";

# home.nix
home.packages = [ inputs.git-bulk-clean.packages.${pkgs.system}.default ];
```

---

## Usage

### Synopsis

```
git-bulk-clean [OPTIONS]
```

All repository discovery and tuning is done via environment variables — there are no positional arguments.

---

### Options

| Flag | Description |
|------|-------------|
| _(none)_ | One-shot: clean every discovered repository and exit |
| `--daemon` | Loop forever, sleeping `MAINTENANCE_INTERVAL` seconds between cycles |
| `--dry-run` | Print every git command that would run — nothing is executed |
| `--list` | Print all discovered repositories (`norm` / `bare`) and exit |
| `--generate-completions SHELL` | Print the completion script for `bash`, `zsh`, or `fish` and exit |
| `-V`, `--version` | Print version and exit |
| `-h`, `--help` | Print a usage summary and exit |

Exit code is `0` on full success, `1` if any repository encountered errors.

---

### Common recipes

#### Explore before you run

Always start with `--list` and `--dry-run` to verify what will happen:

```sh
# See which repositories were discovered
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --list

# See every git command that would be executed, without running any
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --dry-run
```

#### One-shot: clean everything managed by ghq

```sh
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean
```

#### One-shot: clean a specific set of repositories

```sh
MAINTENANCE_REPOS=/path/to/repo1,/path/to/repo2 git-bulk-clean
```

#### Combine ghq and explicit paths

```sh
MAINTENANCE_GHQ_ENABLE=true \
  MAINTENANCE_REPOS=/path/to/extra/repo \
  git-bulk-clean
```

#### Run as a daemon (every 24 hours)

```sh
MAINTENANCE_GHQ_ENABLE=true git-bulk-clean --daemon
```

#### Run as a daemon every 6 hours with aggressive GC

```sh
MAINTENANCE_GHQ_ENABLE=true \
  MAINTENANCE_INTERVAL=21600 \
  MAINTENANCE_AGGRESSIVE=true \
  git-bulk-clean --daemon
```

`--aggressive` replaces `git gc --auto` with `git gc --aggressive --prune=all` and uses a full `git repack -a -d -f` instead of incremental repacking. Significantly slower, but produces the smallest possible pack files.

#### Use more parallel workers for a large collection

```sh
MAINTENANCE_GHQ_ENABLE=true MAINTENANCE_WORKERS=12 git-bulk-clean
```

Default is 5 workers. Set higher if your disk I/O can handle it.

#### Skip submodules or LFS for a quick pass

```sh
MAINTENANCE_GHQ_ENABLE=true \
  MAINTENANCE_SKIP_SUBMODULES=true \
  MAINTENANCE_SKIP_LFS=true \
  git-bulk-clean
```

#### Set a longer reflog expiry

```sh
MAINTENANCE_GHQ_ENABLE=true MAINTENANCE_REFLOG_EXPIRE=90.days.ago git-bulk-clean
```

Any date string accepted by git works: `90.days.ago`, `2024-01-01`, `never`.

---

### Shell completions

Source directly (fish):

```sh
git-bulk-clean --generate-completions fish | source
```

Install permanently:

```sh
# bash
git-bulk-clean --generate-completions bash > ~/.bash_completion.d/git-bulk-clean

# zsh
git-bulk-clean --generate-completions zsh > ~/.zsh/completions/_git-bulk-clean

# fish
git-bulk-clean --generate-completions fish > ~/.config/fish/completions/git-bulk-clean.fish
```

When installed via Nix (`nix build` or the Home Manager module), completion files are installed automatically into the correct locations — no manual step required.

---

## Configuration

All configuration is via environment variables — no config file required.

| Variable | Default | Description |
|----------|---------|-------------|
| `MAINTENANCE_REPOS` | _(empty)_ | Comma-separated absolute paths to repositories |
| `MAINTENANCE_GHQ_ENABLE` | `false` | `true` → also include all repos from `ghq list -p` |
| `MAINTENANCE_REFLOG_EXPIRE` | `30.days.ago` | Cutoff passed to `git reflog expire --expire` |
| `MAINTENANCE_AGGRESSIVE` | `false` | `true` → full repack + `gc --aggressive --prune=all` |
| `MAINTENANCE_INTERVAL` | `86400` | Daemon sleep between cycles (seconds) |
| `MAINTENANCE_WORKERS` | `5` | Parallel worker threads (0 falls back to default) |
| `MAINTENANCE_SKIP_SUBMODULES` | `false` | `true` → skip submodule sync/gc even if `.gitmodules` exists |
| `MAINTENANCE_SKIP_LFS` | `false` | `true` → skip `git lfs prune` even if LFS is configured |

Paths listed in `MAINTENANCE_REPOS` that do not exist or are not git repositories are silently ignored.

---

## Home Manager integration

`git-bulk-clean` ships a [Home Manager](https://github.com/nix-community/home-manager) module that registers a `systemd` user service running the daemon at **idle CPU and I/O priority** (`Nice=19`, `IOSchedulingClass=idle`) so it never competes with your active work.

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    home-manager.url = "github:nix-community/home-manager";
    git-bulk-clean.url = "github:takeokunn/git-bulk-clean";
  };

  outputs = { nixpkgs, home-manager, git-bulk-clean, ... }: {
    homeConfigurations.yourname = home-manager.lib.homeManagerConfiguration {
      pkgs = nixpkgs.legacyPackages.aarch64-darwin;
      modules = [
        git-bulk-clean.homeManagerModules.default
        {
          services.git-maintenance = {
            enable = true;

            # include every repo managed by ghq
            ghq.enable = true;

            # plus any extra paths
            repositories = [
              "/path/to/extra/repo"
            ];

            interval    = 86400;          # seconds between cycles
            reflogExpire = "30.days.ago";
            aggressive  = false;
          };
        }
      ];
    };
  };
}
```

After `home-manager switch`, the service starts automatically on login:

```sh
systemctl --user status git-maintenance
systemctl --user journal -f git-maintenance
```

### Available options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `bool` | `false` | Enable the service |
| `ghq.enable` | `bool` | `false` | Include all `ghq` repos |
| `repositories` | `[str]` | `[]` | Extra repo paths |
| `interval` | `int` | `86400` | Cycle interval in seconds |
| `reflogExpire` | `str` | `"30.days.ago"` | Reflog expiry cutoff |
| `aggressive` | `bool` | `false` | Use aggressive GC mode |

---

## Development

```sh
# Enter the Nix dev shell (cargo, rustc, clippy, rustfmt, git, ghq)
nix develop

# Build
cargo build

# Run tests
cargo test

# Lint
cargo clippy

# Format
cargo fmt

# Check flake outputs
nix flake check

# Build the Nix package
nix build

# Run directly via Nix
nix run . -- --help
```

### Project structure

```
git-bulk-clean/
├── src/main.rs       # All source — stdlib only, no external crates
├── Cargo.toml
├── Cargo.lock
├── flake.nix         # Nix package, dev shell, app, homeManagerModules
├── hm-module.nix     # Home Manager module (systemd user service)
└── .github/
    ├── workflows/
    │   ├── ci.yml    # actionlint + nix flake check + nix build + cargo test
    │   └── main.yml  # triggers on push to main
    └── dependabot.yml
```

---

## License

MIT — see [LICENSE](LICENSE).
