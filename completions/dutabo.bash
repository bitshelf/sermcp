# shellcheck shell=bash
# Bash completion for dutabo.
_dutabo() {
    local cur="${COMP_WORDS[COMP_CWORD]}" cmd='' value='' word opts candidates
    local -a positional=()
    local i
    COMPREPLY=()

    # Global options can appear before the command or between its arguments.
    for ((i = 1; i < COMP_CWORD; i++)); do
        word="${COMP_WORDS[i]}"
        if [[ -n $value ]]; then
            value=
            continue
        fi
        case "$word" in
            -d|--dut) value=dut ;;
            -p|--mcp-port) value=port ;;
            --agent) ;;
            -h|--help|-V|--version) return 0 ;;
            -*) ;;
            *)
                if [[ -z $cmd ]]; then
                    cmd=$word
                else
                    positional+=("$word")
                fi
                ;;
        esac
    done

    case "$value" in
        dut)
            # Use the CLI's config discovery and JSONC parser.
            while IFS= read -r word; do
                if [[ -n $word && $word == "$cur"* ]]; then
                    COMPREPLY+=("$word")
                fi
            done < <(unset SERMCP_TRACE; "${COMP_WORDS[0]}" names 2>/dev/null)
            return 0
            ;;
        port) return 0 ;;
    esac

    opts='-h --help -V --version'
    case "$cmd" in
        '')
            opts+=" -d --dut -p --mcp-port"
            candidates='start stop resume list status serial reboot uboot button uf flash-kernel init filter-log help'
            ;;
        start|stop) opts+=" -p --mcp-port --agent" ;;
        resume) opts+=" -p --mcp-port" ;;
        list) opts+=" --json" ;;
        init) opts+=" --non-interactive --quick --advanced --force --tui --plain" ;;
        status) opts+=" -d --dut -p --mcp-port --watch" ;;
        serial|reboot|uboot|button|uf|flash-kernel) opts+=" -d --dut -p --mcp-port" ;;
        filter-log) opts='-V --version' ;;
        help|names) ;;
        *) return 0 ;;
    esac

    if [[ $cur == -* ]]; then
        candidates=$opts
    else
        case "$cmd:${#positional[@]}" in
            uf:0|flash-kernel:0|filter-log:0)
                mapfile -t COMPREPLY < <(compgen -f -- "$cur")
                compopt -o filenames 2>/dev/null || :
                return 0
                ;;
            filter-log:1)
                # filter-log requires a new output file.
                mapfile -t COMPREPLY < <(compgen -d -- "$cur")
                compopt -o filenames 2>/dev/null || :
                return 0
                ;;
            button:0) candidates='reset download power_key' ;;
            button:1) candidates='press release pulse' ;;
        esac
    fi

    mapfile -t COMPREPLY < <(compgen -W "${candidates:-}" -- "$cur")
}
complete -F _dutabo dutabo
