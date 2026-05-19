#compdef git-bulk-clean

_git_bulk_clean() {
    local -a opts
    opts=(
        '--daemon[run as a background daemon]'
        '--dry-run[show what would be done without making changes]'
        '--list[list repositories that would be cleaned]'
        '--version[print version information]'
        '-V[print version information]'
        '--help[print help information]'
        '-h[print help information]'
    )

    _arguments -s $opts
}

_git_bulk_clean "$@"
