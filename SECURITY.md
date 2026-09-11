# Security Policy

## Supported versions

The latest released minor line is the supported security-reporting scope. At
present, that is the `0.6.x` release line. Older releases are unsupported and
may require upgrading for any available fix. The unreleased `main` branch may
contain additional fixes but is not a supported release.

## Reporting a vulnerability

Please report vulnerabilities [privately through GitHub Security
Advisories](https://github.com/takeokunn/git-bulk-clean/security/advisories/new).
Do not disclose a suspected vulnerability in a public issue, discussion, or
pull request.

If private vulnerability reporting is unavailable for this repository, avoid
publishing exploit details. There is no separately documented security contact;
open a minimal public issue asking the maintainer to enable a private reporting
channel, without including sensitive technical details.

Include the affected version, operating system, Git version, reproduction
steps, impact, and any suggested mitigation. Allow the maintainer time to
confirm the report and coordinate disclosure. No response-time commitment is
currently offered.

## Security scope and trust boundary

`git-bulk-clean` invokes Git, ghq, and optionally Git LFS against configured
repositories. It is intended only for repositories owned and managed by the
current operating-system user. Repository paths, `.git` directories, Git
configuration, objects, worktrees, remotes, and configured helper programs must
all be trusted.

Shared repositories, repositories writable by other users or untrusted
automation, and repositories containing untrusted credential, transport,
filter, diff, or LFS helper configuration are outside the supported security
boundary. Dry-run does not sandbox these inputs: it skips repository-changing
maintenance commands but still invokes Git for read-only repository, mainline,
and LFS detection and checks the filesystem for submodule metadata.

The tool intentionally performs destructive maintenance. Depending on options,
it can expire reflogs, prune unreachable objects and LFS cache objects, remove
stale worktree metadata, delete local tags absent from remotes, and delete
merged local branches. Loss caused by running against untrusted or incorrectly
selected repositories is not considered a vulnerability by itself. Unexpected
deletion outside the documented behavior, argument injection, privilege-boundary
bypass, or command execution caused by data that should be treated as inert is
in scope.

Keep independent backups of irreplaceable work and review `--list` and
`--dry-run` output before changing repositories.
