#!/usr/bin/env python3
"""claude-hook-debug.py 的隔離測試：只用自造文字；不碰 ~/.claude/debug、不開 claude、不連 daemon。"""
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
TOOL = os.path.join(HERE, "claude-hook-debug.py")
# 官方文件 #debug-hooks 的範例行（逐字）。
OFFICIAL = "Hooks: PostToolUse:Write [.claude/hooks/log-write.sh] finished with status 0 (31ms)"


def load_tool():
    import importlib.util
    spec = importlib.util.spec_from_file_location("claude_hook_debug", TOOL)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def run(data, *args):
    p = subprocess.run([sys.executable, "-B", TOOL, *(args or ("-",))], input=data, capture_output=True)
    return p.returncode, p.stdout.decode(), p.stderr.decode()


def summary(text):
    code, out, _ = run(text.encode())
    assert code == 0
    return json.loads(out)


class ClaudeHookDebug(unittest.TestCase):
    def test_official_sample(self):
        s = summary("2026-07-19T02:03:24.382Z [DEBUG] " + OFFICIAL + "\n")
        self.assertEqual(s["groups"], [{"event": "PostToolUse", "outcome": "finished", "count": 1,
                                        "nonzero_status": 0, "with_duration": 1, "p50_ms": 31, "p95_ms": 31,
                                        "max_ms": 31}])

    def test_percentiles_nearest_rank(self):
        text = "".join(f"Hooks: Stop [x] finished with status 0 ({n}ms)\n" for n in range(1, 101))
        g = summary(text)["groups"][0]
        self.assertEqual((g["p50_ms"], g["p95_ms"], g["max_ms"], g["count"]), (50, 95, 100, 100))

    def test_outcomes_and_no_duration_is_not_zero(self):
        text = "\n".join([
            "Hooks: PreToolUse:Bash [a.sh] timed out after 5000ms",
            "Hooks: PreToolUse:Bash [a.sh] cancelled",
            "Hooks: SessionStart:startup [b.sh] [plug] failed to start",
            "Hooks: Stop [c.sh] moved to the background",
            "Hooks: Notification [d.sh] completed with status 2",
            "Hooks: Stop [c.sh] finished with status 1",
        ])
        by = {(g["event"], g["outcome"]): g for g in summary(text)["groups"]}
        self.assertEqual(set(by), {("PreToolUse", "timed_out"), ("PreToolUse", "cancelled"),
                                   ("SessionStart", "failed_to_start"), ("Stop", "backgrounded"),
                                   ("Notification", "completed"), ("Stop", "finished")})
        for g in by.values():  # 這些行都沒有實測耗時：不得出現 0 或任何百分位
            self.assertEqual(g["with_duration"], 0)
            self.assertNotIn("p50_ms", g)
        self.assertEqual(by[("Notification", "completed")]["nonzero_status"], 1)
        self.assertEqual(by[("Stop", "finished")]["nonzero_status"], 1)

    def test_unknown_lines_are_skipped(self):
        text = "\n".join([
            "random debug line", "Hooks: NotARealEvent [x] finished with status 0 (5ms)",
            "Hooks: Stop finished with status 0 (5ms)",  # 少了 [command]
            "Hooks: Stop [x] finished with status 0 (99999999999ms)",  # 超過一天
            "Hooks: Stop [x] exploded (5ms)", OFFICIAL,
        ])
        s = summary(text)
        self.assertEqual((s["lines"], s["skipped_lines"], len(s["groups"])), (6, 5, 1))

    def test_no_raw_text_in_output(self):
        secrets = ["SECRETTOKEN", "my-plugin-name", "sk-ant-abc", "prompt text here"]
        text = (f"Hooks: PostToolUse:Write(prompt text here) [./run.sh --token=SECRETTOKEN sk-ant-abc] "
                f"[my-plugin-name] finished with status 0 (12ms)\n"
                f"Hooks: Stop [./x.sh --token=SECRETTOKEN] timed out after 3000ms\n"
                f"unrelated line with SECRETTOKEN\n")
        code, out, err = run(text.encode())
        self.assertEqual(code, 0)
        for s in secrets:
            self.assertNotIn(s, out)
            self.assertNotIn(s, err)
        self.assertEqual(json.loads(out)["lines"], 3)

    def test_overlong_line_is_skipped_and_bounded(self):
        big = b"Hooks: Stop [" + b"A" * 50_000 + b"] finished with status 0 (5ms)\n"
        s = json.loads(run(big + (OFFICIAL + "\n").encode())[1])
        self.assertEqual((s["overlong_lines"], s["lines"], s["groups"][0]["event"]), (1, 1, "PostToolUse"))
        self.assertNotIn("AAAA", json.dumps(s))

    def test_byte_cap_truncates(self):
        import io
        tool = load_tool()
        tool.MAX_BYTES = 200
        out = tool.summarize(io.BytesIO((OFFICIAL + "\n").encode() * 50))
        self.assertTrue(out["truncated"])
        self.assertLess(out["lines"], 50)

    def test_sample_cap_keeps_totals_and_true_max(self):
        import io
        tool = load_tool()
        tool.MAX_SAMPLES = 3
        data = b"".join(b"Hooks: Stop [x] finished with status 0 (%dms)\n" % n for n in (5, 5, 5, 5, 5, 900, 5))
        g = tool.summarize(io.BytesIO(data))["groups"][0]
        self.assertEqual((g["count"], g["with_duration"], g["percentile_sample"]), (7, 7, 3))
        self.assertEqual((g["max_ms"], g["p95_ms"]), (900, 5))  # max 是全體；百分位只來自前 3 筆

    def test_unbounded_stream_without_newline_stops_at_byte_cap(self):
        import io

        class Endless(io.RawIOBase):  # 永遠不換行、永遠有資料；多讀的 byte 數要能量出來
            served = 0

            def readable(self):
                return True

            def readinto(self, b):
                b[:] = b"A" * len(b)
                Endless.served += len(b)
                return len(b)

        tool = load_tool()
        tool.MAX_BYTES = 100_000
        out = tool.summarize(io.BufferedReader(Endless(), buffer_size=4096))
        self.assertTrue(out["truncated"])
        self.assertEqual(out["lines"], 0)
        self.assertLess(Endless.served, 100_000 + 3 * tool.MAX_LINE)  # 不能多讀無界資料

    def test_long_line_crossing_byte_cap_stops_mid_line(self):
        import io
        tool = load_tool()
        tool.MAX_BYTES = 20_000
        out = tool.summarize(io.BytesIO(b"B" * 1_000_000 + b"\n" + OFFICIAL.encode() + b"\n"))
        self.assertTrue(out["truncated"])
        self.assertEqual((out["lines"], out["groups"]), (0, []))
        self.assertLess(out["bytes_read"], 20_000 + 2 * tool.MAX_LINE)

    def test_absurd_digit_runs_are_skipped_not_crashing(self):
        d = "9" * 5000
        text = "\n".join([f"Hooks: Stop [x] finished with status {d} (5ms)",
                          f"Hooks: Stop [x] finished with status 0 ({d}ms)",
                          f"Hooks: Stop [x] timed out after {d}ms",
                          f"Hooks: Notification [x] completed with status {d}",
                          "Hooks: Stop [x] finished with status 1234567890 (5ms)", OFFICIAL])
        code, out, err = run(text.encode())
        self.assertEqual(code, 0)
        self.assertNotIn("Traceback", err)
        s = json.loads(out)
        self.assertEqual((s["lines"], s["skipped_lines"], len(s["groups"])), (6, 5, 1))

    def test_file_input_and_errors(self):
        with tempfile.NamedTemporaryFile("w", suffix=".txt") as t:
            t.write(OFFICIAL + "\n")
            t.flush()
            self.assertEqual(json.loads(run(b"", t.name)[1])["groups"][0]["max_ms"], 31)
        code, out, err = run(b"", "/nonexistent/claude-hook-debug-input")
        self.assertEqual((code, out), (2, ""))
        self.assertNotIn("Traceback", err)
        self.assertEqual(subprocess.run([sys.executable, "-B", TOOL], capture_output=True).returncode, 2)

    def test_output_states_its_limit(self):
        note = summary(OFFICIAL + "\n")["note"]
        self.assertIn("MCP", note)
        self.assertIn("不能區分", note)


if __name__ == "__main__":
    unittest.main(verbosity=1)
