#!/usr/bin/env bash
# Backend selection, argument fidelity and failed-build propagation without starting a compiler.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
python3 - "$ROOT" <<'PY'
import json, os, pathlib, subprocess, sys, tempfile
root = pathlib.Path(sys.argv[1])
with tempfile.TemporaryDirectory(prefix='agm-blaze-test-') as tmp:
    base = pathlib.Path(tmp); tools = base/'tools'; tools.mkdir(); private = base/'private'; private.mkdir()
    log = base/'argv.json'
    def script(path, body):
        path.write_text('#!/bin/sh\n' + body); path.chmod(0o755)
    script(tools/'cargo', 'exec python3 - "$@" <<\'REC\'\nimport os,json,sys\nwith open(os.environ["TEST_LOG"], "w") as f: json.dump({"args":sys.argv[1:],"toolchain":os.getenv("RUSTUP_TOOLCHAIN"),"tests":os.getenv("RA_JIT_TESTS"),"threads":os.getenv("RUST_TEST_THREADS")},f)\nsys.exit(int(os.getenv("TEST_EXIT","0")))\nREC\n')
    script(tools/'rustup', 'exit "${TEST_RUSTUP_EXIT:-0}"\n')
    script(tools/'uname', 'case "$1" in -s) echo "${TEST_OS:-Linux}";; -m) echo "${TEST_ARCH:-x86_64}";; esac\n')
    script(private/'cargo-ra-jit', 'exit 0\n')
    env = {k:v for k,v in os.environ.items() if not k.startswith(('AM_', 'AGM_', 'BLAZE_', 'RA_JIT_')) and k not in ('RUSTUP_TOOLCHAIN', 'RUST_TEST_THREADS')}
    env.update(PATH=f'{tools}:{os.environ.get("AM_CANARY_DIR", "/nonexistent")}:/usr/bin:/bin', HOME=str(base/'home'), AGM_BLAZE_BIN=str(private), TEST_LOG=str(log))
    count = 0
    def run(args, expected, **changes):
        global count
        r = subprocess.run(['sh', str(root/'scripts/project-cargo.sh'), *args], env=env|changes, capture_output=True,text=True)
        assert r.returncode == int(changes.get('TEST_EXIT','0')), (args,r.stdout,r.stderr)
        got = json.loads(log.read_text()); assert got['args'] == expected,(args,got,r.stderr)
        count += 1
        return got
    run(['build','-p','xtask'], ['ra-jit','build','-p','xtask'])
    got=run(['test','a filter','--','--test-threads=2'], ['ra-jit','test','a filter','--','--test-threads=2'],RA_JIT_TESTS='affected',RUST_TEST_THREADS='3')
    assert got['tests']=='all' and got['threads']=='3' and got['toolchain']=='1.99.0'
    for args in [['build','--release'],['build','-r'],['run','--profile','custom'],['test','--profile=release'],['build','--target=x86_64-unknown-linux-gnu'],['build','--message-format=json'],['+nightly','build'],['check'],['clippy'],['fmt'],['dev']]:
        run(args,args)
    run(['run','--','--release'], ['ra-jit','run','--','--release'])
    run(['build'],['build'],AGM_RUST_ENGINE='cargo')
    run(['build'],['build'],TEST_RUSTUP_EXIT='1')
    run(['build'],['build'],TEST_OS='Darwin',TEST_ARCH='x86_64')
    run(['build'],['ra-jit','build'],TEST_EXIT='42')
    script(private/'cargo-blaze','exit 0\n')
    run(['build'],['blaze','build'])
    (private/'cargo-blaze').unlink(); (private/'cargo-ra-jit').unlink()
    run(['build'],['build'])
    for changes,code in [({'AGM_RUST_ENGINE':'blaze'},127),({'AGM_RUST_ENGINE':'invalid'},2),({'AGM_RUST_ENGINE':'blaze','TEST_OS':'Windows'},2),({'AGM_RUST_ENGINE':'blaze','TEST_RUSTUP_EXIT':'1'},127)]:
        log.unlink(missing_ok=True)
        r=subprocess.run(['sh',str(root/'scripts/project-cargo.sh'),'build'],env=env|changes,capture_output=True,text=True)
        assert r.returncode==code and not log.exists(),(changes,r.returncode,r.stderr)
        count+=1
    print(f'project-cargo: {count} cases passed')
PY
