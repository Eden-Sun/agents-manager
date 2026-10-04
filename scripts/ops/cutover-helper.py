#!/usr/bin/env python3
"""cutover-to-host.sh 的輔助（issue #721，#675）：打 daemon API、做快照、判斷停好了沒、驗證。

同一支在來源（Mac）本機跑，也經 `ssh <目標> python3 - <子命令> … < cutover-helper.py` 在目標跑，
所以只用標準函式庫、token 在行程內讀（不放進 argv，`ps` 看不到）。

  api        <METHOD> <PATH> [--body JSON]          打一支 API，印回應；2xx→0、409→3、其他→1
  snapshot   --label L [--label L …] --out F        GET /api/state，挑出這幾個專案的 bot 與 run 狀態
  active     --snapshot F [--user-only]             印快照裡 run 還活著的 bot id（child 先）
  wait-idle  --snapshot F [--timeout S]             等快照裡的 bot 全部沒有活著的 run
  lock-free  <資料目錄>                             daemon.lock 拿得到（daemon 停了）→0，拿不到→1
  verify     --snapshot F --running F --map A=B …   目標 daemon 的 state 對不對得上來源快照
  transcript-gate --bundle F --report F --running F transcript-transfer 回 2 時：只有舊 session 找不到才放行，印出段數
  drill-verify --db F --config F --bundle F --map A=B …   演練：直接讀匯入後的 DB 複本與 config
"""

import argparse
import fcntl
import gzip
import json
import os
import sqlite3
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

ACTIVE = ("starting", "running", "stopping")
LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost", "0:0:0:0:0:0:0:1"}


def die(msg, code=1):
    print(f"cutover-helper: {msg}", file=sys.stderr)
    sys.exit(code)


def token(args):
    path = os.path.expanduser(args.token_file)
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError as e:
        die(f"讀不到 UI token {path}：{e}")


def check_loopback(base):
    try:
        parts = urllib.parse.urlsplit(base)
        host = (parts.hostname or "").lower()
        has_credentials = parts.username is not None or parts.password is not None
    except ValueError:
        die("daemon base URL 格式錯誤")
    if parts.scheme not in ("http", "https") or host not in LOOPBACK_HOSTS or has_credentials:
        die("daemon base URL 必須是沒有帳密的 loopback 位址")
    if parts.query or parts.fragment:
        die("daemon base URL 不可帶 query 或 fragment")
    return base.rstrip("/")


def redact(value, secret):
    if not secret:
        return value
    if isinstance(value, str):
        return value.replace(secret, "[redacted]")
    if isinstance(value, list):
        return [redact(item, secret) for item in value]
    if isinstance(value, dict):
        return {redact(key, secret): redact(item, secret) for key, item in value.items()}
    return value


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, _req, _fp, _code, _msg, _headers, _newurl):
        return None


def call(args, method, path, body=None):
    """回 (http code, 解析過的 JSON 或原文)。連不上回 (0, 錯誤字串)。"""
    data = None if body is None else json.dumps(body).encode()
    base = check_loopback(args.base)
    secret = token(args)
    req = urllib.request.Request(base + path, data=data, method=method,
                                 headers={"X-AM-Token": secret, "Content-Type": "application/json"})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        with opener.open(req, timeout=args.timeout_http) as r:
            code, raw = r.status, r.read()
    except urllib.error.HTTPError as e:
        code, raw = e.code, e.read()
    except (urllib.error.URLError, OSError) as e:
        return 0, str(e)
    try:
        parsed = json.loads(raw or b"null")
    except ValueError:
        parsed = raw.decode(errors="replace")
    return code, redact(parsed, secret)


def get_state(args):
    code, st = call(args, "GET", "/api/state")
    if code != 200 or not isinstance(st, dict):
        die(f"GET /api/state 失敗（HTTP {code}）：{str(st)[:200]}")
    return st


def pick(state, labels):
    out = []
    for label in labels:
        hits = [p for p in state.get("projects") or [] if p.get("label") == label]
        if len(hits) != 1:
            die(f"state 裡 label = {label!r} 的專案有 {len(hits)} 個（要剛好一個）")
        p = hits[0]
        out.append({
            "id": p["id"], "label": p["label"], "path": p.get("path"), "host": p.get("host") or "local",
            "handed_off_to": p.get("handed_off_to"),
            "bots": [{"id": b["id"], "name": b["name"], "kind": b.get("kind"), "managed_by": b.get("managed_by"),
                      "parent_bot_id": b.get("parent_bot_id"), "cwd": b.get("cwd"),
                      "run_state": (b.get("run") or {}).get("state")} for b in p.get("bots") or []],
        })
    return out


