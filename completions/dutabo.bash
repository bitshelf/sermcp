# dutabo bash completion
_dutabo() {
    local cur prev opts
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    commands="list names status serial reboot uboot button uf flash-kernel init agent"

    case "${prev}" in
        --dut)
            # Read DUT names from .target.jsonc (canonical format).
            local names=$(grep -oP '"dut_name"\s*:\s*"\K[^"]+' .target.jsonc 2>/dev/null)
            COMPREPLY=( $(compgen -W "${names}" -- "${cur}") )
            return 0
            ;;
        uf|flash-kernel)
            COMPREPLY=( $(compgen -f -- "${cur}") )
            return 0
            ;;
        agent)
            COMPREPLY=( $(compgen -W "deploy --select --project --agent --vendor --os --offline --force" -- "${cur}") )
            return 0
            ;;
        dutabo)
            COMPREPLY=( $(compgen -W "${commands} --dut --mcp-port" -- "${cur}") )
            return 0
            ;;
    esac

    if [[ ${cur} == -* ]]; then
        COMPREPLY=( $(compgen -W "--dut --mcp-port" -- "${cur}") )
    else
        COMPREPLY=( $(compgen -W "${commands}" -- "${cur}") )
    fi
}
complete -F _dutabo dutabo
