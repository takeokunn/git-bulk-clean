# Contributing

Contributions are welcome through focused issues and pull requests. Security
reports must follow [SECURITY.md](SECURITY.md), not the public issue tracker.

## Development environment

The supported development environment is the repository's Nix development
shell, which provides Rust, Cargo, rustfmt, Clippy, Git, and ghq:

```sh
nix develop
```

A standalone stable Rust toolchain may be used for local iteration, but the Nix
checks remain the reproducible project gate.

## Making changes

1. Create a topic branch from the latest `main`; do not commit directly to
   `main`.
2. Keep each change scoped and add or update tests for behavior changes.
3. Update README, man-page, and changelog text when user-visible behavior
   changes.
4. Avoid adding dependencies unless the benefit and supply-chain cost are
   justified.

## Required checks

Run these before opening a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
nix flake check --print-build-logs
cargo package --allow-dirty
```

`cargo package --allow-dirty` verifies package assembly without publishing it.
The project is not currently published on crates.io.

## Pull requests

Open a pull request from the topic branch to `main`. Describe the problem,
behavioral and compatibility impact, security or data-loss implications, and
the checks you ran. Keep unrelated refactoring out of the same pull request.
Address review findings with additional commits rather than rewriting shared
history unless a maintainer asks otherwise.

By contributing, you agree that your contribution is licensed under the
project's MIT license.
