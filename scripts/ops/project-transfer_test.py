#!/usr/bin/env python3
"""scripts/ops/project-transfer 的隔離測試（issue #710）：假 DB、假 config、暫存目錄，不碰正式資料。

schema 直接從 daemon/src/db.rs 的 `SCHEMA` 與 additive ALTER 名單抽出來建，daemon 加欄時這裡跟著變。
"""

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
    with open(os.path.join(ROOT, "daemon/src/db.rs"), encoding="utf-8") as f:
        src = f.read()
    body = re.search(r'pub const SCHEMA: &str = r#"(.*?)"#;', src, re.S).group(1)
    stmts = [s.strip() for s in body.split(";\n") if s.strip()]
    alters = re.findall(r'"(ALTER TABLE \w+ ADD COLUMN [^"]+)"', src)
    assert len(alters) > 20, "抽不到 db.rs 的 ALTER 名單，正則要跟著改"
    return stmts + alters + [
        "CREATE TABLE IF NOT EXISTS bot_reads (bot_id TEXT PRIMARY KEY, read_at TEXT NOT NULL, message_id TEXT NOT NULL DEFAULT '')",
        "PRAGMA user_version = 29",
    ]


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
                                           "messages": 3, "attachments": 2, "bot_reads": 1})
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
