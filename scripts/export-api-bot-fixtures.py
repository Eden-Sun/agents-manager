#!/usr/bin/env python3
"""匯出 api_bot_rejected_responses 記錄為測試 fixture。

用法：
  python3 scripts/export-api-bot-fixtures.py [--db PATH] [--bot BOT_ID] [--out-dir DIR] [--limit N]
"""
import argparse
import json
import os
import sqlite3
import sys
from pathlib import Path


def default_db_path() -> Path:
    config_dir = os.environ.get("AM_DATA_DIR")
    if config_dir:
        return Path(config_dir) / "agents-manager.sqlite3"
    home = Path.home()
    return home / ".config" / "agents-manager" / "agents-manager.sqlite3"


def main() -> int:
    parser = argparse.ArgumentParser(description="Export rejected api-bot responses to fixtures.")
    parser.add_argument("--db", type=Path, default=default_db_path(), help="Path to sqlite DB")
    parser.add_argument("--bot", type=str, default=None, help="Filter by bot ID")
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=Path("daemon/tests/fixtures/api_bot"),
        help="Output directory for JSON fixtures",
    )
    parser.add_argument("--limit", type=int, default=100, help="Max records to export")
    args = parser.parse_args()

    if not args.db.is_file():
        print(f"Error: DB file not found: {args.db}", file=sys.stderr)
        return 1

    args.out_dir.mkdir(parents=True, exist_ok=True)

    conn = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    cursor = conn.cursor()

    query = """
        SELECT bot_id, turn_id, attempt_id, response_model, stop_reason, reason, usage_json, content_blocks_json, created_at
        FROM api_bot_rejected_responses
    """
    params = []
    if args.bot:
        query += " WHERE bot_id = ?"
        params.append(args.bot)
    query += " ORDER BY created_at DESC LIMIT ?"
    params.append(args.limit)

    try:
        rows = cursor.execute(query, params).fetchall()
    except sqlite3.OperationalError as e:
        print(f"Database query error (table missing?): {e}", file=sys.stderr)
        return 1
    finally:
        conn.close()

    exported = 0
    for row in rows:
        bot_id = row["bot_id"]
        attempt_id = row["attempt_id"]
        model = row["response_model"]
        try:
            usage = json.loads(row["usage_json"])
            content = json.loads(row["content_blocks_json"])
        except Exception as e:
            print(f"Warning: Failed to parse JSON for {bot_id}/{attempt_id}: {e}", file=sys.stderr)
            continue

        fixture = {
            "name": f"rejected_{bot_id}_{attempt_id}",
            "bot_id": bot_id,
            "turn_id": row["turn_id"],
            "attempt_id": attempt_id,
            "created_at": row["created_at"],
            "model": model,
            "expected": "rejected",
            "expected_reason_contains": row["reason"],
            "request_options": {
                "model": model,
                "effort": "high",
                "fast": False,
                "max_tokens": 8192,
                "compact_at_input_tokens": 0,
                "cache_ttl_1h": False,
            },
            "response": {
                "id": f"msg_{attempt_id}",
                "model": model,
                "stop_reason": row["stop_reason"],
                "content": content,
                "usage": usage,
            },
        }

        filename = f"rejected_{bot_id}_{attempt_id}.json"
        target_path = args.out_dir / filename
        with open(target_path, "w", encoding="utf-8") as f:
            json.dump(fixture, f, ensure_ascii=False, indent=2)
            f.write("\n")
        exported += 1

    print(f"Exported {exported} rejected response(s) to {args.out_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
