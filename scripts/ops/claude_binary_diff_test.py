#!/usr/bin/env python3
"""claude_binary_diff.py tests; fake binaries are shell scripts in a temp dir."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.ops import claude_binary_diff as cbd

SCRIPT = Path(cbd.__file__)
DESC = 'x.describe("a field description long enough to match")'
ENV_A = "CLAUDE_CODE_OLD_FLAG"
ENV_B = "CLAUDE_CODE_NEW_FLAG"


def fake(root: Path, name: str, help_text: str, subs: dict[str, str] | None = None, trailer: str = "") -> str:
    """假 binary：shell script，檔尾 `#` 註解裡夾 describe／環境變數字串供掃描。"""
    cases = "".join(f'  {k}) cat <<\'EOT\'\n{v}\nEOT\n;;\n' for k, v in (subs or {}).items())
    body = (f"#!/bin/sh\ncase \"$1\" in\n{cases}  *) cat <<'EOT'\n{help_text}\nEOT\n;;\nesac\n"
            f"exit 0\n# {trailer}\n")
    path = root / name
    path.write_text(body, encoding="utf-8")
    path.chmod(0o755)
    return str(path)


HELP_OLD = "Usage: claude [options] [command]\n\nOptions:\n  --old  gone\n\nCommands:\n  mcp  Manage MCP\n  help  x\n"
HELP_NEW = ("Usage: claude [options] [command]\n\nOptions:\n  --new  fresh\n\nCommands:\n  mcp  Manage MCP\n"
            "  plugin|plugins  Plugins\n  help  x\n")


class BinaryDiffTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="claude-binary-diff-test-")
        self.root = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def report(self, old: str, new: str, **kw) -> str:
        return cbd.build_report(old, new, kw.get("timeout", 30), kw.get("max_lines", 600), kw.get("max_bytes", 60000))

    def test_help_fields_and_env_diff(self):
        old = fake(self.root, "old", HELP_OLD, {"mcp": "mcp old"}, f"{ENV_A} {DESC.replace('field', 'older')}")
        new = fake(self.root, "new", HELP_NEW, {"mcp": "mcp new", "plugin": "plugin help"}, f"{ENV_B} {DESC}")
        text = self.report(old, new)
        self.assertIn("-  --old  gone", text)
        self.assertIn("+  --new  fresh", text)
        self.assertIn("NEW SUBCOMMANDS: plugin", text)
        self.assertIn("-- NEW `plugin --help` --\nplugin help", text)
        self.assertIn("-- CHANGED `mcp --help` --", text)
        new_env = text.split("== NEW ENV ==\n")[1].split("\n")[0]
        self.assertEqual(new_env, ENV_B)
        self.assertEqual(text.split("== REMOVED ENV ==\n")[1].split("\n")[0], ENV_A)
        self.assertIn(" - a field description long enough to match", text.split("== NEW FIELDS")[1].split("== REMOVED")[0])
        self.assertIn("older description", text.split("== REMOVED FIELDS")[1])

    def test_identical_binaries_report_no_diff(self):
        a = fake(self.root, "a", HELP_OLD, trailer=f"{ENV_A} {DESC}")
        b = fake(self.root, "b", HELP_OLD, trailer=f"{ENV_A} {DESC}")
        text = self.report(a, b)
        self.assertIn("(頂層 --help 無差異)", text)
        self.assertEqual(text.count("(無)"), 2 + 4)  # 兩行子命令＋四個區塊

    def test_output_is_deterministic(self):
        a = fake(self.root, "a", HELP_OLD, trailer=f"{ENV_A} {ENV_B}")
        b = fake(self.root, "b", HELP_NEW, {"plugin": "p"}, trailer=f"{DESC} CLAUDE_CODE_ZED_FLAG CLAUDE_CODE_ALPHA_FLAG")
        self.assertEqual(self.report(a, b), self.report(a, b))

    def test_match_spanning_chunk_boundary_is_found(self):
        old = fake(self.root, "old", HELP_OLD)
        new = fake(self.root, "new", HELP_OLD)
        target = Path(new)
        data = target.read_bytes()
        marker = DESC.encode() + b" " + ENV_B.encode()
        for chunk in (64, 100, 128, 1024):
            for pad in range(0, len(marker)):
                target.write_bytes(data + b"\0" * pad + marker)
                saved, cbd.CHUNK = cbd.CHUNK, chunk
                try:
                    fields, envs = cbd.scan(str(target), cbd.Deadline(30))
                finally:
                    cbd.CHUNK = saved
                self.assertIn("a field description long enough to match", fields, (chunk, pad))
                self.assertIn(ENV_B, envs, (chunk, pad))

    def test_help_timeout_is_reported_not_raised(self):
        slow = self.root / "slow"
        slow.write_text("#!/bin/sh\nsleep 30\n")
        slow.chmod(0o755)
        text = cbd.build_report(str(slow), str(slow), 1.0, 600, 60000)
        self.assertIn("ERROR: OLD --help：", text)
        self.assertIn("== NEW ENV ==", text)  # --help 卡住，字串掃描照樣產出

    def test_output_cap_truncates(self):
        old = fake(self.root, "old", HELP_OLD)
        many = " ".join(f'y.describe("field description number {i:04d} padding padding")' for i in range(200))
        new = fake(self.root, "new", HELP_OLD, trailer=many)
        text = self.report(old, new, max_lines=20)
        self.assertEqual(len(text.splitlines()), 21)
        self.assertIn("[截斷：", text)

    def test_help_environment_is_isolated(self):
        env_dump = self.root / "envdump"
        env_dump.write_text('#!/bin/sh\nenv | sort\n')
        env_dump.chmod(0o755)
        home = str(self.root)
        text, err = cbd.run_help(str(env_dump), [], cbd.Deadline(10), home)
        self.assertEqual(err, "")
        self.assertIn(f"HOME={home}", text)
        self.assertNotIn("AM_BOT_TOKEN", text)
        self.assertNotIn("ANTHROPIC", text)

    def test_cli_rejects_missing_binary(self):
        proc = subprocess.run([sys.executable, "-I", str(SCRIPT), "/nonexistent/a", "/nonexistent/b"],
                              capture_output=True, text=True)
        self.assertEqual(proc.returncode, 2)
        self.assertIn("OLD 不是可執行檔", proc.stderr)

    def test_cli_prints_report(self):
        a = fake(self.root, "a", HELP_OLD)
        b = fake(self.root, "b", HELP_NEW)
        proc = subprocess.run([sys.executable, "-I", str(SCRIPT), a, b], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertTrue(proc.stdout.startswith("Claude binary diff"))

    def test_subcommands_parser(self):
        self.assertEqual(cbd.subcommands(HELP_NEW), ["mcp", "plugin"])


if __name__ == "__main__":
    unittest.main()
