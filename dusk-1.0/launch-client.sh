#!/usr/bin/env bash
# Dusk v1.0 — client launcher
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export GDK_BACKEND=x11
export DISPLAY="${DISPLAY:-:0}"
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
cd "$SCRIPT_DIR"

if [[ $# -gt 0 ]]; then
    HOST_IP="$1"
else
    HOST_IP="$(python3 -c "
import tomllib, sys
with open('config.toml','rb') as f: c = tomllib.load(f)
ip = c.get('host_ip','')
if not ip or ip == '100.x.x.x':
    print('ERROR: host_ip not set in config.toml', file=sys.stderr)
    sys.exit(1)
print(ip)
")" || { echo "Aborting."; exit 1; }
fi

exec python main.py --connect "$HOST_IP"
