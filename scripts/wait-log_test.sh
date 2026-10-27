#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# canary-gap: only Python stdlib synthetic fixtures; no external job or service.
exec python3 -B "${HERE}/wait-log_test.py"
