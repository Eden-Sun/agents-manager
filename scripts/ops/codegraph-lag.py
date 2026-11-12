#!/usr/bin/env python3
"""主 checkout 的 codegraph 索引落後多少（唯讀，不跑 sync、不改任何檔）。

  python3 scripts/ops/codegraph-lag.py --repo ~/project/agents-manager [--max-commits 20] [--max-minutes 30] [--json]

判斷方式（codegraph 的 project_metadata 沒有「最後同步的 commit」欄位，`indexed_at_commit` 一直是空字串，
所以改用內容雜湊）：
- 索引 `files.content_hash` 是該檔內容的 sha256。對每個索引範圍內的檔（HEAD 的 tracked 檔，副檔名與索引裡的一致），
  索引的雜湊同時不等於「主樹現在的檔」與「HEAD 的 blob」，就算過期（stale）。主樹的檔有未提交改動時，索引跟著
  主樹走是正常的，所以兩邊都要不吻合才算落後。索引有、HEAD 已刪的檔也算過期。
- lag_commits ＝ 碰到過期檔的不同 commit 數（`git log HEAD -- <過期檔>`）。
- lag_minutes ＝ 最舊那個未反映 commit 的 committer 時間到現在幾分鐘。
- 落後超過 --max-commits 或 --max-minutes 才回 exit 1（kick 據此喊 ops-alert）；索引缺席回 exit 2。

--json 輸出一行 JSON：status（ok／lag／no_index／error）、head、stale、lag_commits、lag_minutes、
index_built_at（files.indexed_at 最大值，UTC）、interrupted_refs（unresolved_refs 數，codegraph 警告的那種）。
"""
import argparse
import hashlib
import json
import os
import sqlite3
import subprocess
import sys
import time


def git(repo: str, *args: str, input_bytes: bytes | None = None) -> bytes:
    return subprocess.run(['git', '-C', repo, *args], input=input_bytes, capture_output=True, check=True).stdout


def head_blobs(repo: str) -> dict[str, str]:
    """path → blob sha（HEAD 的 tracked 檔）。"""
    out = git(repo, 'ls-tree', '-r', '-z', 'HEAD')
    res = {}
    for rec in out.split(b'\0'):
        if not rec:
            continue
        meta, path = rec.split(b'\t', 1)
        parts = meta.split()
        if parts[1] == b'blob':
            res[path.decode()] = parts[2].decode()
    return res


def blob_hashes(repo: str, shas: list[str]) -> dict[str, str]:
    """blob sha → sha256(內容)，一次 cat-file --batch 讀完。"""
    if not shas:
        return {}
    inp = ('\n'.join(shas) + '\n').encode()
    out = git(repo, 'cat-file', '--batch', input_bytes=inp)
    res, pos = {}, 0
    for sha in shas:
        nl = out.index(b'\n', pos)
        size = int(out[pos:nl].split()[2])
        data = out[nl + 1: nl + 1 + size]
        res[sha] = hashlib.sha256(data).hexdigest()
        pos = nl + 1 + size + 1
    return res


def unindexed_commits(repo: str, stale: list[str], idx: dict[str, str]) -> dict[str, int]:
    """過期檔各自：索引版本之後（不含）碰過它的 commit → {sha: committer 時間}。

    索引版本＝歷史上第一個內容雜湊等於索引雜湊的 commit（由新往舊找）；找不到（索引是未提交的版本）就整段算。
    """
    commits: dict[str, int] = {}
    for path in stale:
        out = git(repo, 'log', '--format=%H %ct', 'HEAD', '--', path).decode().splitlines()
        rows = [(line.split()[0], int(line.split()[1])) for line in out if line.strip()]
        want = idx.get(path)
        n = 0
        if want:
            inp = ''.join(f'{sha}:{path}\n' for sha, _ in rows).encode()
            blob = git(repo, 'cat-file', '--batch', input_bytes=inp) if rows else b''
            pos = 0
            for sha, _ in rows:
                nl = blob.index(b'\n', pos)
                head = blob[pos:nl].split()
                if head[0] == b'missing':
                    pos = nl + 1
                    continue
                size = int(head[2])
                data = blob[nl + 1: nl + 1 + size]
                pos = nl + 1 + size + 1
                if hashlib.sha256(data).hexdigest() == want:
                    break
                n += 1
        else:
            n = len(rows)
        for sha, ct in rows[:n]:
            commits[sha] = ct
    return commits


def check(repo: str, max_commits: int, max_minutes: float) -> dict:
    db = os.path.join(repo, '.codegraph', 'codegraph.db')
    res: dict = {'status': 'ok', 'head': git(repo, 'rev-parse', '--short', 'HEAD').decode().strip(),
                 'stale': 0, 'lag_commits': 0, 'lag_minutes': 0.0, 'index_built_at': None, 'interrupted_refs': 0}
    if not os.path.exists(db):
        res['status'] = 'no_index'
        return res
    con = sqlite3.connect(f'file:{db}?mode=ro', uri=True, timeout=5)
    try:
        idx = dict(con.execute('select path, content_hash from files'))
        built = con.execute('select max(indexed_at) from files').fetchone()[0]
        res['index_built_at'] = time.strftime('%F %T', time.gmtime((built or 0) / 1000))
        try:
            res['interrupted_refs'] = con.execute('select count(*) from unresolved_refs').fetchone()[0]
        except sqlite3.Error:
            pass
    finally:
        con.close()

    exts = {os.path.splitext(p)[1] for p in idx}
    blobs = head_blobs(repo)
    scope = {p: s for p, s in blobs.items() if os.path.splitext(p)[1] in exts}
    shas = sorted(set(scope.values()))
    head_h = blob_hashes(repo, shas)

    stale: list[str] = []
    for p, sha in scope.items():
        h = idx.get(p)
        if h is None:
            stale.append(p)
            continue
        if h == head_h[sha]:
            continue
        wt = os.path.join(repo, p)
        if os.path.isfile(wt):
            with open(wt, 'rb') as f:
                if h == hashlib.sha256(f.read()).hexdigest():
                    continue
        stale.append(p)
    # 索引有、HEAD 沒有、主樹也沒有的檔（已刪）才算過期；主樹還在的是未追蹤檔，索引照主樹走，不算。
    stale += [p for p in idx if p not in scope and p not in blobs and not os.path.exists(os.path.join(repo, p))]
    res['stale'] = len(stale)
    if not stale:
        return res

    commits = unindexed_commits(repo, stale, idx)
    res['lag_commits'] = len(commits)
    oldest = min(commits.values()) if commits else time.time()
    res['lag_minutes'] = round((time.time() - oldest) / 60, 1)
    if res['lag_commits'] > max_commits or res['lag_minutes'] > max_minutes:
        res['status'] = 'lag'
    return res


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument('--repo', default=os.path.expanduser('~/project/agents-manager'))
    ap.add_argument('--max-commits', type=int, default=20)
    ap.add_argument('--max-minutes', type=float, default=30)
    ap.add_argument('--json', action='store_true')
    a = ap.parse_args()
    try:
        r = check(a.repo, a.max_commits, a.max_minutes)
    except (subprocess.CalledProcessError, sqlite3.Error, ValueError, OSError) as e:
        r = {'status': 'error', 'detail': str(e)[:300]}
    print(json.dumps(r, ensure_ascii=False) if a.json else
          ' '.join(f'{k}={v}' for k, v in r.items()))
    return {'ok': 0, 'lag': 1, 'no_index': 2, 'error': 2}[r['status']]


if __name__ == '__main__':
    sys.exit(main())
