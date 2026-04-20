#!/usr/bin/env bash
# Dusk v1.0 — host launcher
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export GDK_BACKEND=x11
export DISPLAY="${DISPLAY:-:0}"
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
cd "$SCRIPT_DIR"
exec python main.py --host "$@"
