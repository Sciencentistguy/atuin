set -gx ATUIN_SESSION (atuin uuid)

function _atuin_preexec --on-event fish_preexec
    if not test -n "$fish_private_mode"
        set -gx ATUIN_HISTORY_ID (atuin history start -- "$argv[1]")
    end
end

function _atuin_postexec --on-event fish_postexec
    set s $status
    if test -n "$ATUIN_HISTORY_ID"
        RUST_LOG=error atuin history end --exit $s -- $ATUIN_HISTORY_ID &>/dev/null &
        disown
    end
end

function _atuin_search
    set h (RUST_LOG=error atuin search $* -i -- (commandline -b) 3>&1 1>&2 2>&3)
    commandline -f repaint
    if test -n "$h"
        commandline -r $h
    end
end

function _atuin_bind_up
    # Fallback to fish's builtin up-or-search if we're in search or paging mode
    if commandline --search-mode; or commandline --paging-mode
        up-or-search
        return
    end

    # Only invoke atuin if we're on the top line of the command
    set -l lineno (commandline --line)
    switch $lineno
        case 1
            _atuin_search --shell-up-key-binding
        case '*'
            up-or-search
    end
end

if test -z $ATUIN_NOBIND
    bind \cr _atuin_search
    bind -k up _atuin_bind_up
    bind \eOA _atuin_bind_up
    bind \e\[A _atuin_bind_up


    if bind -M insert > /dev/null 2>&1
        bind -M insert \cr _atuin_search
        bind -M insert -k up _atuin_bind_up
        bind -M insert \eOA _atuin_bind_up
        bind -M insert \e\[A _atuin_bind_up
    end
end
