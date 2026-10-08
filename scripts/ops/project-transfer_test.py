#!/usr/bin/env python3
"""scripts/ops/project-transfer 的隔離測試（issue #710）：假 DB、假 config、暫存目錄，不碰正式資料。

schema 直接從 crates/am-base/src/db.rs 的 `SCHEMA` 與 additive ALTER 名單抽出來建，daemon 加欄時這裡跟著變。
"""

import datetime
import fcntl
import gzip
import hashlib
import json
import os
import re
import sqlite3
import subprocess
import sys
import tempfile
import tomllib
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
TOOL = os.path.join(HERE, "project-transfer")
PID, OTHER = "01PROJTRANSFER0000000000P1", "01PROJTRANSFER0000000000Q1"
B1, B2, C1, QB = "01BOT000000000000000000B01", "01BOT000000000000000000B02", "01BOT000000000000000000C01", "01BOT000000000000000000Q01"
D1 = "01BOT000000000000000000D01"  # 已軟刪的 child
DELETED_ROWS = {"r-dead", "r-dead-kid", "t-dead", "m-dead", "m-dead-kid", "a-dead", B2, D1, "cv-" + B2, "cv-" + D1}


def schema_statements():
    with open(os.path.join(ROOT, "crates/am-base/src/db.rs"), encoding="utf-8") as f:
        src = f.read()
    body = re.search(r'pub const SCHEMA: &str = r#"(.*?)"#;', src, re.S).group(1)
    stmts = [s.strip() for s in body.split(";\n") if s.strip()]
    alters = re.findall(r'"(ALTER TABLE \w+ ADD COLUMN [^"]+)"', src)
    assert len(alters) > 20, "抽不到 db.rs 的 ALTER 名單，正則要跟著改"
    return stmts + alters + module_statements() + [
        "CREATE TABLE IF NOT EXISTS bot_reads (bot_id TEXT PRIMARY KEY, read_at TEXT NOT NULL, message_id TEXT NOT NULL DEFAULT '')",
        "PRAGMA user_version = 29",
    ]


# 協調者與群組任務的表（#720）：各模組自己的 `DDL` 常數＋migrate 裡的 ALTER／額外索引。
# 先建表、再補欄、最後建索引（有些索引用到 ALTER 才加的欄，例如 supervisor_inbox.role）。
MODULES = ["crates/am-supervisor/src/supervisor/store.rs", "crates/am-supervisor/src/supervisor/roles.rs", "crates/am-supervisor/src/mission/store.rs"]


def module_statements():
    tables, alters, indexes = [], [], []
    for rel in MODULES:
        with open(os.path.join(ROOT, rel), encoding="utf-8") as f:
            src = f.read()
        body = re.search(r'const DDL: &str = r#"(.*?)"#;', src, re.S).group(1)
        for st in (x.strip() for x in body.split(";\n") if x.strip()):
            (indexes if re.match(r"CREATE (UNIQUE )?INDEX", st) else tables).append(st.rstrip(";"))
        alters += re.findall(r'"(ALTER TABLE \w+ ADD COLUMN [^"]+)"', src)
        indexes += [re.sub(r"\s+", " ", x) for x in re.findall(r'"(CREATE (?:UNIQUE )?INDEX IF NOT EXISTS [^"]+)"', src)]
    assert len(tables) >= 12 and len(alters) >= 40, (len(tables), len(alters))
    return tables + alters + indexes


def make_db(path):
    conn = sqlite3.connect(path)
    for s in schema_statements():
        # 跟 db::apply_migrations 一樣：已經有這一欄就不加（SCHEMA 裡本來就有的欄位 ALTER 名單也列著）。
        m = re.match(r"ALTER TABLE (\w+) ADD COLUMN (\w+)", s)
        if m and m.group(2) in [r[1] for r in conn.execute(f"PRAGMA table_info({m.group(1)})")]:
            continue
        conn.execute(s)
    conn.commit()
    return conn


T0 = "2026-09-28T01:00:00.000Z"


