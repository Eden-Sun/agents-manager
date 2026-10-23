#!/bin/bash
# Isolated bundle publication tests; no real Bun, daemon, or deployment.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 -B - "$HERE/build-web-bundle.py" <<'PY'
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

HELPER = Path(sys.argv.pop())
spec = importlib.util.spec_from_file_location('bundle', HELPER)
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)

class BundleTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='bundle-test-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.web = self.root / 'web with spaces'
        self.web.mkdir()
        self.source = self.web / 'source'
        self.source.mkdir()
        self.dist = self.web / 'dist'
        self.bun = self.root / 'fake-bun'
        self.bun.write_text('''#!/usr/bin/env python3
import os, shutil, sys
from pathlib import Path
if os.environ.get('BUNDLE_FAIL') == sys.argv[1]:
    if sys.argv[1] == 'run':
        Path(sys.argv[4]).mkdir()
        (Path(sys.argv[4]) / 'partial.js').write_text('partial')
    sys.exit(7)
if sys.argv[1:] == ['install', '--frozen-lockfile']:
    sys.exit(0)
assert sys.argv[1:4] == ['run', 'build', '--outDir'], sys.argv
shutil.copytree('source', sys.argv[4])
''')
        self.bun.chmod(0o755)
        self.put('index.html', b'<html>old</html>')
        self.put('assets/nested/app.js', b'old asset\x00')
        shutil.copytree(self.source, self.dist)
        # Deliberately old timestamps: equality must preserve files AND directories.
        for p in [self.dist] + list(self.dist.rglob('*')):
            os.utime(p, ns=(1000000000, 1000000000))

    def put(self, path, data):
        p = self.source / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)

    def state(self):
        return {str(p.relative_to(self.dist)): (p.stat().st_mtime_ns,
                None if p.is_dir() else p.read_bytes())
                for p in [self.dist] + list(self.dist.rglob('*'))}

    def run_build(self, fail=None):
        env = dict(os.environ)
        env.pop('BUNDLE_FAIL', None)
        if fail:
            env['BUNDLE_FAIL'] = fail
        result = subprocess.run([sys.executable, str(HELPER), '--web', str(self.web),
                                 '--bun', str(self.bun)], env=env, capture_output=True, text=True)
        self.assertEqual(list(self.web.glob('.dist-stage-*')), [])
        return result

    def assert_published(self):
        self.assertEqual(bundle.inventory(self.source), bundle.inventory(self.dist))
        for p in bundle.inventory(self.source)[0]:
            self.assertEqual((self.source / p).read_bytes(), (self.dist / p).read_bytes())

    def test_identical_preserves_every_file_and_directory_mtime(self):
        before = self.state()
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('unchanged', result.stdout)
        self.assertEqual(self.state(), before)

    def test_one_byte_with_same_size_and_mtime_is_published(self):
        self.put('assets/nested/app.js', b'new asset\x00')
        os.utime(self.source / 'assets/nested/app.js', ns=(1000000000, 1000000000))
        self.assertEqual(self.run_build().returncode, 0)
        self.assert_published()

    def test_new_file_is_published(self):
        self.put('assets/new file.bin', b'\x00\xff')
        self.assertEqual(self.run_build().returncode, 0)
        self.assert_published()

    def test_removed_file_is_removed(self):
        (self.source / 'assets/nested/app.js').unlink()
        self.assertEqual(self.run_build().returncode, 0)
        self.assert_published()

    def test_removed_directory_is_removed(self):
        shutil.rmtree(self.source / 'assets')
        self.assertEqual(self.run_build().returncode, 0)
        self.assert_published()

    def test_empty_directory_lists_are_compared(self):
        (self.source / 'empty').mkdir()
        self.assertEqual(self.run_build().returncode, 0)
        self.assert_published()

    def test_first_build(self):
        shutil.rmtree(self.dist)
        self.assertEqual(self.run_build().returncode, 0)
        self.assert_published()

    def test_install_failure_keeps_old_dist_and_cleans_stage(self):
        before = self.state()
        result = self.run_build('install')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.state(), before)

    def test_build_failure_keeps_old_dist_and_cleans_partial_stage(self):
        before = self.state()
        result = self.run_build('run')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.state(), before)

    def test_missing_index_keeps_old_dist(self):
        before = self.state()
        (self.source / 'index.html').unlink()
        self.assertNotEqual(self.run_build().returncode, 0)
        self.assertEqual(self.state(), before)

    def test_file_becomes_directory(self):
        (self.source / 'assets/nested/app.js').unlink()
        self.put('assets/nested/app.js/new.js', b'new')
        self.assertEqual(self.run_build().returncode, 0)
        self.assert_published()

    def test_directory_becomes_file(self):
        shutil.rmtree(self.source / 'assets')
        self.put('assets', b'flat')
        self.assertEqual(self.run_build().returncode, 0)
        self.assert_published()

    def test_assets_before_html_and_stale_cleanup_last(self):
        self.put('assets/new.js', b'new')
        self.put('index.html', b'<html>new</html>')
        (self.source / 'assets/nested/app.js').unlink()
        stage = self.root / 'stage'
        shutil.copytree(self.source, stage)
        actual_replace = os.replace
        events = []
        def replace(source, target):
            # Same-filesystem rename of a complete built file; never copy into dist.
            self.assertEqual(Path(source).stat().st_dev, Path(target).parent.stat().st_dev)
            self.assertTrue((self.dist / 'assets/nested/app.js').exists())
            events.append(Path(target).relative_to(self.dist).as_posix())
            actual_replace(source, target)
        with patch.object(bundle.os, 'replace', replace):
            bundle.publish(stage, self.dist)
        self.assertEqual(events, ['assets/new.js', 'index.html'])
        self.assert_published()

unittest.main()
PY
