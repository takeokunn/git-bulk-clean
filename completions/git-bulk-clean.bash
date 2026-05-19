# bash completion for git-bulk-clean

_git_bulk_clean() {
    local cur prev words cword
    _init_completion 2>/dev/null || {
        cur="${COMP_WORDS[COMP_CWORD]}"
    }

    local opts="--daemon --dry-run --list --version -V --help -h"

    COMPREPLY=($(compgen -W "${opts}" -- "${cur}"))
}

complete -F _git_bulk_clean git-bulk-clean
