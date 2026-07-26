# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/takeokunn/git-bulk-clean/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/takeokunn/git-bulk-clean/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/takeokunn/git-bulk-clean/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/takeokunn/git-bulk-clean/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/takeokunn/git-bulk-clean/releases/tag/v0.2.0
