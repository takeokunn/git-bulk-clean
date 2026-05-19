# fish completion for git-bulk-clean

complete -c git-bulk-clean -f

complete -c git-bulk-clean -l daemon   -d 'Run as a background daemon'
complete -c git-bulk-clean -l dry-run  -d 'Show what would be done without making changes'
complete -c git-bulk-clean -l list     -d 'List repositories that would be cleaned'
complete -c git-bulk-clean -l version  -d 'Print version information'
complete -c git-bulk-clean -s V        -d 'Print version information'
complete -c git-bulk-clean -l help     -d 'Print help information'
complete -c git-bulk-clean -s h        -d 'Print help information'