def seed_source(conn, att_file):
    x = conn.execute
    x("INSERT INTO projects (id, path, label, host, workspace_id, created_at) VALUES (?,?,?,?,?,?)",
      (PID, "/Users/m4p/project/hub", "智選hub", "local", "w1", T0))
    x("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)",
      (OTHER, "/Users/m4p/project/other", "other", "local", T0))
    bot = ("INSERT INTO bots (id, project_id, name, kind, model, args_json, autostart, identity, env_json, managed_by,"
           " parent_bot_id, hook_token, deleted_at, created_at, position) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
    x(bot, (B1, PID, "hub-main", "claude", "claude-opus-5-5", json.dumps(["--x", 'y"q\\z']), 1, "cc1",
            json.dumps({"FOO": "1", "中文": "值"}), "user", None, "a" * 32, None, T0, 0))
    x(bot, (B2, PID, "hub-old", "codex", None, "[]", 0, None, "{}", "user", None, "b" * 32, T0, T0, 1))
    x(bot, (C1, PID, "hub-main-kid", "claude", None, "[]", 0, None, "{}", "child", B1, "c" * 32, None, T0, 0))
    x(bot, (D1, PID, "hub-main-kid2", "claude", None, "[]", 0, None, "{}", "child", B1, "e" * 32, T0, T0, 0))
    x(bot, (QB, OTHER, "other-bot", "claude", None, "[]", 0, None, "{}", "user", None, "d" * 32, None, T0, 0))
    for bid in (B1, B2, C1, D1, QB):
        x("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?)", ("cv-" + bid, bid, T0))
    run = ("INSERT INTO runs (id, bot_id, state, native_session_id, transcript_path, pane_id, started_at, ended_at)"
           " VALUES (?,?,?,?,?,?,?,?)")
    # 同一毫秒起的兩個 run：挑最後一個靠 rowid（issue #461），插入順序必須保住。
    x(run, ("r-z-old", B1, "exited", "sess-old", "/t/old.jsonl", "p1", T0, T0))
    x(run, ("r-a-new", B1, "running", "sess-new", "/t/new.jsonl", "p2", T0, None))
    x(run, ("r-kid", C1, "running", "sess-kid", None, "p3", T0, None))
    x(run, ("r-dead", B2, "exited", "sess-dead", None, "p4", T0, T0))
    x(run, ("r-dead-kid", D1, "exited", "sess-dead-kid", None, "p5", T0, T0))
    x(run, ("r-q", QB, "running", "sess-q", None, "p9", T0, None))
    turn = ("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, native_session_id, created_at, completed_at)"
            " VALUES (?,?,?,?,?,?,?,?,?)")
    x(turn, ("t-done", "cv-" + B1, "r-z-old", "web", "completed", "ok", "sess-old", T0, T0))
    x(turn, ("t-fly", "cv-" + B1, "r-a-new", "web", "in_flight", "ok", "sess-new", T0, None))
    x(turn, ("t-queue", "cv-" + B1, None, "web", "queued", "pending", None, T0, None))
    x(turn, ("t-dead", "cv-" + B2, "r-dead", "web", "completed", "ok", "sess-dead", T0, T0))
    x(turn, ("t-q", "cv-" + QB, "r-q", "web", "completed", "ok", None, T0, T0))
    msg = "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,?,?,?,?)"
    x(msg, ("m1", "cv-" + B1, "t-done", "user", "你好", "web", T0))
    x(msg, ("m2", "cv-" + B1, "t-done", "assistant", "hi", "hook", T0))
    x(msg, ("m3", "cv-" + C1, None, "assistant", "kid", "hook", T0))
    x(msg, ("m-dead", "cv-" + B2, "t-dead", "user", "deleted bot history", "web", T0))
    x(msg, ("m-dead-kid", "cv-" + D1, None, "assistant", "deleted kid", "hook", T0))
    x(msg, ("m-q", "cv-" + QB, "t-q", "user", "secret of other project", "web", T0))
    att = ("INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, message_id, state, created_at)"
           " VALUES (?,?,?,?,?,?,?,?,?,?,?)")
    x(att, ("a-ok", B1, "shot.png", "image/png", 4, att_file, att_file, "local", "m1", "ready", T0))
    x(att, ("a-gone", B1, "gone.png", "image/png", 4, "/nonexistent/gone.png", "/nonexistent/gone.png", "local", "m1", "ready", T0))
    x(att, ("a-dead", B2, "dead.png", "image/png", 4, att_file, att_file, "local", "m-dead", "ready", T0))
    x("INSERT INTO bot_reads (bot_id, read_at, message_id) VALUES (?,?,?)", (B1, T0, "m2"))
    x("INSERT INTO bot_reads (bot_id, read_at, message_id) VALUES (?,?,?)", (B2, T0, "m-dead"))
    mission = ("INSERT INTO missions (id, project_id, client_request_id, text, delivery_mode, executor_kind, on_5h_limit,"
               " created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?)")
    x(mission, ("ms-hub", PID, "crid-ms-hub", "修 bug", "auto", "codex", "wait", T0, T0))
    x(mission, ("ms-other", OTHER, "crid-ms-other", "別的專案", "auto", "codex", "wait", T0, T0))
    ev = "INSERT INTO mission_events (id, mission_id, kind, text, created_at) VALUES (?,?,?,?,?)"
    x(ev, ("me-hub", "ms-hub", "instruction", "開工", T0))
    x(ev, ("me-other", "ms-other", "instruction", "別的", T0))
    conn.commit()


def ago(days):
    t = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=days)
    return t.strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


AGM_CWD = "/Users/m4p/.config/agents-manager/supervisor/AGM"


