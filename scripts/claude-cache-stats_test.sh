#!/usr/bin/env bash
# scripts/claude-cache-stats.py 的隔離測試：只用自造 jsonl（--root 指到暫存目錄），不讀 ~/.claude*。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: requires python3" >&2
    exit 1
fi
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
# canary-gap: PATH 原樣繼承；被測腳本與這段造資料只用 python 標準庫。
python3 -B - "$TMP" <<'PY'
import datetime, json, os, sys
root = sys.argv[1]
now = datetime.datetime.now(datetime.timezone.utc)
def rec(mid, rid, minutes_ago, i, w, r):
    ts = (now - datetime.timedelta(minutes=minutes_ago)).isoformat().replace("+00:00", "Z")
    return json.dumps({"timestamp": ts, "requestId": rid,
                       "message": {"id": mid, "usage": {"input_tokens": i, "cache_creation_input_tokens": w, "cache_read_input_tokens": r}}})
for proj, rows in {
    "-x-supervisor-AGM": [rec("a", "1", 300, 1, 1000, 0), rec("a", "1", 300, 1, 1000, 0),  # 重複行只算一次
                          rec("b", "2", 290, 1, 0, 1000), rec("c", "3", 100, 1, 5000, 1000)],  # 190 分間隔＝冷喚醒
    "-x-supervisor-AGM-responder": [rec("d", "4", 10, 1, 0, 2000)],
}.items():
    os.makedirs(f"{root}/{proj}")
    open(f"{root}/{proj}/aaaaaaaa-0000.jsonl", "w").write("\n".join(rows) + "\n")
PY
# canary-gap: PATH 原樣繼承；被測腳本只用 python 標準庫、只讀 --root 指到的暫存目錄，不 spawn 其他行程。
S="python3 -B $HERE/claude-cache-stats.py"
out="$($S summary --root "$TMP" --project 'supervisor-AGM$' 2>/dev/null)"
grep -q "supervisor-AGM " <<<"$out" || { echo "FAIL: summary 沒列出 supervisor-AGM"; echo "$out"; exit 1; }
grep -q "responder" <<<"$out" && { echo "FAIL: \$ 結尾比對把 responder 也算進去"; exit 1; }
# 3 筆去重後請求：write 6000、read 2000 → hit = 2000/(3+6000+2000)=25.0%
grep -q "TOTAL .* 3 " <<<"$out" && grep -q "25.0" <<<"$out" || { echo "FAIL: 去重或命中率不對"; echo "$out"; exit 1; }
out="$($S sessions --root "$TMP" --project 'supervisor-AGM$' --min-turns 1 2>/dev/null)"
grep -Eq "s-[0-9a-f]+ +3 +.* 1 +0\.0" <<<"$out" || { echo "FAIL: sessions 輪數／冷喚醒次數不對"; echo "$out"; exit 1; }
echo "ok: claude-cache-stats"
# canary-gap: isolated stdlib fixtures; no real logs, credentials, services or model calls.
python3 -B - "$HERE/claude-cache-stats.py" <<'PY'
import contextlib
import datetime
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("cache_stats", sys.argv[1])
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
sys.argv = [sys.argv[0]]
STAMP = "2026-10-21T12:00:00Z"
SECRET = "SENTINEL_PRIVATE_PROMPT_TOOL_PATH"


def request(mid="m", rid="r", usage=None, stamp=STAMP):
    return {"type": "assistant", "timestamp": stamp, "requestId": rid,
            "message": {"id": mid, "role": "assistant",
                        "usage": usage if usage is not None else {
                            "input_tokens": 10, "cache_creation_input_tokens": 20,
                            "cache_read_input_tokens": 30},
                        "content": [{"type": "tool_use", "input": {"command": SECRET}}]}}


class StatsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix=SECRET)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.project = self.root / ("-project-" + SECRET)
        self.project.mkdir()
        self.path = self.project / (SECRET + ".jsonl")

    def write(self, records):
        self.path.write_text("".join(json.dumps(x) + "\n" for x in records))

    def call(self, *flags, cmd="summary"):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = mod.main([cmd, "--root", str(self.root), "--since", "2026-10-20T00:00:00Z",
                           "--until", "2026-10-22T00:00:00Z", "--min-turns", "1", *flags])
        text = out.getvalue() + err.getvalue()
        self.assertNotIn(SECRET, text)
        self.assertNotIn(str(self.root), text)
        return rc, out.getvalue(), err.getvalue()

    def test_partial_merge_and_complete_denominator(self):
        self.write([request(usage={"input_tokens": 10}),
                    request(usage={"cache_creation_input_tokens": 20, "cache_read_input_tokens": 30}),
                    request(usage={"input_tokens": 5, "cache_read_input_tokens": 25}),
                    request("unknown", "r2", {"input_tokens": 7})])
        rc, out, err = self.call()
        self.assertEqual(rc, 0)
        self.assertIn("duplicate=2", err)
        self.assertIn("unknown_requests=1", err)
        total = next(line for line in out.splitlines() if line.startswith("TOTAL")).split()
        self.assertEqual(total[1], "2")
        self.assertEqual(total[4], "50.0")  # 30/(10+20+30), not partial zeros
        self.assertEqual(total[6], "1")  # measured denominator
        self.assertEqual(total[7:9], ["0.1", "0.1"])

    def test_unknown_not_zero_and_invalid_usage(self):
        self.write([request(usage={"input_tokens": True, "cache_creation_input_tokens": -1,
                                   "cache_read_input_tokens": "30"})])
        rc, out, err = self.call(cmd="sessions")
        self.assertEqual(rc, 0)
        self.assertIn("NA", out)
        self.assertIn("unknown_requests=1", err)
        self.assertIn("unknown_fields=3", err)

    def test_malformed_and_bad_timestamps(self):
        self.write([request(), request("bad", stamp="2026-10-21T12:00:00")])
        with self.path.open("ab") as f:
            f.write(b'{bad json\n[]\n\xff\n')
        rc, out, err = self.call()
        self.assertEqual(rc, 0)
        self.assertIn("malformed=3", err)
        self.assertIn("invalid_timestamp=1", err)

    def test_timezone_and_half_open_window(self):
        self.write([request("yes", stamp="2026-10-19T20:00:00-04:00"),
                    request("end", stamp="2026-10-21T20:00:00-04:00"),
                    request("early", stamp="2026-10-19T23:59:59Z")])
        _, _, err = self.call()
        self.assertIn("requests=1", err)
        self.assertEqual(mod.parse_ts("2026-10-20T08:00:00+08:00"),
                         mod.parse_ts("2026-10-20T00:00:00Z"))

    def test_user_turns_exclude_tool_meta_and_duplicates(self):
        def user(uid, content, meta=False):
            return {"type": "user", "timestamp": STAMP, "uuid": uid, "isMeta": meta,
                    "message": {"role": "user", "content": content}}
        text = [{"type": "text", "text": SECRET}]
        first = user("u1", text)
        self.write([request(), first, first, user("u2", SECRET),
                    user("tool", [{"type": "tool_result", "content": SECRET}, *text]),
                    user("meta", text, True), user(None, text)])
        rc, out, err = self.call(cmd="sessions")
        self.assertEqual(rc, 0)
        self.assertIn("user_turns=2", err)
        self.assertIn("user_duplicate=1", err)
        self.assertIn("user_missing_uuid=1", err)
        self.assertIn("session_user_turns measured=1 p50=2 p95=2", out)

    def test_inode_and_exported_record_deduplication(self):
        self.write([request()])
        os.link(self.path, self.project / "hardlink.jsonl")
        (self.project / "copy.jsonl").write_bytes(self.path.read_bytes())
        _, _, err = self.call()
        self.assertIn("files=2", err)
        self.assertIn("requests=1", err)
        self.assertIn("duplicate=1", err)

    def test_missing_ids(self):
        self.write([request(None), request("fallback", None), request("fallback", None)])
        _, _, err = self.call()
        self.assertIn("requests=1", err)
        self.assertIn("missing_message_id=1", err)
        self.assertIn("missing_request_id=2", err)

    def test_bounds_fail_without_partial_reports(self):
        self.write([request(), request("m2", "r2")])
        for flags in (("--max-files", "1"), ("--max-bytes", "1"),
                      ("--max-line-bytes", "10"), ("--max-records", "1")):
            with self.subTest(flags=flags):
                if flags[0] == "--max-files":
                    (self.project / "second.jsonl").write_bytes(b"")
                rc, out, err = self.call(*flags)
                self.assertEqual(rc, 2)
                self.assertEqual(out, "")
                self.assertIn("incomplete=1", err)

    def test_snapshot_ignores_appended_bytes(self):
        self.write([request()])
        original = mod.os.fstat
        def append_after_stat(fd):
            result = original(fd)
            with self.path.open("a") as f:
                f.write(json.dumps(request("new", "r2")) + "\n")
            return result
        with mock.patch.object(mod.os, "fstat", side_effect=append_after_stat):
            _, _, err = self.call()
        self.assertIn("requests=1", err)

    def test_no_files_or_read_error_not_zero_report(self):
        rc, out, err = self.call()
        self.assertEqual((rc, out), (2, ""))
        self.write([request()])
        with mock.patch.object(mod.os, "open", side_effect=PermissionError):
            rc, out, err = self.call()
        self.assertEqual((rc, out), (2, ""))
        self.assertIn("file read failed", err)

    def test_request_session_context_distributions(self):
        self.write([request(str(i), str(i), {"input_tokens": i, "cache_creation_input_tokens": 0,
                                           "cache_read_input_tokens": 0}) for i in range(1, 21)])
        _, out, _ = self.call(cmd="sessions")
        self.assertIn("session_turns measured=1 p50=20 p95=20", out)
        self.assertEqual(mod.percentile(list(range(1, 21)), .5), 10)
        self.assertEqual(mod.percentile(list(range(1, 21)), .95), 19)
        self.assertIn("理論截斷量、不是實測節省", out)
        self.assertNotIn("最多省", out)

    def test_argument_validation(self):
        for flags in (("--days", "nan"), ("--days", "inf"), ("--days", "-1"),
                      ("--top", "0"), ("--max-records", "0"),
                      ("--since", "invalid"), ("--until", "2026-10-19T00:00:00Z")):
            with self.subTest(flags=flags), self.assertRaises(SystemExit) as caught:
                self.call(*flags)
            self.assertEqual(caught.exception.code, 2)

    def test_subagent_signature_and_filter(self):
        sub = self.project / "parent" / "subagents"
        sub.mkdir(parents=True)
        (sub / (SECRET + ".jsonl")).write_text(json.dumps(request()) + "\n")
        rc, out, _ = self.call("--project", SECRET + "$", cmd="sessions")
        self.assertEqual(rc, 0)
        self.assertIn("*", out)
        self.assertIn("s-", out)

    def test_user_bound(self):
        self.write([{"type": "user", "timestamp": STAMP, "uuid": str(i),
                     "message": {"role": "user", "content": SECRET}} for i in range(2)])
        rc, out, err = self.call("--max-records", "1")
        self.assertEqual((rc, out), (2, ""))
        self.assertIn("user turns exceeded", err)


unittest.main()
PY
