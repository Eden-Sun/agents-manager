#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

DB="$TMP/test.sqlite3"
OUT="$TMP/fixtures"

sqlite3 "$DB" <<'EOF'
CREATE TABLE api_bot_rejected_responses (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    bot_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    reason TEXT NOT NULL,
    response_model TEXT NOT NULL,
    stop_reason TEXT NOT NULL,
    usage_json TEXT NOT NULL,
    content_blocks_json TEXT NOT NULL
);

INSERT INTO api_bot_rejected_responses (bot_id, turn_id, attempt_id, created_at, reason, response_model, stop_reason, usage_json, content_blocks_json) VALUES (
    'bot_test1',
    'turn_test1',
    'att_test1',
    '2026-11-15T06:00:00Z',
    'untrusted_usage: cache_creation 細項總和不符',
    'claude-opus-5-5',
    'end_turn',
    '{"input_tokens": 10, "output_tokens": 5, "speed": "standard"}',
    '[{"type": "text"}]'
);
EOF

PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 -B "$HERE/export-api-bot-fixtures.py" --db "$DB" --out-dir "$OUT"

test -f "$OUT/rejected_bot_test1_att_test1.json"

grep -q '"name": "rejected_bot_test1_att_test1"' "$OUT/rejected_bot_test1_att_test1.json"
grep -q '"expected": "rejected"' "$OUT/rejected_bot_test1_att_test1.json"
grep -q 'cache_creation 細項總和不符' "$OUT/rejected_bot_test1_att_test1.json"

echo "export-api-bot-fixtures_test passed."
