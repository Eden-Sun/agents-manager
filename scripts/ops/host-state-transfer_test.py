#!/usr/bin/env python3
"""隔離測試 host-state-transfer.py；只用暫存假資料，不啟 daemon、不碰正式 DB。"""

import fcntl
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent
TOOL = ROOT / "host-state-transfer.py"


class HostStateTransferTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="agm-host-state-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.home = self.root / "source-home"
        self.source = self.home / ".config" / "agents-manager"
        self.target = self.root / "target-data"
        self.bundle = self.root / "bundle"
        self.backup = self.root / "backup"
        self.home.mkdir(parents=True)
        self.source.mkdir(parents=True)
        self.target.mkdir()
        self.repo_map = ("/Users/m4p/project", str(self.root / "target-projects"))
        self.data_map = ("/Users/m4p/.config/agents-manager", str(self.target))
        self.maps = [self.repo_map, self.data_map]
        self.config = '''# full source config
[server]
data_dir = "/Users/m4p/.config/agents-manager"
[build.remote]
repo = "/Users/m4p/project/agents-manager"
[[identities]]
name = "cc1"
kind = "claude"
[identities.env]
CLAUDE_CONFIG_DIR = "$HOME/.claude-cc1"
[[projects]]
id = "P1"
host = "local"
path = "/Users/m4p/project/agents-manager"
[[projects.bots]]
id = "B1"
autostart = true
'''
        (self.source / "config.toml").write_text(self.config)
        (self.source / "ui-token").write_bytes(b"source-ui-token\n")
        outbox = self.source / "outbox"
        (outbox / "bot-a").mkdir(parents=True)
        (outbox / "bot-a" / "report.txt").write_text("deliverable")
        (outbox / "root.bin").write_bytes(b"\x00\x01payload")
        for name in (".claude", ".claude-cc1", ".codex", ".grok"):
            (self.home / name).mkdir()
        (self.home / ".claude-cc1" / ".credentials.json").write_text("never copy this")
        (self.target / "config.toml").write_text('''[server]
data_dir = "/target/old-data"
[[projects]]
id = "P1"
host = "local"
path = "/target-projects/agents-manager"
[[projects.bots]]
id = "B1"
autostart = false
''')
        (self.target / "ui-token").write_bytes(b"target-ui-token\n")
        (self.target / "agents-manager.sqlite3").write_bytes(b"fake-db-copy-do-not-touch")
        (self.target / "daemon.lock").touch()
        (self.target / "outbox").mkdir()
        (self.target / "outbox" / "existing.txt").write_text("keep me")

    def command(self, *args, ok=True):
        p = subprocess.run([sys.executable, str(TOOL), *map(str, args)], text=True,
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if ok:
            self.assertEqual(p.returncode, 0, p.stderr + p.stdout)
        else:
            self.assertNotEqual(p.returncode, 0, p.stdout)
        return p

    def snapshot(self):
        args = ["snapshot", "--source-data", self.source, "--source-home", self.home,
                "--out", self.bundle]
        for src, dst in self.maps:
            args += ["--map", f"{src}={dst}"]
        return self.command(*args)

    def install(self, *extra, ok=True):
        return self.command("install", "--bundle", self.bundle, "--target-data", self.target,
                            "--backup-dir", self.backup, *extra, ok=ok)

    def test_snapshot_carries_full_mapped_config_token_outbox_and_paths_only_inventory(self):
        result = self.snapshot()
        config = (self.bundle / "config.toml").read_text()
        self.assertIn(f'data_dir = "{self.target}"', config)
        self.assertIn(f'repo = "{self.root / "target-projects"}/agents-manager"', config)
        self.assertIn('[[identities]]', config)
        self.assertIn('autostart = true', config)
        self.assertEqual((self.bundle / "ui-token").read_bytes(), b"source-ui-token\n")
        self.assertEqual((self.bundle / "outbox" / "bot-a" / "report.txt").read_text(), "deliverable")
        self.assertEqual((self.bundle / "outbox" / "root.bin").read_bytes(), b"\x00\x01payload")
        inventory = json.loads((self.bundle / "identity-directories.json").read_text())
        paths = {row["path"] for row in inventory["directories"]}
        self.assertIn(str(self.home / ".claude-cc1"), paths)
        self.assertIn(str(self.home / ".codex"), paths)
        self.assertNotIn(".credentials.json", " ".join(paths))
        self.assertFalse(any(path.name == ".credentials.json" for path in self.bundle.rglob("*")))
        self.assertNotIn("source-ui-token", result.stdout + result.stderr)
        self.assertEqual((self.source / "config.toml").read_text(), self.config)
        self.assertEqual((self.source / "ui-token").read_bytes(), b"source-ui-token\n")
        self.assertEqual((self.bundle / "ui-token").stat().st_mode & 0o777, 0o600)

    def test_install_merges_full_non_project_config_and_preserves_imported_project_rows(self):
        self.snapshot()
        db_before = hashlib.sha256((self.target / "agents-manager.sqlite3").read_bytes()).hexdigest()
        out = self.install()
        config = (self.target / "config.toml").read_text()
        self.assertIn(f'data_dir = "{self.target}"', config)
        self.assertIn(f'repo = "{self.root / "target-projects"}/agents-manager"', config)
        self.assertIn('autostart = false', config)
        self.assertNotIn('autostart = true', config)
        self.assertEqual((self.target / "ui-token").read_bytes(), b"source-ui-token\n")
        self.assertEqual((self.target / "outbox" / "bot-a" / "report.txt").read_text(), "deliverable")
        self.assertEqual((self.target / "outbox" / "existing.txt").read_text(), "keep me")
        self.assertEqual(hashlib.sha256((self.target / "agents-manager.sqlite3").read_bytes()).hexdigest(), db_before)
        self.assertIn('"outbox_files": 2', out.stdout)
        self.command("verify", "--bundle", self.bundle, "--target-data", self.target)

    def test_install_leaves_missing_project_rows_for_project_transfer(self):
        (self.target / "config.toml").write_text('[server]\ndata_dir = "/target/data"\n')
        self.snapshot()
        self.install()
        config = (self.target / "config.toml").read_text()
        self.assertIn(f'data_dir = "{self.target}"', config)
        self.assertNotIn("[[projects]]", config)

    def test_install_refuses_a_locked_daemon_and_leaves_target_unchanged(self):
        self.snapshot()
        before = (self.target / "config.toml").read_bytes(), (self.target / "ui-token").read_bytes()
        with open(self.target / "daemon.lock", "rb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.install("--dry-run", ok=False)
            self.assertIn("daemon.lock", result.stderr)
        self.assertEqual((self.target / "config.toml").read_bytes(), before[0])
        self.assertEqual((self.target / "ui-token").read_bytes(), before[1])
        self.assertFalse(self.backup.exists())

    def test_outbox_conflict_aborts_before_changing_any_target_file(self):
        self.snapshot()
        path = self.target / "outbox" / "bot-a" / "report.txt"
        path.parent.mkdir()
        path.write_text("different target data")
        before = (self.target / "config.toml").read_bytes(), (self.target / "ui-token").read_bytes()
        result = self.install(ok=False)
        self.assertIn("outbox", result.stderr.lower())
        self.assertEqual((self.target / "config.toml").read_bytes(), before[0])
        self.assertEqual((self.target / "ui-token").read_bytes(), before[1])

    def test_restore_returns_target_config_token_and_removes_only_imported_outbox_files(self):
        self.snapshot()
        old_config = (self.target / "config.toml").read_bytes()
        old_token = (self.target / "ui-token").read_bytes()
        self.install()
        self.command("restore", "--backup-dir", self.backup, "--target-data", self.target)
        self.assertEqual((self.target / "config.toml").read_bytes(), old_config)
        self.assertEqual((self.target / "ui-token").read_bytes(), old_token)
        self.assertEqual((self.target / "outbox" / "existing.txt").read_text(), "keep me")
        self.assertFalse((self.target / "outbox" / "bot-a" / "report.txt").exists())
        self.assertFalse((self.target / "outbox" / "root.bin").exists())

    def test_restore_preserves_a_transferred_outbox_file_that_changed_after_install(self):
        self.snapshot()
        self.install()
        report = self.target / "outbox" / "bot-a" / "report.txt"
        report.write_text("new target deliverable")
        result = self.command("restore", "--backup-dir", self.backup, "--target-data", self.target)
        self.assertEqual(report.read_text(), "new target deliverable")
        self.assertIn("保留切換後已變更的 outbox 檔", result.stdout)

    def test_snapshot_rejects_symlinked_outbox_without_following_it(self):
        outbox = self.source / "outbox"
        for child in outbox.iterdir():
            if child.is_dir():
                import shutil
                shutil.rmtree(child)
            else:
                child.unlink()
        outside = self.root / "outside-secret"
        outside.write_text("not part of the snapshot")
        (outbox / "link").symlink_to(outside)
        args = ["snapshot", "--source-data", self.source, "--source-home", self.home, "--out", self.bundle]
        for src, dst in self.maps:
            args += ["--map", f"{src}={dst}"]
        p = self.command(*args, ok=False)
        self.assertIn("symlink", p.stderr.lower())
        self.assertFalse(self.bundle.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