def seed_supervisor(conn):
    """協調者（#720）：每一種挑選規則各一筆要搬、一筆不搬的。"""
    x = conn.execute
    now, old = ago(0), ago(30)
    x("INSERT INTO supervisors (id, bot_id, project_id, cwd, persona_text, persona_version, summary, summary_version, generation,"
      " status, desired_running, watchdog_attempts, remote_status, remote_url, created_at, updated_at)"
      " VALUES ('AGM',?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
      (B1, PID, AGM_CWD, "我是 AGM", 3, "交接摘要", 2, 9, "failed", 1, 4, "connected", "https://remote", T0, T0))
    role = "INSERT INTO supervisor_roles (role, bot_id, project_id, cwd, wakes, persona_text, desired_running, status, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?)"
    x(role, ("patrol", None, None, None, 12, None, 0, "", T0, T0))
    x(role, ("responder", C1, PID, AGM_CWD + "-responder", 7, "我是 responder", 1, "waiting_quota", T0, T0))
    x("INSERT INTO supervisor_requests (id, supervisor_id, text, created_at) VALUES ('rq1','AGM','幫我修',?)", (T0,))
    asg = ("INSERT INTO supervisor_assignments (id, supervisor_id, request_id, target_bot_id, client_request_id, text, status,"
           " created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?)")
    x(asg, ("as-done", "AGM", "rq1", QB, "agm-crid-done", "做完了", "completed", T0, T0))
    x(asg, ("as-open", "AGM", "rq1", QB, "agm-crid-open", "做到一半", "delivered", T0, T0))
    x(asg, ("as-crid", "AGM", "rq1", QB, "agm-crid-dup", "目標也有這個 crid", "completed", T0, T0))
    review = ("INSERT INTO supervisor_reviews (id, supervisor_id, assignment_id, decision, from_status, to_status, actor, created_at)"
              " VALUES (?,'AGM',?,'accept','awaiting_review','completed','AGM',?)")
    x(review, ("rv1", "as-done", T0))
    x(review, ("rv-dup", "as-crid", T0))
    note = "INSERT INTO supervisor_notes (id, supervisor_id, kind, body, created_at) VALUES (?,?,?,?,?)"
    x(note, ("n-handoff", "AGM", "handoff", "交接", T0))
    x(note, ("n-retire", B1, "child_retirement_hold", "{}", T0))
    inc = "INSERT INTO supervisor_incidents (id, supervisor_id, kind, resource, status, first_seen_at, last_seen_at) VALUES (?,?,?,?,?,?,?)"
    x(inc, ("ic-open", "AGM", "host_disconnected", "m4p", "open", T0, T0))
    x(inc, ("ic-done", "AGM", "host_disconnected", "m4p", "resolved", T0, T0))
    apv = "INSERT INTO supervisor_approvals (id, supervisor_id, requester, purpose, scope, status, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?)"
    for aid, st in (("ap-pending", "pending"), ("ap-approved", "approved"), ("ap-consumed", "consumed"), ("ap-denied", "denied")):
        x(apv, (aid, "AGM", "fixer", "rebuild", "daemon", st, T0, T0))
    x("INSERT INTO supervisor_leases (resource, owner, fence, lease_token) VALUES ('restart','fixer',3,'SECRET-TOKEN')")
    x("INSERT INTO bot_sleeps (bot_id, slept_at) VALUES (?,?)", (B1, T0))
    inbox = ("INSERT INTO supervisor_inbox (id, supervisor_id, event_key, kind, state, merged_into, notify_attempts, notify_next_at,"
             " created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?)")
    x(inbox, ("i-old-handled", "AGM", "k-old-handled", "bot_turn_done", "handled", None, 1, None, old, old))
    x(inbox, ("i-old-target", "AGM", "k-old-target", "bot_turn_done", "handled", None, 1, None, old, old))
    # 舊版寫的秒格式：跟毫秒格式比要先正規化，不然字串比較會把它當成「比 cutoff 新」。
    x(inbox, ("i-old-seconds", "AGM", "k-old-seconds", "bot_turn_done", "handled", None, 1, None, old[:19] + "Z", old))
    # SQLite `datetime()` 的空白格式、落在截止那一天但晚於截止時刻：直接比字串的話空白排在 `T` 前面，會被當成比較舊而漏掉。
    edge = (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=7, hours=-1)).strftime("%Y-%m-%d %H:%M:%S")
    x(inbox, ("i-edge-space", "AGM", "k-edge-space", "bot_turn_done", "handled", None, 1, None, edge, edge))
    x(inbox, ("i-old-pending", "AGM", "k-old-pending", "bot_turn_done", "pending", None, 2, now, old, old))
    x(inbox, ("i-new-handled", "AGM", "k-new-handled", "bot_turn_done", "handled", None, 1, None, now, now))
    x(inbox, ("i-new-delivered", "AGM", "k-new-delivered", "bot_turn_done", "delivered", None, 1, None, now, now))
    x(inbox, ("i-new-merged", "AGM", "k-new-merged", "bot_turn_done", "handled", "i-old-target", 1, None, now, now))
    x(inbox, ("i-collide", "AGM", "persona:3:changed", "persona_changed", "pending", None, 0, None, now, now))
    conn.commit()


TARGET_CONFIG = """[server]
listen = "127.0.0.1:7788"

[[hosts]]
name = "m4p"
ssh = "m4p"

[[projects]]
id = "01TARGETPROJ00000000000R01"
path = "/home/u/r"
label = "r"

[panes]
idle_close_secs = 600
"""


def run(*argv, env=None, check=True):
    p = subprocess.run([sys.executable, "-B", TOOL, *argv], capture_output=True, text=True, env=env)
    if check and p.returncode != 0:
        raise AssertionError(f"{argv} rc={p.returncode}\nstdout={p.stdout}\nstderr={p.stderr}")
    return p


def slurp(path, mode="rb"):
    with open(path, mode) as f:
        return f.read()


def put(path, data):
    with open(path, "wb" if isinstance(data, bytes) else "w") as f:
        f.write(data)


def digest(path):
    return hashlib.sha256(slurp(path)).hexdigest()


def counts(conn):
    return {t: conn.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0]
            for t in ("projects", "bots", "conversations", "runs", "turns", "messages", "attachments", "bot_reads")}


class ProjectTransferTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pt-test-")
        d = self.tmp.name
        self.snapdir = os.path.join(d, "snaptmp")
        os.mkdir(self.snapdir)
        self.env = {**os.environ, "TMPDIR": self.snapdir}
        self.att = os.path.join(d, "shot.png")
        put(self.att, b"\x89PNG")
        self.src_path = os.path.join(d, "src.sqlite3")
        src = make_db(self.src_path)
        seed_source(src, self.att)
        src.close()
        self.tgt_dir = os.path.join(d, "target")
        os.mkdir(self.tgt_dir)
        self.cfg = os.path.join(self.tgt_dir, "config.toml")
        put(self.cfg, TARGET_CONFIG)
        self.tgt_path = os.path.join(self.tgt_dir, "agents-manager.sqlite3")
        tgt = make_db(self.tgt_path)
        tgt.execute("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)",
                    ("01TARGETPROJ00000000000R01", "/home/u/r", "r", "local", T0))
        tgt.commit()
        tgt.close()
        self.bundle = os.path.join(d, "hub.json.gz")

    def tearDown(self):
        self.tmp.cleanup()

    def export(self):
        before = digest(self.src_path)
        run("export", "--db", self.src_path, "--project", PID, "--out", self.bundle, env=self.env)
        self.assertEqual(digest(self.src_path), before, "export 不能寫來源 DB")
        self.assertEqual(os.listdir(self.snapdir), [], "DB 快照要讀完當下刪")
        self.assertEqual(os.stat(self.bundle).st_mode & 0o777, 0o600)

    def imp(self, *extra, check=True):
        return run("import", "--bundle", self.bundle, "--host", "m4p", "--config", self.cfg, *extra, env=self.env, check=check)

    def tgt(self):
        c = sqlite3.connect(self.tgt_path)
        c.row_factory = sqlite3.Row
        return c

    def test_export_contains_only_the_project(self):
        self.export()
        b = json.loads(gzip.decompress(slurp(self.bundle)))
        ids = {t: [r.get("id", r.get("bot_id")) for r in spec["rows"]] for t, spec in b["tables"].items()}
        self.assertEqual(ids["projects"], [PID])
        self.assertEqual(sorted(ids["bots"]), sorted([B1, C1]), "只帶活著的 bot（含 live child）")
        for table, got in ids.items():
            self.assertEqual(DELETED_ROWS & set(got), set(), f"{table} 帶到了已刪 bot 的列")
        self.assertEqual(ids["runs"], ["r-z-old", "r-a-new", "r-kid"], "照 rowid 順序")
        self.assertNotIn("t-q", ids["turns"])
        self.assertNotIn("m-q", ids["messages"])
        self.assertEqual(list(b["files"]), ["a-ok"])
        self.assertEqual([m["id"] for m in b["missing_files"]], ["a-gone"])
        self.assertEqual({r["hook_token"] for r in b["tables"]["bots"]["rows"]}, {""}, "來源的 hook token 不能帶出去")

    def test_import_rewrites_and_keeps_foreign_keys(self):
        self.export()
        out = json.loads(self.imp().stdout)
        self.assertEqual(out["inserted"], {"projects": 1, "bots": 2, "conversations": 2, "runs": 3, "turns": 3,
                                           "messages": 3, "attachments": 2, "bot_reads": 1,
                                           "missions": 1, "mission_events": 1}, "群組任務跟著專案走，別的專案的不帶")
        self.assertIsNone(out["supervisor"], "沒加 --with-supervisor 就不碰協調者")
        self.assertEqual(out["autostart_turned_off"], ["hub-main"])
        self.assertEqual((out["runs_closed"], out["turns_failed"]), (2, 2))
        self.assertTrue(any("cc1" in w for w in out["warnings"]), out["warnings"])
        for b in out["backups"]:
            self.assertTrue(os.path.exists(b), b)
        c = self.tgt()
        c.execute("PRAGMA foreign_keys = ON")
        self.assertEqual(c.execute("PRAGMA foreign_key_check").fetchall(), [])
        p = c.execute("SELECT * FROM projects WHERE id = ?", (PID,)).fetchone()
        self.assertEqual((p["host"], p["path"], p["label"]), ("m4p", "/Users/m4p/project/hub", "智選hub"))
        src = sqlite3.connect(self.src_path)
        for bid in (B1, C1):
            old = src.execute("SELECT hook_token FROM bots WHERE id = ?", (bid,)).fetchone()[0]
            b = c.execute("SELECT * FROM bots WHERE id = ?", (bid,)).fetchone()
            self.assertNotEqual(b["hook_token"], old, "hook token 要重新產生")
            self.assertRegex(b["hook_token"], r"^[0-9a-f]{32}$")
            self.assertEqual(b["autostart"], 0)
        self.assertEqual(c.execute("SELECT parent_bot_id, managed_by FROM bots WHERE id = ?", (C1,)).fetchone()[:], (B1, "child"))
        runs = {r["id"]: r for r in c.execute("SELECT * FROM runs")}
        self.assertEqual({k: (r["state"], r["native_session_id"]) for k, r in runs.items()},
                         {"r-z-old": ("exited", "sess-old"), "r-a-new": ("exited", "sess-new"), "r-kid": ("exited", "sess-kid")})
        self.assertEqual(runs["r-a-new"]["exit_reason"], "project transfer")
        self.assertIsNotNone(runs["r-a-new"]["ended_at"])
        self.assertIsNone(runs["r-z-old"]["exit_reason"], "本來就結束的 run 不改")
        # 跟 db::last_native_session 同一句：接回要挑到最後那段。
        last = c.execute("SELECT native_session_id FROM runs WHERE bot_id = ? AND ended_at IS NOT NULL AND native_session_id IS NOT NULL"
                         " ORDER BY started_at DESC, rowid DESC LIMIT 1", (B1,)).fetchone()[0]
        self.assertEqual(last, "sess-new")
        turns = dict(c.execute("SELECT id, status FROM turns").fetchall())
        self.assertEqual(turns, {"t-done": "completed", "t-fly": "failed", "t-queue": "failed"})
        a = c.execute("SELECT * FROM attachments WHERE id = 'a-ok'").fetchone()
        self.assertEqual(a["host"], "m4p")
        self.assertEqual(a["agent_path"], self.att, "agent 讀的仍是來源機器上的路徑")
        self.assertEqual(a["local_path"], os.path.join(self.tgt_dir, "attachments", B1, "shot.png"))
        self.assertEqual(slurp(a["local_path"]), b"\x89PNG")
        self.assertEqual(c.execute("SELECT COUNT(*) FROM messages WHERE id = 'm-q'").fetchone()[0], 0)
        for table in ("bots", "conversations", "runs", "turns", "messages", "attachments"):
            got = {r[0] for r in c.execute(f"SELECT id FROM {table}")}
            self.assertEqual(DELETED_ROWS & got, set(), f"目標 {table} 有已刪 bot 的列")
        self.assertEqual([r[0] for r in c.execute("SELECT bot_id FROM bot_reads")], [B1])

        cfg = tomllib.loads(slurp(self.cfg, "r"))
        self.assertEqual(cfg["panes"], {"idle_close_secs": 600}, "既有的表不能被接到別處")
        projs = {p["id"]: p for p in cfg["projects"]}
        self.assertEqual(set(projs), {"01TARGETPROJ00000000000R01", PID})
        hub = projs[PID]
        self.assertEqual((hub["host"], hub["path"], hub["label"]), ("m4p", "/Users/m4p/project/hub", "智選hub"))
        self.assertEqual([b["id"] for b in hub["bots"]], [B1], "只寫 user bot；child 只在 DB")
        b = hub["bots"][0]
        self.assertEqual((b["name"], b["kind"], b["model"], b["identity"], b["autostart"]),
                         ("hub-main", "claude", "claude-opus-5-5", "cc1", False))
        self.assertEqual(b["args"], ["--x", 'y"q\\z'])
        self.assertEqual(b["env"], {"FOO": "1", "中文": "值"})

    def test_reimport_is_idempotent(self):
        self.export()
        self.imp()
        c = self.tgt()
        before, cfg_before = counts(c), slurp(self.cfg, "r")
        tokens = dict(c.execute("SELECT id, hook_token FROM bots").fetchall())
        c.close()
        out = json.loads(self.imp().stdout)
        self.assertEqual(set(out["inserted"].values()), {0})
        self.assertEqual(out["config"], "already present")
        c = self.tgt()
        self.assertEqual(counts(c), before)
        self.assertEqual(dict(c.execute("SELECT id, hook_token FROM bots").fetchall()), tokens, "重跑不換 token")
        self.assertEqual(slurp(self.cfg, "r"), cfg_before)

    def test_import_clears_a_stale_handoff_flag_on_an_existing_config_project(self):
        """Import resets projects.handed_off_to; a pre-existing config row must not restore the old owner on restart."""
        self.export()
        put(self.cfg, TARGET_CONFIG + (
            '\n[[projects]]\nid = "%s"\npath = "/Users/m4p/project/hub"\nlabel = "智選hub"\nhost = "m4p"\n'
            'handed_off_to = "previous-owner"\n\n[[projects.bots]]\nid = "%s"\nname = "hub-main"\nkind = "claude"\n'
        ) % (PID, B1))

        out = json.loads(self.imp().stdout)
        self.assertEqual(out["config"], "handoff cleared")
        c = self.tgt()
        self.assertIsNone(c.execute("SELECT handed_off_to FROM projects WHERE id = ?", (PID,)).fetchone()[0])
        imported = {p["id"]: p for p in tomllib.loads(slurp(self.cfg, "r"))["projects"]}[PID]
        self.assertNotIn("handed_off_to", imported, "config projection must not reapply the stale handoff")

    def test_dry_run_writes_nothing(self):
        self.export()
        db0, cfg0 = digest(self.tgt_path), digest(self.cfg)
        out = json.loads(self.imp("--dry-run").stdout)
        self.assertEqual(out["inserted"]["messages"], 3)
        self.assertEqual((digest(self.tgt_path), digest(self.cfg)), (db0, cfg0))
        self.assertFalse(os.path.exists(os.path.join(self.tgt_dir, "attachments")))
        self.assertEqual(sorted(os.listdir(self.tgt_dir)), ["agents-manager.sqlite3", "config.toml", "daemon.lock"])

    def test_refuses_while_daemon_holds_the_lock(self):
        self.export()
        fd = os.open(os.path.join(self.tgt_dir, "daemon.lock"), os.O_RDWR | os.O_CREAT)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        os.write(fd, b"4242")
        try:
            db0, cfg0 = digest(self.tgt_path), digest(self.cfg)
            p = self.imp(check=False)
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("daemon 還在跑", p.stderr)
            self.assertIn("4242", p.stderr)
            self.assertEqual((digest(self.tgt_path), digest(self.cfg)), (db0, cfg0))
        finally:
            os.close(fd)

    def test_refuses_unknown_host_and_path_collision(self):
        self.export()
        p = run("import", "--bundle", self.bundle, "--host", "nope", "--config", self.cfg, env=self.env, check=False)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("[[hosts]]", p.stderr)
        c = self.tgt()
        c.execute("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)",
                  ("01TARGETPROJ00000000000X01", "/Users/m4p/project/hub", "dup", "m4p", T0))
        c.commit()
        c.close()
        db0, cfg0 = digest(self.tgt_path), digest(self.cfg)
        p = self.imp(check=False)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("佔著", p.stderr)
        self.assertEqual((digest(self.tgt_path), digest(self.cfg)), (db0, cfg0))

    def test_local_host_needs_a_path_map(self):
        self.export()
        db0, cfg0 = digest(self.tgt_path), digest(self.cfg)
        p = run("import", "--bundle", self.bundle, "--host", "local", "--config", self.cfg, env=self.env, check=False)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("--path-map", p.stderr)
        p = run("import", "--bundle", self.bundle, "--host", "local", "--config", self.cfg,
                "--path-map", "/Users/m4p/project/other=/home/u/other", env=self.env, check=False)
        self.assertNotEqual(p.returncode, 0, "換不到這個專案的 path 一樣不行")
        self.assertEqual((digest(self.tgt_path), digest(self.cfg)), (db0, cfg0))

    def local_import(self, transcripts_here):
        """#717：專案改在目標本機跑。B1 在 worktree、C1 在兄弟目錄（不能被專案那條 map 吃到）。"""
        src = sqlite3.connect(self.src_path)
        src.execute("UPDATE bots SET cwd = '/Users/m4p/project/hub/.claude/worktrees/w1' WHERE id = ?", (B1,))
        src.execute("UPDATE bots SET cwd = '/Users/m4p/project/hub-sibling' WHERE id = ?", (C1,))
        if transcripts_here:
            for rid in ("r-z-old", "r-a-new", "r-kid"):
                f = os.path.join(self.tgt_dir, f"{rid}.jsonl")
                put(f, b"{}")
                src.execute("UPDATE runs SET transcript_path = ? WHERE id = ?", (f, rid))
        src.commit()
        src.close()
        self.export()
        return json.loads(run("import", "--bundle", self.bundle, "--host", "local", "--config", self.cfg,
                              "--path-map", "/Users/m4p/project/hub=/home/u/hub", env=self.env).stdout)

    def test_local_import_maps_project_paths(self):
        out = self.local_import(transcripts_here=True)
        self.assertEqual(out["host"], "local")
        self.assertFalse(any("transcript" in w for w in out["warnings"]), out["warnings"])
        c = self.tgt()
        p = c.execute("SELECT host, path FROM projects WHERE id = ?", (PID,)).fetchone()
        self.assertEqual(tuple(p), ("local", "/home/u/hub"))
        cwd = dict(c.execute("SELECT id, cwd FROM bots").fetchall())
        self.assertEqual(cwd[B1], "/home/u/hub/.claude/worktrees/w1", "worktree 跟著專案換")
        self.assertEqual(cwd[C1], "/Users/m4p/project/hub-sibling", "只換完整的路徑段")
        self.assertEqual(c.execute("SELECT agent_path FROM attachments WHERE id = 'a-ok'").fetchone()[0], self.att,
                         "不在 map 底下的不動")
        hub = {p["id"]: p for p in tomllib.loads(slurp(self.cfg, "r"))["projects"]}[PID]
        self.assertEqual((hub["host"], hub["path"]), ("local", "/home/u/hub"))

    def test_a_config_project_missing_the_bundles_bots_is_refused_not_half_imported(self):
        """目標 config 已經有這個專案（id 一樣）但沒有 bundle 裡的 user bot：只插 DB 的話，下次開機投影看到
        「bot 不在 config.toml」就把剛匯入的 bot 軟刪，而這支還印 `config: already present`。要嘛補進 config，
        做不到就整批拒絕——DB 一個字不動。"""
        self.export()
        put(self.cfg, slurp(self.cfg, "r") + (
            '\n[[projects]]\nid = "%s"\npath = "/Users/m4p/project/hub"\nlabel = "智選hub"\nhost = "m4p"\n' % PID))
        db0, cfg0 = digest(self.tgt_path), digest(self.cfg)
        p = self.imp(check=False)
        self.assertNotEqual(p.returncode, 0, p.stdout)
        self.assertIn("hub-main", p.stderr)
        self.assertEqual((digest(self.tgt_path), digest(self.cfg)), (db0, cfg0), "DB 與 config 都不能動")

    def test_workspace_id_follows_the_host_it_was_made_on(self):
        """herdr 的 workspace id 是各台機器自己的短 id（`w1`、`wV`…）：帶到另一台 herdr 上可能剛好是**別的**
        workspace，daemon 的 `workspace_get` 看到「存在」就把這個專案的 bot 開在不相干的 workspace 裡。
        專案改在目標本機跑（`--host local`）就要清掉；`--host m4p`（來源機器仍是那台）原樣保留。"""
        self.local_import(transcripts_here=True)
        c = self.tgt()
        self.assertIsNone(c.execute("SELECT workspace_id FROM projects WHERE id = ?", (PID,)).fetchone()[0],
                          "來源機器的 workspace id 在目標本機的 herdr 上沒有意義")

    def test_remote_import_keeps_the_workspace_id(self):
        self.export()
        self.imp()
        c = self.tgt()
        self.assertEqual(c.execute("SELECT workspace_id FROM projects WHERE id = ?", (PID,)).fetchone()[0], "w1")

    def test_local_import_warns_when_transcripts_were_not_moved(self):
        out = self.local_import(transcripts_here=False)
        w = [x for x in out["warnings"] if "transcript" in x]
        self.assertEqual(len(w), 1, out["warnings"])
        self.assertIn("2 段", w[0])  # r-kid 沒記 transcript_path：沒得看
        self.assertIn("transcript-transfer", w[0])

    # ------------------------------------------------ #720 協調者

    def sup_export(self):
        src = sqlite3.connect(self.src_path)
        seed_supervisor(src)
        src.close()
        before = digest(self.src_path)
        run("export", "--db", self.src_path, "--project", PID, "--out", self.bundle, "--with-supervisor", env=self.env)
        self.assertEqual(digest(self.src_path), before, "export 不能寫來源 DB")
        return json.loads(gzip.decompress(slurp(self.bundle)))

    def sup_target(self):
        """目標開機過：有自己的 AGM 那一列與 responder，外加會撞唯一索引的 inbox、交辦與群組任務。"""
        c = self.tgt()
        c.execute("INSERT INTO supervisors (id, generation, status, created_at, updated_at) VALUES ('AGM', 5, 'failed', ?, ?)", (T0, T0))
        c.execute("INSERT INTO supervisor_roles (role, bot_id, wakes, created_at, updated_at) VALUES ('responder','old-bot',1,?,?)", (T0, T0))
        c.execute("INSERT INTO supervisor_inbox (id, supervisor_id, event_key, kind, state, created_at, updated_at)"
                  " VALUES ('t-inbox','AGM','persona:3:changed','persona_changed','pending',?,?)", (T0, T0))
        c.execute("INSERT INTO supervisor_assignments (id, supervisor_id, target_bot_id, client_request_id, text, status, created_at, updated_at)"
                  " VALUES ('t-asg','AGM','x','agm-crid-dup','target own','completed',?,?)", (T0, T0))
        c.execute("INSERT INTO supervisor_leases (resource, owner, fence) VALUES ('restart', 'target-owner', 1)")
        c.commit()
        c.close()

    def sup_import(self, *extra, check=True):
        return run("import", "--bundle", self.bundle, "--host", "local", "--config", self.cfg, "--with-supervisor",
                   "--path-map", "/Users/m4p=/home/u", *extra, env=self.env, check=check)

    def test_export_with_supervisor_picks_only_what_moves(self):
        b = self.sup_export()["supervisor"]
        ids = {t: {r.get("id") or r.get("role") for r in spec["rows"]} for t, spec in b["tables"].items()}
        self.assertNotIn("supervisor_leases", ids, "租約是這顆 daemon 自己的，不搬")
        self.assertNotIn("bot_sleeps", ids)
        self.assertEqual(ids["supervisors"], {"AGM"})
        self.assertEqual(ids["supervisor_roles"], {"patrol", "responder"})
        self.assertEqual(ids["supervisor_assignments"], {"as-done", "as-open", "as-crid"}, "未結案的也照原狀搬")
        self.assertEqual(ids["supervisor_notes"], {"n-handoff"}, "child_retirement_* 是 10 分鐘的執行期守衛")
        self.assertEqual(ids["supervisor_incidents"], {"ic-done"}, "開著的 incident 由目標重新判定")
        self.assertEqual(ids["supervisor_approvals"], {"ap-consumed", "ap-denied"}, "還有效的核准不搬")
        self.assertEqual(ids["supervisor_inbox"],
                         {"i-old-pending", "i-new-handled", "i-new-delivered", "i-new-merged", "i-old-target", "i-collide",
                          "i-edge-space"},
                         "未處理的＋最近 7 天的，外加 merged_into 指到的舊事件；秒格式的舊事件照樣算舊")

    def test_import_with_supervisor_merges_into_the_targets_agm(self):
        self.sup_export()
        self.sup_target()
        out = json.loads(self.sup_import().stdout)
        sup = out["supervisor"]
        c = self.tgt()
        agm = c.execute("SELECT * FROM supervisors WHERE id='AGM'").fetchone()
        self.assertEqual((agm["bot_id"], agm["project_id"], agm["cwd"]), (B1, PID, "/home/u/.config/agents-manager/supervisor/AGM"))
        self.assertEqual((agm["persona_text"], agm["persona_version"], agm["summary"], agm["summary_version"]), ("我是 AGM", 3, "交接摘要", 2))
        self.assertEqual(agm["generation"], 5, "generation 是目標 controller 的守衛，不能被來源的蓋掉")
        self.assertEqual((agm["status"], agm["desired_running"], agm["watchdog_attempts"], agm["remote_status"], agm["remote_url"]),
                         ("", 0, 0, "unknown", None), "執行期欄位重設：接手後才打開")
        resp = c.execute("SELECT * FROM supervisor_roles WHERE role='responder'").fetchone()
        self.assertEqual((resp["bot_id"], resp["cwd"], resp["wakes"], resp["status"], resp["desired_running"]),
                         (C1, "/home/u/.config/agents-manager/supervisor/AGM-responder", 7, "", 0))
        self.assertEqual(sup["singletons"], {"supervisors:AGM": "updated", "supervisor_roles:patrol": "inserted",
                                             "supervisor_roles:responder": "updated"})
        inbox = {r["id"]: r for r in c.execute("SELECT * FROM supervisor_inbox")}
        self.assertEqual(inbox["i-old-pending"]["state"], "gave_up", "未處理的不再通知：停在 gave_up")
        self.assertEqual(inbox["i-new-delivered"]["state"], "gave_up")
        self.assertIsNone(inbox["i-old-pending"]["notify_next_at"])
        self.assertIn("#720", inbox["i-old-pending"]["notify_error"])
        self.assertEqual(inbox["i-new-handled"]["state"], "handled", "處理過的照原樣")
        self.assertNotIn("i-collide", inbox, "event_key 撞到目標既有的：跳過")
        self.assertEqual(sup["inbox_parked"], 2)
        self.assertEqual({(x["table"], x["id"]) for x in sup["skipped_unique"]},
                         {("supervisor_inbox", "i-collide"), ("supervisor_assignments", "as-crid"),
                          ("supervisor_reviews", "rv-dup")}, "被跳過的交辦，它的驗收也跟著跳過")
        self.assertEqual([r[0] for r in c.execute("SELECT id FROM supervisor_reviews")], ["rv1"])
        asg = dict(c.execute("SELECT id, status FROM supervisor_assignments").fetchall())
        self.assertEqual(asg, {"t-asg": "completed", "as-done": "completed", "as-open": "delivered"})
        self.assertEqual(c.execute("SELECT owner, fence FROM supervisor_leases").fetchall()[0][:], ("target-owner", 1), "目標的租約不動")
        self.assertEqual(c.execute("SELECT COUNT(*) FROM bot_sleeps").fetchone()[0], 0)
        self.assertEqual(set(r[0] for r in c.execute("SELECT id FROM supervisor_approvals")), {"ap-consumed", "ap-denied"})
        note = c.execute("SELECT * FROM supervisor_notes WHERE kind='transfer'").fetchall()
        self.assertEqual(len(note), 1)
        body = json.loads(note[0]["body"])
        self.assertEqual(note[0]["id"], sup["transfer_note"])
        self.assertEqual(set(body["imported"]["supervisor_inbox"]),
                         {"i-old-pending", "i-new-handled", "i-new-delivered", "i-new-merged", "i-old-target", "i-edge-space"})
        self.assertEqual(body["skipped_unique"], sup["skipped_unique"])
        self.assertRegex(note[0]["id"], r"^[0-9A-HJKMNP-TV-Z]{26}$", "ULID，跟 daemon 的 id 同一種")

    def test_reimport_with_supervisor_changes_nothing(self):
        self.sup_export()
        self.sup_target()
        self.sup_import()
        c = self.tgt()
        before = {t: c.execute(f"SELECT * FROM {t} ORDER BY rowid").fetchall() for t in
                  ("supervisors", "supervisor_roles", "supervisor_inbox", "supervisor_notes", "supervisor_assignments")}
        c.close()
        sup = json.loads(self.sup_import().stdout)["supervisor"]
        self.assertEqual(set(sup["singletons"].values()), {"unchanged"})
        self.assertEqual(sum(sup["inserted"].values()), 0)
        self.assertIsNone(sup["transfer_note"], "什麼都沒搬就不再寫一筆 transfer")
        c = self.tgt()
        for t, rows_ in before.items():
            self.assertEqual([tuple(r) for r in c.execute(f"SELECT * FROM {t} ORDER BY rowid")], [tuple(r) for r in rows_], t)

    def test_supervisor_import_needs_it_in_the_bundle_and_dry_run_writes_nothing(self):
        self.export()
        p = self.sup_import(check=False)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("--with-supervisor", p.stderr)
        self.sup_export()
        self.sup_target()
        db0, cfg0 = digest(self.tgt_path), digest(self.cfg)
        out = json.loads(self.sup_import("--dry-run").stdout)
        self.assertTrue(out["dry_run"])
        self.assertEqual(out["supervisor"]["inbox_parked"], 2)
        self.assertEqual((digest(self.tgt_path), digest(self.cfg)), (db0, cfg0))

    def test_a_mission_crid_collision_is_skipped_and_reported(self):
        self.export()
        c = self.tgt()
        c.execute("INSERT INTO missions (id, project_id, client_request_id, text, delivery_mode, executor_kind, on_5h_limit,"
                  " created_at, updated_at) VALUES ('t-ms', ?, 'crid-ms-hub', 'x', 'auto', 'codex', 'wait', ?, ?)", (PID, T0, T0))
        c.commit()
        c.close()
        out = json.loads(self.imp().stdout)
        self.assertEqual((out["inserted"]["missions"], out["inserted"]["mission_events"]), (0, 0))
        self.assertEqual([(x["table"], x["id"]) for x in out["skipped_unique"]],
                         [("missions", "ms-hub"), ("mission_events", "me-hub")], "任務被跳過，它的事件也不留孤兒")
        self.assertEqual(out["inserted"]["projects"], 1, "撞到的只跳過那一筆，專案照樣搬")

    def test_failed_insert_rolls_back_db_and_config(self):
        self.export()
        # 目標已有同一個 native turn（UNIQUE turns_native）：插到一半失敗，前面插過的也要回滾。
        c = self.tgt()
        c.execute("INSERT INTO projects (id, path, label, host, created_at) VALUES ('px','/x','x','local',?)", (T0,))
        c.execute("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('bx','px','x','claude','t',?)", (T0,))
        c.execute("INSERT INTO conversations (id, bot_id, created_at) VALUES ('cx','bx',?)", (T0,))
        c.execute("INSERT INTO turns (id, conversation_id, origin, status, native_session_id, native_turn_id, created_at)"
                  " VALUES ('tx','cx','external','completed','sess-old','nt1',?)", (T0,))
        c.commit()
        c.close()
        s = sqlite3.connect(self.src_path)
        s.execute("UPDATE turns SET native_turn_id = 'nt1' WHERE id = 't-done'")
        s.commit()
        s.close()
        self.export()
        c = self.tgt()
        before = counts(c)
        c.close()
        cfg0 = digest(self.cfg)
        p = self.imp(check=False)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("t-done", p.stderr)
        self.assertIn("已回滾", p.stderr)
        c = self.tgt()
        self.assertEqual(counts(c), before)
        self.assertEqual(digest(self.cfg), cfg0)

    def test_config_with_daemon_empty_projects_array(self):
        # daemon 在專案清單為空時寫的是頂層 `projects = []`；直接 append `[[projects]]` 是 duplicate key（智選hub 實測）。
        put(self.cfg, "projects = []\n\n[server]\nlisten = \"127.0.0.1:7788\"\n\n[[hosts]]\nname = \"m4p\"\nssh = \"m4p\"\n\n[panes]\nidle_close_secs = 600\n")
        self.export()
        cfg0 = digest(self.cfg)
        self.assertEqual(json.loads(self.imp("--dry-run").stdout)["config"], "would append")
        self.assertEqual(digest(self.cfg), cfg0)
        self.imp()
        text = slurp(self.cfg, "r")
        cfg = tomllib.loads(text)
        self.assertEqual([p["id"] for p in cfg["projects"]], [PID])
        self.assertNotIn("projects = []", text)
        self.assertEqual((cfg["server"], cfg["panes"], [h["name"] for h in cfg["hosts"]]),
                         ({"listen": "127.0.0.1:7788"}, {"idle_close_secs": 600}, ["m4p"]))
        self.assertEqual(json.loads(self.imp().stdout)["config"], "already present")
        self.assertEqual(slurp(self.cfg, "r"), text)

    def test_refuses_config_whose_projects_cannot_take_an_append(self):
        hosts = '\n[[hosts]]\nname = "m4p"\nssh = "m4p"\n'
        cases = {
            # inline 陣列：append `[[projects]]` 直接 duplicate key，解析不了。
            "inline": 'projects = [{ id = "01TARGETPROJ00000000000R01", path = "/home/u/r", label = "r" }]\n' + hosts,
            # 解析得過但內容變了：多行字串裡剛好有一行 `projects = []`，刪掉它會改到別的值。
            "in-string": 'note = """\nprojects = []\n"""\n' + hosts,
        }
        self.export()
        c = self.tgt()
        before = counts(c)
        c.close()
        for case, text in cases.items():
            put(self.cfg, text)
            cfg0 = digest(self.cfg)
            for extra in (("--dry-run",), ()):
                p = self.imp(*extra, check=False)
                self.assertNotEqual(p.returncode, 0, (case, extra))
                self.assertIn("手動處理", p.stderr, case)
                self.assertIn("已回滾", p.stderr, case)
            self.assertEqual(digest(self.cfg), cfg0, case)
        c = self.tgt()
        self.assertEqual(counts(c), before)
        self.assertEqual([f for f in os.listdir(self.tgt_dir) if "pre-transfer" in f and "config" in f], [])

    def add_legacy_team_columns(self, team_id):
        # 已移除的 Team 功能留在舊 DB 的欄位（db.rs：`teams`／`team_*` may exist; nothing reads them）。
        s = sqlite3.connect(self.src_path)
        s.execute("ALTER TABLE bots ADD COLUMN team_id TEXT")
        s.execute("ALTER TABLE bots ADD COLUMN team_role TEXT")
        s.execute("UPDATE bots SET team_id = ?", (team_id,))
        s.commit()
        s.close()

    def test_skips_source_only_columns_that_are_all_null(self):
        self.add_legacy_team_columns(None)
        self.export()
        out = json.loads(self.imp().stdout)
        self.assertEqual(out["inserted"]["bots"], 2)
        self.assertTrue(any("team_id" in w and "team_role" in w for w in out["warnings"]), out["warnings"])
        c = self.tgt()
        self.assertNotIn("team_id", [r[1] for r in c.execute("PRAGMA table_info(bots)")])

    def test_refuses_source_only_columns_with_values(self):
        self.add_legacy_team_columns("team-1")
        self.export()
        db0, cfg0 = digest(self.tgt_path), digest(self.cfg)
        p = self.imp(check=False)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("team_id", p.stderr)
        self.assertNotIn("team_role", p.stderr, "只點名真的有值的欄")
        self.assertEqual((digest(self.tgt_path), digest(self.cfg)), (db0, cfg0))

    def test_refuses_target_missing_columns(self):
        s = sqlite3.connect(self.src_path)
        s.execute("UPDATE messages SET sent_via = 'send_now' WHERE id = 'm1'")
        s.commit()
        s.close()
        self.export()
        c = self.tgt()
        c.execute("ALTER TABLE messages DROP COLUMN sent_via")
        c.commit()
        c.close()
        p = self.imp(check=False)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("sent_via", p.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
