#!/usr/bin/env python3
"""Adapt the shared policy to Codex's PreToolUse output contract.

Pi also uses this JSON adapter. Install the shared Claude hook directory
beside this script as `claude/` when copying it outside the repository.
"""

import contextlib
import io
import json
from pathlib import Path
import runpy
import sys


def main():
    directory = Path(__file__).resolve().parent
    shared = directory.parent / "claude" / "pre-tool-use.py"
    if not shared.is_file():
        shared = directory / "claude" / "pre-tool-use.py"
    output = io.StringIO()
    try:
        with contextlib.redirect_stdout(output):
            try:
                runpy.run_path(str(shared), run_name="__main__")
            except SystemExit as error:
                if error.code not in (None, 0):
                    raise RuntimeError(f"Shared policy exited with {error.code}") from error
        result = json.loads(output.getvalue())
        # Codex rejects even continue:true for PreToolUse; keep the deny envelope.
        result.pop("continue", None)
        print(json.dumps(result))
    except Exception as error:
        print(f"sermcp command policy unavailable: {error}", file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
