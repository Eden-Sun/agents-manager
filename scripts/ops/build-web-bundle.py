#!/usr/bin/env python3
"""Build off to the side; preserve an identical dist, publish changed files atomically."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def inventory(root):
    files, directories = set(), set()
    if root.exists():
        for current, dirs, names in os.walk(root):
            relative = Path(current).relative_to(root)
            directories.update(relative / name for name in dirs)
            files.update(relative / name for name in names)
    return files, directories


def same_bytes(left, right):
    with left.open('rb') as a, right.open('rb') as b:
        while True:
            chunk = a.read(1024 * 1024)
            if chunk != b.read(1024 * 1024):
                return False
            if not chunk:
                return True


def publish(stage, dist):
    new_files, new_dirs = inventory(stage)
    old_files, old_dirs = inventory(dist)
    if (dist.is_dir() and new_files == old_files and new_dirs == old_dirs
            and all(same_bytes(stage / p, dist / p) for p in new_files)):
        print('web bundle unchanged: preserved dist and all directory/file mtimes', flush=True)
        return

    # Staging is a sibling on the destination filesystem. Cargo runs only after
    # this helper returns successfully. Each rename exposes complete file bytes;
    # this is intentionally not a transaction across the whole directory tree.
    dist.mkdir(exist_ok=True)
    if stage.stat().st_dev != dist.stat().st_dev:
        raise OSError('staging and dist must be on the same filesystem')

    # Resolve file/directory shape changes before installing their replacements.
    for path in old_files & new_dirs:
        (dist / path).unlink()
    for path in sorted(old_dirs, key=lambda p: len(p.parts), reverse=True):
        if path in new_files or any(parent in new_files for parent in path.parents):
            for child in (dist / path).iterdir():
                child.unlink()
            (dist / path).rmdir()
    for path in sorted(new_dirs, key=lambda p: len(p.parts)):
        (dist / path).mkdir(exist_ok=True)

    # Assets first, HTML entrypoints last; stale assets remain until then.
    for path in sorted(new_files, key=lambda p: (p.suffix.lower() == '.html', str(p))):
        target = dist / path
        if target.parent.stat().st_dev != stage.stat().st_dev:
            raise OSError('destination file is not on the staging filesystem')
        os.replace(stage / path, target)
    for path in old_files - new_files:
        target = dist / path
        if target.is_file():
            target.unlink()
    for path in sorted(old_dirs - new_dirs, key=lambda p: len(p.parts), reverse=True):
        target = dist / path
        if target.is_dir():
            target.rmdir()
    print('web bundle changed: published assets, then HTML, then removed stale files', flush=True)


def build(web, bun):
    web = web.resolve()
    # Do not create staging under dist: that alone would invalidate Cargo's watch.
    with tempfile.TemporaryDirectory(prefix='.dist-stage-', dir=web) as temporary:
        stage = Path(temporary) / 'bundle'
        subprocess.run([bun, 'install', '--frozen-lockfile'], cwd=web, check=True)
        subprocess.run([bun, 'run', 'build', '--outDir', str(stage)], cwd=web, check=True)
        if not (stage / 'index.html').is_file():
            raise RuntimeError('web build did not produce staging/index.html')
        publish(stage, web / 'dist')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--web', required=True, type=Path)
    parser.add_argument('--bun', default='bun')
    args = parser.parse_args()
    try:
        build(args.web, args.bun)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print('web bundle build/publish failed: {}'.format(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
