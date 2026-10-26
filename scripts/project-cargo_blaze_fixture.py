#!/usr/bin/env python3
"""Opt-in Linux integration check; requires installed Blaze. Never run by ops."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true', help='run real compiler invocations')
    args = parser.parse_args()
    if not args.run:
        parser.error('explicit --run is required')
    if not sys.platform.startswith('linux'):
        parser.error('PID namespace verification currently requires Linux /proc')
    repo = Path(__file__).resolve().parent.parent
    subprocess.run([str(repo / 'scripts/wait-load.sh'), '--max', '40',
                    '--min-free-mem', '30'], check=True)
    root = Path(tempfile.mkdtemp(prefix='agm-blaze-fixture-'))
    print(f'Blaze fixture evidence: {root}', flush=True)
    base = root / 'fixture'
    (base / 'src').mkdir(parents=True)
    (base / 'Cargo.toml').write_text(
        '[package]\nname="stale-guard-fixture"\nversion="0.1.0"\nedition="2021"\n[workspace]\n')
    (base / 'build.rs').write_bytes((repo / 'daemon/build.rs').read_bytes())
    (base / '.gitignore').write_text('/target\n/Cargo.lock\n')
    (base / 'src/main.rs').write_text(
        'mod a; mod b; mod c; fn main(){ println!("{},{},{}",a::n(),b::n(),c::n()); }\n')
    values = dict(a=1, b=10, c=100)

    def edit(module, value):
        values[module] = value
        (base / f'src/{module}.rs').write_text(f'pub fn n()->i32 {{ {value} }}\n')

    for module, value in values.items():
        edit(module, value)
    for command in [['init', '-q'], ['add', '.'],
                    ['-c', 'user.name=Fixture', '-c', 'user.email=fixture@local',
                     'commit', '-qm', 'fixture']]:
        subprocess.run(['git', *command], cwd=base, check=True)
    runtime_base = root / 'runtime'
    runtime = runtime_base / 'native-base-v1'
    # Deliberately conflicting aliases test that the ENTRYPOINT enforces the guard.
    # All paths are private even if that enforcement regresses.
    env = os.environ | dict(
        AGM_RUST_ENGINE='blaze', AGM_BLAZE_RUNTIME_HOME=str(runtime_base),
        AGM_BLAZE_IDLE_SECS='60', RA_JIT_HOME=str(root / 'old-ra'),
        BLAZE_HOME=str(root / 'old-blaze'), RA_JIT_RUSTC_BASE='1',
        BLAZE_RUSTC_BASE='1', RA_JIT_CACHE=str(root / 'store'),
        BLAZE_CACHE=str(root / 'store'), CARGO_TARGET_DIR=str(base / 'target'))
    owned = set()
    records = []

    def process_env(pid):
        try:
            return (Path('/proc') / str(pid) / 'environ').read_bytes().split(b'\0')
        except OSError:
            return []

    # Include the two private decoy namespaces so failed enforcement still cleans up.
    namespaces = [runtime, runtime_base, root / 'old-ra', root / 'old-blaze']

    def private(pid):
        variables = process_env(pid)
        return any(f'RA_JIT_HOME={path}'.encode() in variables for path in namespaces)

    def collect():
        for entry in Path('/proc').iterdir():
            if entry.name.isdigit() and private(int(entry.name)):
                owned.add(int(entry.name))
        (root / 'owned-pids.json').write_text(json.dumps(sorted(owned)))

    def invoke(command, label):
        with (root / f'{label}.stdout').open('w') as out, \
                (root / f'{label}.stderr').open('w') as err:
            proc = subprocess.Popen(command, cwd=base, env=env, stdout=out, stderr=err)
            with (root / 'foreground-pids.jsonl').open('a') as log:
                log.write(json.dumps(dict(label=label, pid=proc.pid)) + '\n')
            try:
                return proc.wait(timeout=1200)
            finally:
                if proc.poll() is None:
                    proc.terminate()  # Only this invocation's recorded PID.
                    proc.wait(timeout=5)
                collect()

    def step(label, subcommand='run', error=False):
        started = time.monotonic()
        rc = invoke(['sh', str(repo / 'scripts/project-cargo.sh'), subcommand], label)
        output = (root / f'{label}.stdout').read_text().strip()
        if subcommand == 'build' and rc == 0:
            binary_rc = invoke([str(base / 'target/debug/stale-guard-fixture')], label + '-binary')
            output = (root / f'{label}-binary.stdout').read_text().strip()
            if binary_rc:
                raise AssertionError(f'{label}: native binary exited {binary_rc}')
        expected = ','.join(str(value) for value in values.values())
        passed = (rc != 0 and not output) if error else (rc == 0 and output == expected)
        record = dict(label=label, subcommand=subcommand, seconds=time.monotonic()-started,
                      rc=rc, expected='nonzero, no output' if error else expected,
                      actual=output, passed=passed,
                      sources={m: (base / f'src/{m}.rs').read_text() for m in values})
        records.append(record)
        (root / 'results.json').write_text(json.dumps(records, indent=2))
        print(json.dumps(record), flush=True)
        if not passed:
            raise AssertionError(f'{label}: first invocation failed; see {root}')

    try:
        step('00-cold')
        if not (runtime / 'daemon.sock').exists():
            raise AssertionError('entrypoint did not use the safety-versioned runtime')
        worker_vars = [process_env(pid) for pid in owned if process_env(pid)]
        if not worker_vars or any(b'RA_JIT_RUSTC_BASE=0' not in v or
                                  b'BLAZE_RUSTC_BASE=0' not in v or
                                  f'BLAZE_HOME={runtime}'.encode() not in v for v in worker_vars):
            raise AssertionError('private daemon/worker did not inherit guard aliases')
        for label, module, value in [('01-first-edit', 'a', 2), ('02-same-a', 'a', 3),
                                     ('03-other-b', 'b', 20), ('04-same-b', 'b', 30),
                                     ('05-other-c', 'c', 200), ('06-same-c', 'c', 300)]:
            edit(module, value)
            step(label)
        (base / 'src/b.rs').write_text('pub fn n()->i32 { this is not valid Rust }\n')
        step('07-syntax-error', error=True)
        edit('b', 40)
        step('08-first-fixed')
        edit('a', 4)
        step('09-after-fix-other')
        edit('c', 400)
        step('10-build', subcommand='build')
        edit('a', 5)
        step('11-back-to-run')
    finally:
        collect()
        for pid in sorted(owned):
            if private(pid):
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
        # Only cleanup waits: never delay or repeat a build to hide stale output.
        deadline = time.monotonic() + 5
        survivors = [pid for pid in owned if private(pid)]
        while survivors and time.monotonic() < deadline:
            time.sleep(0.05)
            survivors = [pid for pid in owned if private(pid)]
        (root / 'cleanup.json').write_text(json.dumps(dict(survivors=survivors)))
        if survivors:
            raise RuntimeError(f'private processes survived SIGTERM: {survivors}; {root}')
    print(f'12 cases passed; private processes exited; evidence retained: {root}', flush=True)


if __name__ == '__main__':
    main()
