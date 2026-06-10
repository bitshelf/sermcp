#!/usr/bin/env bash
set -euo pipefail

search_dir=${CODEX_WORKSPACE_ROOT:-${PWD}}
while [[ ${search_dir} != "/" ]]; do
    if [[ -f "${search_dir}/.target.jsonc" ]]; then
        target_config="${search_dir}/.target.jsonc"
        break
    fi
    search_dir=$(dirname "${search_dir}")
done

if [[ -z ${target_config:-} ]]; then
    echo "sermcp is inactive: no .target.jsonc found from ${PWD} upward" >&2
    exit 0
fi

export TARGET_CONF=${target_config}

if [[ -n ${SERMCP_BIN:-} ]]; then
    if [[ ! -x ${SERMCP_BIN} ]]; then
        echo "SERMCP_BIN is not executable: ${SERMCP_BIN}" >&2
        exit 126
    fi
    exec "${SERMCP_BIN}"
fi

if executable=$(command -v sermcp 2>/dev/null); then
    exec "${executable}"
fi

for executable in \
    "${CARGO_HOME:-${HOME}/.cargo}/bin/sermcp" \
    "${HOME}/.local/bin/sermcp"; do
    if [[ -x ${executable} ]]; then
        exec "${executable}"
    fi
done

echo "sermcp is required for projects with .target.jsonc." >&2
echo "Install on x86_64 Linux: rustup target add x86_64-unknown-linux-musl && cargo install --git https://github.com/bitshelf/sermcp --locked --target x86_64-unknown-linux-musl" >&2
exit 127
