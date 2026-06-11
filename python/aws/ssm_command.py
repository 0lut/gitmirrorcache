"""Build an SSM AWS-RunShellScript parameters document from a script file.

Usage: ssm_command.py SCRIPT_FILE [VAR=VALUE ...]

Emits {"commands": [script]} on stdout, where the script is
`set -euo pipefail`, the given variables assigned shell-quoted, then the
contents of SCRIPT_FILE.
"""

import json
import shlex
import sys

script_path = sys.argv[1]
lines = ["set -euo pipefail"]
for arg in sys.argv[2:]:
    name, sep, value = arg.partition("=")
    if not sep or not name.isidentifier():
        raise SystemExit(f"invalid VAR=VALUE argument: {arg!r}")
    lines.append(f"{name}={shlex.quote(value)}")
body = open(script_path).read()
json.dump({"commands": ["\n".join(lines) + "\n\n" + body]}, sys.stdout)