def load(path):
    with open(path) as f:
        return json.load(f)


def active_ids(projects, user_only=False):
    bots = [b for p in projects for b in p["bots"] if b["run_state"] in ACTIVE]
    if user_only:
        bots = [b for b in bots if b["managed_by"] != "child"]
    # child 先停：先停 parent 的話，它的 child 會以為 parent 還在叫它做事。
    bots.sort(key=lambda b: (b["managed_by"] != "child",))
    return [b["id"] for b in bots]


# ---------------------------------------------------------------- 子命令


def cmd_api(args):
    method = args.method.upper()
    # POST／PATCH 沒給 body 就送 `{}`：supervisor start／stop 這類 handler 收 JSON，空 body 會被 axum 擋成 4xx。
    body = json.loads(args.body) if args.body else ({} if method in ("POST", "PATCH", "PUT") else None)
    code, out = call(args, method, args.path, body)
    print(out if isinstance(out, str) else json.dumps(out, ensure_ascii=False))
    if 200 <= code < 300:
        return 0
    print(f"HTTP {code}", file=sys.stderr)
    return 3 if code == 409 else 1


def cmd_snapshot(args):
    projects = pick(get_state(args), args.label)
    fd = os.open(args.out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump({"taken_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "projects": projects}, f, ensure_ascii=False, indent=2)
    for p in projects:
        run = [b["name"] for b in p["bots"] if b["run_state"] in ACTIVE]
        print(f"{p['label']}（{p['id']}，host={p['host']}，handed_off_to={p['handed_off_to']}）："
              f"{len(p['bots'])} 顆 bot，在跑 {len(run)}：{', '.join(run) or '—'}")
    return 0


def cmd_active(args):
    for i in active_ids(load(args.snapshot)["projects"], args.user_only):
        print(i)
    return 0


def cmd_wait_idle(args):
    want = {b["id"] for p in load(args.snapshot)["projects"] for b in p["bots"]}
    deadline = time.time() + args.timeout
    while True:
        st = get_state(args)
        left = [b["name"] for p in st.get("projects") or [] for b in p.get("bots") or []
                if b["id"] in want and (b.get("run") or {}).get("state") in ACTIVE]
        if not left:
            return 0
        if time.time() >= deadline:
            print(f"逾時：還有 {len(left)} 顆在跑：{', '.join(left)}", file=sys.stderr)
            return 1
        time.sleep(2)


def cmd_lock_free(args):
    path = os.path.join(os.path.expanduser(args.data_dir), "daemon.lock")
    if not os.path.exists(path):
        return 0
    fd = os.open(path, os.O_RDONLY)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return 1
    finally:
        os.close(fd)  # 關 fd 就放鎖：只是看一眼，不拿著
    return 0


def parse_maps(items):
    out = []
    for item in items or []:
        src, sep, dst = item.partition("=")
        if not sep:
            die(f"--map 要寫成 /來源=/目標：{item!r}")
        out.append((src.rstrip("/"), dst.rstrip("/")))
    return sorted(out, key=lambda m: len(m[0]), reverse=True)


def map_path(path, maps):
    for src, dst in maps:
        if path == src or (path or "").startswith(src + "/"):
            return dst + path[len(src):]
    return path


def latest_sessions(runs):
    """每顆 bot 最後一段原生對話（started_at、再照原本順序；接回挑的就是它，§11.9／#461）。"""
    last = {}
    for i, r in enumerate(runs):
        sid = r.get("native_session_id")
        if not sid:
            continue
        key = (r.get("started_at") or "", i)
        if r["bot_id"] not in last or key >= last[r["bot_id"]][0]:
            last[r["bot_id"]] = (key, sid)
    return {b: v[1] for b, v in last.items()}


def cmd_transcript_gate(args):
    """transcript-transfer 結束碼 2 之後：conflicts／refused 一律擋；missing 只有在「在跑名單裡的 bot 最後一段」時擋
    （那顆接回就會開新對話），更早的舊 session 在來源已經沒有檔、也不會被接回，放行並印出段數給 import 的閘門比對。"""
    report = load(args.report)
    with gzip.open(args.bundle, "rb") as f:
        bundle = json.loads(f.read().decode())
    bad = []
    if report.get("conflicts"):
        bad.append(f"目標已有內容不同的同名檔 {len(report['conflicts'])} 個（例：{report['conflicts'][0]}）：確定要蓋才手動加 --overwrite 重跑")
    if report.get("refused"):
        bad.append(f"拒絕搬 {len(report['refused'])} 段（例：{report['refused'][0].get('why')}）")
    missing = {m.get("session") for m in report.get("missing") or []}
    resume = set(load(args.running).get("resume") or [])
    names = {b["id"]: b.get("name") for b in bundle["tables"]["bots"]["rows"]}
    latest = latest_sessions(bundle["tables"]["runs"]["rows"])
    critical = sorted(names.get(b, b) for b in resume if latest.get(b) in missing)
    if critical:
        bad.append(f"要接回的 bot 最後一段對話找不到：{', '.join(critical)}（接回會開新對話）")
    for m in bad:
        print(m, file=sys.stderr)
    print(len(missing))
    return 1 if bad else 0


def cmd_verify(args):
    """目標 daemon 起來、接回之後：專案在、本機、path 換過、user bot 一顆不少、在跑名單都活著。"""
    src = load(args.snapshot)["projects"]
    running = set(load(args.running).get("resume") or [])
    maps = parse_maps(args.map)
    st = get_state(args)
    by_id = {p["id"]: p for p in st.get("projects") or []}
    problems, report = [], []
    for p in src:
        t = by_id.get(p["id"])
        if t is None:
            problems.append(f"目標沒有專案 {p['label']}（{p['id']}）")
            continue
        if (t.get("host") or "local") != "local":
            problems.append(f"{p['label']} 在目標的 host={t.get('host')!r}，不是 local")
        if t.get("path") != map_path(p["path"], maps):
            problems.append(f"{p['label']} 在目標的 path={t.get('path')!r}，預期 {map_path(p['path'], maps)!r}")
        if t.get("handed_off_to"):
            problems.append(f"{p['label']} 在目標還掛著 handed_off_to={t['handed_off_to']!r}")
        tb = {b["id"]: b for b in t.get("bots") or []}
        users = [b for b in p["bots"] if b["managed_by"] != "child"]
        miss = [b["name"] for b in users if b["id"] not in tb]
        if miss:
            problems.append(f"{p['label']} 少了 user bot：{', '.join(miss)}")
        children = [b for b in p["bots"] if b["managed_by"] == "child"]
        dead = [b["name"] for b in tb.values() if b["id"] in running and (b.get("run") or {}).get("state") not in ACTIVE]
        if dead:
            problems.append(f"{p['label']} 在跑名單裡這些沒接回：{', '.join(dead)}")
        report.append({"project": p["label"], "user_bots": f"{len(users) - len(miss)}/{len(users)}",
                       "children_in_target": sum(1 for b in tb.values() if b.get("managed_by") == "child"),
                       "children_in_source": len(children),
                       "running": sum(1 for b in tb.values() if (b.get("run") or {}).get("state") in ACTIVE)})
    print(json.dumps({"ok": not problems, "projects": report, "problems": problems}, ensure_ascii=False, indent=2))
    return 0 if not problems else 1


def cmd_drill_verify(args):
    """演練：匯入後的 DB 複本與 config 對不對得上 bundle（不起 daemon，直接讀檔）。"""
    try:
        import tomllib
    except ImportError:
        die("需要 python 3.11 以上（tomllib）")
    with gzip.open(args.bundle, "rb") as f:
        bundle = json.loads(f.read().decode())
    maps = parse_maps(args.map)
    proj = bundle["tables"]["projects"]["rows"][0]
    pid, want_path = proj["id"], map_path(proj["path"], maps)
    live_bots = bundle["tables"]["bots"]["rows"]
    conn = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
    problems = []
    try:
        row = conn.execute("SELECT host, path, deleted_at FROM projects WHERE id = ?", (pid,)).fetchone()
        if row is None:
            problems.append(f"DB 沒有專案 {pid}")
        else:
            if row[0] != "local":
                problems.append(f"DB 的 host={row[0]!r}，不是 local")
            if row[1] != want_path:
                problems.append(f"DB 的 path={row[1]!r}，預期 {want_path!r}")
            if row[2]:
                problems.append(f"DB 的專案被軟刪了（{row[2]}）")
        n = conn.execute("SELECT COUNT(*) FROM bots WHERE project_id = ? AND deleted_at IS NULL", (pid,)).fetchone()[0]
        if n != len(live_bots):
            problems.append(f"DB 活著的 bot {n} 顆，bundle 有 {len(live_bots)} 顆")
        bad_cwd = [c for (c,) in conn.execute(
            "SELECT cwd FROM bots WHERE project_id = ? AND cwd IS NOT NULL AND cwd <> ''", (pid,))
            if c != map_path(c, maps)]
        if bad_cwd:
            problems.append(f"bot 的 cwd 還是來源路徑：{bad_cwd[:3]}")
        fk = conn.execute("PRAGMA foreign_key_check").fetchall()
        if fk:
            problems.append(f"外鍵檢查沒過：{fk[:5]}")
        ok = conn.execute("PRAGMA integrity_check").fetchone()[0]
        if ok != "ok":
            problems.append(f"integrity_check：{ok}")
        # 每段原生對話最後一個 run 的 transcript：目標 daemon 接回前會先看它在不在（SPEC §11.9a）。
        # 擋的是每顆 bot 最後一段（接回挑的那段）；更早的舊 session 在來源已經沒有檔的只列數字。
        last, by_bot = {}, {}
        for bot, sid, path in conn.execute(
                "SELECT r.bot_id, r.native_session_id, r.transcript_path FROM runs r JOIN bots b ON b.id = r.bot_id "
                "WHERE b.project_id = ? AND b.deleted_at IS NULL AND r.native_session_id IS NOT NULL "
                "AND r.transcript_path IS NOT NULL ORDER BY r.started_at, r.rowid", (pid,)):
            last[sid] = path
            by_bot[bot] = path
        gone = sorted(p for p in last.values() if not os.path.exists(p))
        gone_latest = sorted(p for p in by_bot.values() if not os.path.exists(p))
    finally:
        conn.close()
    if gone_latest:
        problems.append(f"{len(gone_latest)} 顆 bot 最後一段對話的 transcript 不在（例：{gone_latest[0]}）")
    with open(args.config, "rb") as f:
        cfg = tomllib.load(f)
    cp = [p for p in cfg.get("projects") or [] if p.get("id") == pid]
    if len(cp) != 1:
        problems.append(f"config.toml 裡專案 {pid} 有 {len(cp)} 個")
    elif cp[0].get("host", "local") != "local" or cp[0].get("path") != want_path:
        problems.append(f"config.toml 的專案 host={cp[0].get('host')!r} path={cp[0].get('path')!r}")
    elif any(b.get("autostart") for b in cp[0].get("bots") or []):
        problems.append("config.toml 有 bot autostart = true（匯入應一律關掉）")
    print(json.dumps({"ok": not problems, "project": proj.get("label"), "bots": len(live_bots),
                      "sessions": len(last), "transcripts_missing_old": len(gone) - len(gone_latest), "problems": problems},
                     ensure_ascii=False, indent=2))
    return 0 if not problems else 1


def main():
    ap = argparse.ArgumentParser(prog="cutover-helper", description=__doc__.split("\n\n")[0])
    ap.add_argument("--base", default="http://127.0.0.1:7788")
    ap.add_argument("--token-file", default="~/.config/agents-manager/ui-token")
    ap.add_argument("--timeout-http", type=float, default=60)
    sub = ap.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("api")
    a.add_argument("method")
    a.add_argument("path")
    a.add_argument("--body")
    s = sub.add_parser("snapshot")
    s.add_argument("--label", action="append", required=True)
    s.add_argument("--out", required=True)
    s = sub.add_parser("active")
    s.add_argument("--snapshot", required=True)
    s.add_argument("--user-only", action="store_true")
    s = sub.add_parser("wait-idle")
    s.add_argument("--snapshot", required=True)
    s.add_argument("--timeout", type=float, default=120)
    s = sub.add_parser("lock-free")
    s.add_argument("data_dir")
    s = sub.add_parser("verify")
    s.add_argument("--snapshot", required=True)
    s.add_argument("--running", required=True)
    s.add_argument("--map", action="append", default=[])
    s = sub.add_parser("transcript-gate")
    s.add_argument("--bundle", required=True)
    s.add_argument("--report", required=True)
    s.add_argument("--running", required=True)
    s = sub.add_parser("drill-verify")
    s.add_argument("--db", required=True)
    s.add_argument("--config", required=True)
    s.add_argument("--bundle", required=True)
    s.add_argument("--map", action="append", default=[])
    args = ap.parse_args()
    fn = {"api": cmd_api, "snapshot": cmd_snapshot, "active": cmd_active, "wait-idle": cmd_wait_idle,
          "lock-free": cmd_lock_free, "verify": cmd_verify, "transcript-gate": cmd_transcript_gate,
          "drill-verify": cmd_drill_verify}[args.cmd]
    sys.exit(fn(args))


if __name__ == "__main__":
    main()
