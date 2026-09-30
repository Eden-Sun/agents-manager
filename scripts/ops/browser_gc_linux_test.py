#!/usr/bin/env python3
"""Linux browser-gc worker tests; all process and filesystem effects use temp fixtures."""

from __future__ import annotations

import os
import shutil
import signal
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.ops.browser_gc_linux import run_once


class BrowserGcLinuxTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="browser-gc-linux-test-")
        self.root = Path(self.tmp.name)
        self.proc = self.root / "proc"
        self.proc.mkdir()
        self.tmp_root = self.root / "tmp"
        self.tmp_root.mkdir()
        self.uptime = self.root / "uptime"
        self.uptime.write_text("1000.00 0.00\n", encoding="ascii")
        self.logs: list[str] = []
        self.signals: list[tuple[int, int]] = []

    def tearDown(self):
        self.tmp.cleanup()

    def add_process(self, pid: int, ppid: int, age: int, argv: list[str]):
        proc = self.proc / str(pid)
        proc.mkdir()
        (proc / "cmdline").write_bytes(b"\0".join(a.encode() for a in argv) + b"\0")
        start_ticks = int((1000 - age) * os.sysconf("SC_CLK_TCK"))
        fields = ["S", str(ppid)] + ["0"] * 17 + [str(start_ticks)]
        (proc / "stat").write_text(f"{pid} (chromium) {' '.join(fields)}\n", encoding="ascii")
        uid = os.getuid()
        (proc / "status").write_text(f"Name:\tchromium\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n", encoding="ascii")

    def profile(self, name: str) -> Path:
        path = self.tmp_root / name
        path.mkdir()
        (path / "Local State").write_text("{}", encoding="utf-8")
        return path

    def run_gc(self, *, connection_probe=None):
        def send_signal(pid, sig):
            self.signals.append((pid, sig))
            if sig == signal.SIGTERM:
                shutil.rmtree(self.proc / str(pid), ignore_errors=True)

        return run_once(
            proc_root=self.proc,
            uptime_path=self.uptime,
            tmp_root=self.tmp_root,
            connection_probe=connection_probe or (lambda _port: False),
            send_signal=send_signal,
            pid_exists=lambda _pid: False,
            sleep_fn=lambda _seconds: None,
            log=self.logs.append,
        )

    def test_only_old_orphan_headless_chrome_without_cdp_is_reaped(self):
        orphan_profile = self.profile("am-orphan")
        self.add_process(101, 1, 300, ["/usr/bin/chromium", "--headless=new",
                                      f"--user-data-dir={orphan_profile}", "--remote-debugging-port=9222"])
        self.add_process(102, 91, 300, ["/usr/bin/chromium", "--headless=new",
                                       "--user-data-dir=/tmp/am-active", "--remote-debugging-port=9223"])
        self.add_process(103, 1, 300, ["/usr/bin/chromium", "--headless=new",
                                       "--user-data-dir=/tmp/am-cdp", "--remote-debugging-port=9224"])
        self.add_process(104, 1, 30, ["/usr/bin/chromium", "--headless=new",
                                      "--user-data-dir=/tmp/am-young", "--remote-debugging-port=9225"])
        self.add_process(105, 1, 300, ["/usr/bin/chromium", "--headless=new",
                                      "--user-data-dir=/tmp/am-unknown"])
        self.add_process(106, 1, 300, ["/usr/bin/chromium", "--type=renderer", "--headless=new"])

        result = self.run_gc(connection_probe=lambda port: True if port == 9224 else False)

        self.assertEqual(result.killed_pids, [101])
        self.assertEqual(self.signals, [(101, signal.SIGTERM)])
        self.assertFalse(orphan_profile.exists())
        self.assertIn(102, result.kept_pids)
        self.assertIn(103, result.kept_pids)
        self.assertIn(104, result.kept_pids)
        self.assertIn(105, result.kept_pids)
        self.assertIn("headless Chrome：收掉 1／保留 4", "\n".join(self.logs))

    def test_unknown_cdp_status_fails_closed_and_preserves_profile(self):
        profile = self.profile("am-unknown-cdp")
        self.add_process(201, 1, 300, ["/usr/bin/google-chrome", "--headless",
                                      f"--user-data-dir={profile}", "--remote-debugging-port=9333"])

        result = self.run_gc(connection_probe=lambda _port: None)

        self.assertEqual(result.killed_pids, [])
        self.assertEqual(self.signals, [])
        self.assertTrue(profile.exists())
        self.assertIn(201, result.kept_pids)

    def test_never_signals_a_headless_browser_owned_by_another_user(self):
        self.add_process(202, 1, 300, ["/usr/bin/chromium", "--headless",
                                      "--user-data-dir=/tmp/am-other-user", "--remote-debugging-port=9334"])
        status = self.proc / "202" / "status"
        uid = os.getuid() + 1
        status.write_text(f"Name:\tchromium\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n", encoding="ascii")

        result = self.run_gc()

        self.assertEqual(result.killed_pids, [])
        self.assertEqual(self.signals, [])

    def test_stale_profile_cleanup_is_limited_to_owned_marked_direct_children(self):
        stale = self.profile("am-stale")
        active_ui = self.profile("am-active-ui")
        old = 1000 - 7200
        os.utime(stale, (old, old))
        os.utime(active_ui, (old, old))
        self.add_process(301, 80, 300, ["/usr/bin/chromium", f"--user-data-dir={active_ui}"])
        unrelated = self.root / "unrelated"
        unrelated.mkdir()
        (unrelated / "Local State").touch()
        os.utime(unrelated, (old, old))
        symlink = self.tmp_root / "am-symlink"
        symlink.symlink_to(unrelated, target_is_directory=True)

        result = self.run_gc()

        self.assertEqual(result.removed_profiles, [stale])
        self.assertFalse(stale.exists())
        self.assertTrue(active_ui.exists())
        self.assertTrue(unrelated.exists())
        self.assertTrue(symlink.is_symlink())

    def test_runs_optional_pane_gc_once(self):
        pane_gc = Mock()

        run_once(
            proc_root=self.proc,
            uptime_path=self.uptime,
            tmp_root=self.tmp_root,
            connection_probe=lambda _port: False,
            send_signal=lambda _pid, _sig: None,
            pid_exists=lambda _pid: False,
            sleep_fn=lambda _seconds: None,
            log=self.logs.append,
            pane_gc=pane_gc,
        )

        pane_gc.assert_called_once_with()


if __name__ == "__main__":
    unittest.main(verbosity=2)
