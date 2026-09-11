# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.1] - 2026-09-12

### Security

- Remove `GIT_ASKPASS`, `SSH_ASKPASS`, and `SSH_ASKPASS_REQUIRE` from child Git
  processes. `GIT_TERMINAL_PROMPT=0` only suppresses git's own terminal
  fallback prompt; an askpass program inherited from the parent environment
  still runs and can hand a credential to an unattended fetch.
- Remove three more classes of inherited environment variables from child Git
  processes: `GIT_PROXY_COMMAND` (overrides `core.gitProxy` unconditionally,
  per git-config(1), so no config layer can neutralize it); `GIT_SSH_VARIANT`
  (can make git reinterpret the forced `core.sshCommand=ssh` argument shape
  as a different SSH client's syntax); and the `GIT_TRACE*` diagnostic family
  (`GIT_TRACE`, `GIT_TRACE2`, `GIT_TRACE2_EVENT`, `GIT_TRACE2_PERF`,
  `GIT_TRACE_CURL`, `GIT_TRACE_CURL_NO_DATA`, `GIT_TRACE_PACKET`,
  `GIT_TRACE_PACK_ACCESS`, `GIT_TRACE_PERFORMANCE`, `GIT_TRACE_SETUP`,
  `GIT_TRACE_SHALLOW`), which write protocol dumps, packet contents, and
  pack-access logs to a file path taken directly from the environment.

### Changed

- Consolidated every `GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n` construction behind
  one `with_git_config` helper, and moved `phase_branches`'s local `git
  branch --merged` onto the same fully hardened command used by fetch and
  push, removing the divergence between "read-only local" and
  "remote-touching" git invocations.
- Changed the crate-wide lint from `forbid(unsafe_code)` to
  `deny(unsafe_code)` with three narrowly scoped `#[allow(unsafe_code)]`
  sites, so a small, audited FFI island can call libc's `signal`, `kill`,
  `setpgid`, and `_exit` directly for SIGTERM/SIGINT handling. No dependency
  was added; unsafe code remains a hard compile error everywhere else.

### Fixed

- Fixed a test race where two concurrently running test processes (or a
  stray leftover one) could compute the same temporary git repository path
  and corrupt each other's fixtures, by mixing the process id into every
  generated temp-directory name.
- Fixed test setup depending on the developer's global git config: a
  `core.hooksPath` pointing at a commit scanner, or `commit.gpgsign=true`
  without a usable key, previously ran on every test commit. Test-only git
  invocations now pass `-c core.hooksPath=/dev/null -c commit.gpgsign=false`.
- Fixed child `git` processes (fetch, gc, ...) being left running after the
  daemon received SIGTERM or SIGINT. The daemon now puts itself in its own
  process group at startup and, on either signal, forwards SIGTERM to that
  whole group (`kill(0, SIGTERM)`) before exiting — an operator sending
  SIGTERM to the daemon's pid can now expect every git child it spawned to
  stop too, instead of continuing unattended.

## [0.6.0] - 2026-09-11

### Added

- Added `MAINTENANCE_PRUNE_WORKTREES` (Home Manager: `pruneWorktrees`): when
  enabled, deletes worktree directories that are fully merged into the
  mainline or idle for more than three days, skipping any worktree whose
  checked-out branch is in `MAINTENANCE_PROTECTED_BRANCHES`.
- Added `MAINTENANCE_CREDENTIAL_HELPERS` (Home Manager: `credentialHelpers`):
  when enabled, `git fetch` and `git lfs prune` use the credential helpers
  from git config instead of running with every helper reset, so HTTPS
  remotes that require authentication can be maintained unattended. Every
  other phase keeps the reset.

## [0.5.0] - 2026-07-26

### Changed

- Probe configured repositories concurrently while preserving deterministic
  output order, and use a bounded atomic work queue for repository maintenance.
- Canonicalize discovered repository paths before deduplication.
- Deduplicate linked worktrees by their shared Git common directory to prevent
  concurrent maintenance processes from contending for the same locks.
- Skip worktree, branch, and submodule maintenance for bare repositories.
- Skip incremental repacking when a repository has no pack to index after the
  loose-object maintenance task.
- Make dry-run output describe only the operations that would actually run.

### Security

- Remove repository-redirection and command-injection-sensitive `GIT_*`
  variables from child Git processes.
- Restrict reflog expiry values to ASCII letters, digits, dots, hyphens, and
  spaces, and reject the immediately destructive values `now` and `all`.
- Remove `GIT_CEILING_DIRECTORIES` from child Git processes so inherited
  discovery boundaries cannot hide configured repositories.

### Compatibility

- Reflog expiry expressions outside the documented safe subset, including ISO
  8601 timestamps containing colons, now emit a warning and fall back to
  `30.days.ago`. Use a supported relative expression or `YYYY-MM-DD` date.

### Performance

- Reduced the mean runtime of `--list` over 100 local bare repositories from
  498.2 ms to 132.3 ms (10 runs after 2 warmups on Apple Silicon), a 73.4%
  reduction and 3.77x speedup in the project benchmark fixture.

### Documentation

- Clarify dry-run behavior, the trusted-repository security boundary, release
  installation, Home Manager platform support, and contributor guidance.

## [0.4.0] - 2026-07-05

### Changed

- Made branch pruning opt-in and tag pruning opt-in by default.
- Protected branch arguments from option injection and added configurable
  protected branches.
- Improved mainline detection and branch-pruning safety checks.

## [0.3.0] - 2026-07-05

### Added

- Added macOS support through a Home Manager launchd agent while retaining the
  Linux systemd user service.

### Changed

- Made Cachix upload optional when its authentication token is unavailable.

## [0.2.0] - 2026-07-05

### Changed

- Updated the Rust crate to edition 2024 and introduced stricter domain types.
- Consolidated formatting, linting, build, and test validation under
  `nix flake check` and fixed issues exposed by that gate.

[Unreleased]: https://github.com/takeokunn/git-bulk-clean/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/takeokunn/git-bulk-clean/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/takeokunn/git-bulk-clean/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/takeokunn/git-bulk-clean/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/takeokunn/git-bulk-clean/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/takeokunn/git-bulk-clean/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/takeokunn/git-bulk-clean/releases/tag/v0.2.0
