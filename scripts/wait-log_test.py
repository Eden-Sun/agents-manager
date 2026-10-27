#!/usr/bin/env python3
"""Synthetic files and fake time only; no services or producing jobs."""
import contextlib
import importlib.util
import io
import os
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('wait_log', Path(__file__).with_name('wait-log.py'))
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)


class WaitLogTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / 'log'

    def run_wait(self, flags, initial=None, changes=()):
        if initial is not None:
            self.path.write_bytes(initial)
        args = mod.parser().parse_args([str(self.path), '--timeout', '1', '--interval', '.1'] + flags)
        now = [0.0]
        pending = iter(changes)
        def sleep(seconds):
            now[0] += seconds
            action = next(pending, None)
            if action:
                action()
        out, err = io.StringIO(), io.StringIO()
        rc = mod.wait(args, clock=lambda: now[0], sleep=sleep, out=out, err=err)
        return rc, out.getvalue(), err.getvalue()

    def append(self, data):
        with self.path.open('ab') as stream:
            stream.write(data)

    def test_literal_partial_and_no_regex(self):
        self.assertEqual(self.run_wait(['--contains', 'a.*b'], b'aZZb')[0], 124)
        self.assertEqual(self.run_wait(['--contains', 'a.*b'], b'a.*b')[0], 0)

    def test_multiple_markers_or(self):
        self.assertEqual(self.run_wait(['--contains', 'one', '--contains', 'two'], b'two')[0], 0)

    def test_prefix_requires_line_start_and_newline(self):
        for data in (b'xDONE\n', b'DONE'):
            self.assertEqual(self.run_wait(['--line-prefix', 'DONE'], data)[0], 124)
        self.assertEqual(self.run_wait(['--line-prefix', 'DONE'], b'DONE yes\n')[0], 0)

    def test_exit_statuses(self):
        for code in (0, 1, 42, 124, 255):
            with self.subTest(code=code):
                self.assertEqual(self.run_wait(['--exit-prefix', 'RC='], f'RC={code}\n'.encode())[0], code)

    def test_invalid_exit_status(self):
        for value in ('', '-1', '256', '0 extra', '1.0'):
            self.assertEqual(self.run_wait(['--exit-prefix', 'RC='], f'RC={value}\n'.encode())[0], 2)

    def test_partial_exit_not_premature(self):
        rc, _, _ = self.run_wait(['--exit-prefix', 'RC='], b'RC=1', [lambda: self.append(b'2\n')])
        self.assertEqual(rc, 12)

    def test_missing_then_created(self):
        self.assertEqual(self.run_wait(['--exit-prefix', 'RC='], changes=[lambda: self.path.write_bytes(b'RC=0\n')])[0], 0)

    def test_missing_deadline(self):
        rc, _, err = self.run_wait(['--contains', 'done'])
        self.assertEqual(rc, 124)
        self.assertIn('deadline', err)

    def test_append(self):
        self.assertEqual(self.run_wait(['--contains', 'finished'], b'fin', [lambda: self.append(b'ished')])[0], 0)

    def test_rotation_clears_partial_and_tail(self):
        def rotate():
            self.path.rename(self.path.with_suffix('.old'))
            self.path.write_bytes(b'DONE\n')
        rc, out, _ = self.run_wait(['--line-prefix', 'DONE'], b'old\nunfinished', [rotate])
        self.assertEqual(rc, 0)
        self.assertEqual(out, 'DONE\n')

    def test_truncation_clears_partial(self):
        rc, out, _ = self.run_wait(['--line-prefix', 'OK'], b'old lengthy partial', [lambda: self.path.write_bytes(b'OK\n')])
        self.assertEqual((rc, out), (0, 'OK\n'))

    def test_bounded_tail_and_output(self):
        rc, out, _ = self.run_wait(['--line-prefix', 'DONE', '--tail', '2', '--max-output', '8'], b'first\nsecond\nDONE\n')
        self.assertEqual(rc, 0)
        self.assertLessEqual(len(out), 9)
        self.assertNotIn('first', out)
        self.assertTrue(out.endswith('DONE\n'))

    def test_tail_zero(self):
        self.assertEqual(self.run_wait(['--line-prefix', 'DONE', '--tail', '0'], b'DONE\n')[1], '')

    def test_large_line_fails(self):
        self.assertEqual(self.run_wait(['--line-prefix', 'DONE'], b'x' * (mod.MAX_LINE + 1))[0], 2)

    def test_chunk_boundary(self):
        self.assertEqual(self.run_wait(['--contains', 'abcdef'], b'x' * (mod.CHUNK - 3) + b'abcdef')[0], 0)

    def test_directory_fifo_symlink_rejected(self):
        for kind in ('directory', 'fifo', 'symlink'):
            with self.subTest(kind=kind):
                if kind == 'directory':
                    self.path.mkdir()
                elif kind == 'fifo':
                    os.mkfifo(self.path)
                else:
                    self.path.symlink_to(Path(self.tmp.name))
                self.assertEqual(self.run_wait(['--contains', 'done'])[0], 2)
                if kind == 'directory':
                    self.path.rmdir()
                else:
                    self.path.unlink()

    def test_invalid_arguments(self):
        for flags in (['--contains', ''], ['--contains', 'x', '--tail', '-1'], ['--contains', 'x', '--timeout', 'nan'], ['--contains', 'x', '--interval', 'inf']):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
                mod.main([str(self.path)] + flags)
            self.assertEqual(caught.exception.code, 2)

    def test_log_unchanged(self):
        self.path.write_bytes(b'RC=0\n')
        before = self.path.stat()
        self.run_wait(['--exit-prefix', 'RC='])
        self.assertEqual(self.path.read_bytes(), b'RC=0\n')
        self.assertEqual(self.path.stat().st_mtime_ns, before.st_mtime_ns)

    def test_cli_propagates_job_failure(self):
        self.path.write_bytes(b'failed\nBUILD_RC=37\n')
        result = subprocess.run([sys.executable, '-B', str(Path(mod.__file__)),
                                 str(self.path), '--exit-prefix', 'BUILD_RC='],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 37)
        self.assertIn('failed', result.stdout)

    def test_cli_deadline_not_success(self):
        result = subprocess.run([sys.executable, '-B', str(Path(mod.__file__)),
                                 str(self.path), '--contains', 'done', '--timeout', '.02'],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 124)


if __name__ == '__main__':
    unittest.main()
