#!/bin/bash
# OB compatibility entry: -p now requires an AG Man project ID, never a label.
# scripts/chatgpt-consult.sh -p <project-id> --request-id <stable-id> "問題"
# Full CLI: python3 scripts/ob.py --help
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
exec python3 "$here/ob.py" ask "$@"
