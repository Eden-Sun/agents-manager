"""daemon-start.py 的 daemon.log 輪替。"""

import fcntl
import gzip
import importlib.util
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("daemon_start", os.path.join(HERE, "daemon-start.py"))
daemon_start = importlib.util.module_from_spec(spec)
spec.loader.exec_module(daemon_start)


def gz(path: str) -> bytes:
    with gzip.open(path, "rb") as f:
        return f.read()


class RotateLogTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = self._tmp.name
        self.log = os.path.join(self.dir, "daemon.log")
        open(os.path.join(self.dir, "daemon.lock"), "a").close()

    def tearDown(self):
        self._tmp.cleanup()

    def write(self, path: str, data: bytes):
        with open(path, "wb") as f:
            f.write(data)

    def test_small_log_is_left_alone(self):
        self.write(self.log, b"x" * 10)
        self.assertFalse(daemon_start.rotate_log(self.log, limit=100, keep=3))
        self.assertEqual(open(self.log, "rb").read(), b"x" * 10)
        self.assertFalse(os.path.exists(self.log + ".1.gz"))

    def test_big_log_is_moved_and_compressed_without_losing_bytes(self):
        body = b"line of the daemon\n" * 50
        self.write(self.log, body)
        self.assertTrue(daemon_start.rotate_log(self.log, limit=100, keep=3))
        self.assertFalse(os.path.exists(self.log), "the live name is free for the next daemon to create")
        self.assertEqual(gz(self.log + ".1.gz"), body)
        self.assertEqual(os.listdir(self.dir).count("daemon.log.rotating"), 0)

    def test_only_keep_generations_survive_and_they_shift(self):
        for i, tag in ((1, b"gen1"), (2, b"gen2"), (3, b"gen3")):
            with gzip.open(f"{self.log}.{i}.gz", "wb") as f:
                f.write(tag)
        self.write(self.log, b"x" * 500)
        self.assertTrue(daemon_start.rotate_log(self.log, limit=100, keep=3))
        self.assertEqual(gz(self.log + ".1.gz"), b"x" * 500)
        self.assertEqual(gz(self.log + ".2.gz"), b"gen1")
        self.assertEqual(gz(self.log + ".3.gz"), b"gen2")
        self.assertFalse(os.path.exists(self.log + ".4.gz"))

    def test_a_live_daemon_holding_the_lock_blocks_rotation(self):
        self.write(self.log, b"x" * 500)
        with open(os.path.join(self.dir, "daemon.lock"), "a") as holder:
            fcntl.flock(holder.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertFalse(daemon_start.rotate_log(self.log, limit=100, keep=3))
        self.assertEqual(open(self.log, "rb").read(), b"x" * 500, "the live log is untouched")

    def test_a_rotation_interrupted_midway_is_finished_next_time(self):
        self.write(self.log + ".rotating", b"half done\n")
        self.assertTrue(daemon_start.rotate_log(self.log, limit=100, keep=3))
        self.assertEqual(gz(self.log + ".1.gz"), b"half done\n")
        self.assertFalse(os.path.exists(self.log + ".rotating"))


if __name__ == "__main__":
    unittest.main()
