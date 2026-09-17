"""OB durable queue. One SQLite transaction owns each state transition."""
import contextlib
import fcntl
import json
import os
import re
import sqlite3
import time
import uuid
from pathlib import Path


class OBError(Exception):
    pass


def project_id(value):
    if not re.fullmatch(r"[0-9A-HJKMNP-TV-Z]{26}", value or ""):
        raise OBError("project_id 必須是 AG Man project ID，不接受名稱或 worktree 名")
    return value


def conversation_url(value):
    if not re.fullmatch(r"https://chatgpt\.com/c/[a-zA-Z0-9-]+", value or ""):
        raise OBError("需要 https://chatgpt.com/c/<conversation-id>")
    return value


@contextlib.contextmanager
def exclusive(path, wait=0):
    # Never unlink lock files: an old fd and a new inode could both hold the lock.
    # wait: seconds to retry, so a short status/recover probe cannot turn a fresh kick away.
    with open(path, "a") as fd:
        deadline = time.monotonic() + wait
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise OBError("busy")
                time.sleep(0.05)
        try:
            yield fd
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)


class Store:
    CLAIMABLE = """SELECT * FROM requests r WHERE status IN ('pending','waiting_quota')
        AND NOT EXISTS (SELECT 1 FROM requests u WHERE u.project_id=r.project_id AND u.status IN ('unknown','running'))"""

    def __init__(self, root):
        self.root = Path(root).expanduser().resolve()
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.db = sqlite3.connect(self.root / "ob.sqlite3", timeout=30, isolation_level=None)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA busy_timeout=30000")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY, label TEXT NOT NULL, url TEXT UNIQUE
            );
            CREATE TABLE IF NOT EXISTS requests (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
                request_id TEXT NOT NULL, question TEXT NOT NULL, source_bot_id TEXT,
                status TEXT NOT NULL DEFAULT 'pending', answer TEXT, url TEXT,
                error TEXT, created_at REAL NOT NULL, updated_at REAL NOT NULL,
                UNIQUE(project_id, request_id)
            );
            CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            -- 一個 project 一串對話，但那一串不能無限長：太長之後 GPT 每次都要吃完整段舊脈絡，
            -- 又慢又容易被不相干的題目帶偏（使用者 2026-09-16）。所以「現在這一串」會輪替，
            -- 舊的留在這張表裡查得到（`projects.url` 永遠是**目前**那一串）。
            CREATE TABLE IF NOT EXISTS conversations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id TEXT NOT NULL REFERENCES projects(id),
                url TEXT UNIQUE,
                started_at REAL NOT NULL,
                turns INTEGER NOT NULL DEFAULT 0,
                retired_at REAL, retired_why TEXT
            );
            CREATE INDEX IF NOT EXISTS conversations_live ON conversations(project_id, retired_at);
        """)
        # Binds sends and results to the claim that started them; an orphan of an old claim cannot act for a new one.
        # Check and ALTER in one write transaction: concurrent first runs of a new CLI must not both add it.
        with self.transaction():
            cols = {r[1] for r in self.db.execute("PRAGMA table_info(requests)")}
            if "claim_token" not in cols:
                self.db.execute("ALTER TABLE requests ADD COLUMN claim_token TEXT")
            # 這一單是不是「要開新的一串」。存在列上而不是記在記憶體：`remember_url`／`finish` 要靠它
            # 分辨「輪替換串」與「別的分頁冒充這個 project」——後者仍然一律拒絕。
            if "rotate" not in cols:
                self.db.execute("ALTER TABLE requests ADD COLUMN rotate INTEGER NOT NULL DEFAULT 0")
            # 升級：既有 project 的那一串補一列，turns 用已完成的單數回填，才不會一升級就立刻輪替。
            for row in self.db.execute("SELECT id, url FROM projects WHERE url IS NOT NULL"):
                if self.db.execute("SELECT 1 FROM conversations WHERE url=?", (row[1],)).fetchone():
                    continue
                stats = self.db.execute(
                    "SELECT COUNT(*), MIN(created_at) FROM requests WHERE project_id=? AND status='done'", (row[0],)
                ).fetchone()
                self.db.execute(
                    "INSERT INTO conversations(project_id,url,started_at,turns) VALUES (?,?,?,?)",
                    (row[0], row[1], stats[1] or time.time(), stats[0] or 0),
                )

    @contextlib.contextmanager
    def transaction(self):
        self.db.execute("BEGIN IMMEDIATE")
        try:
            yield
            self.db.execute("COMMIT")
        except BaseException:
            self.db.execute("ROLLBACK")
            raise

    def setting(self, key, default=None):
        row = self.db.execute("SELECT value FROM settings WHERE key=?", (key,)).fetchone()
        return json.loads(row[0]) if row else default

    def set_setting(self, key, value):
        self.db.execute("INSERT OR REPLACE INTO settings VALUES (?,?)", (key, json.dumps(value)))

    def get(self, ident):
        row = self.db.execute("SELECT * FROM requests WHERE id=?", (ident,)).fetchone()
        if row is None:
            raise OBError("request_not_found")
        return dict(row)

    #: 一串問滿這麼多題就換下一串。20 題的對話已經夠長到讓 GPT 開始「記得太多」。
    ROTATE_TURNS = int(os.environ.get("OB_ROTATE_TURNS") or 20)
    #: 或者放了這麼多天——放久的那一串裡的背景多半已經不是現況了。
    ROTATE_DAYS = float(os.environ.get("OB_ROTATE_DAYS") or 14)

    def current_conversation(self, pid):
        row = self.db.execute(
            "SELECT * FROM conversations WHERE project_id=? AND retired_at IS NULL ORDER BY id DESC LIMIT 1", (pid,)
        ).fetchone()
        return dict(row) if row else None

    def conversations(self, pid):
        return [dict(r) for r in self.db.execute("SELECT * FROM conversations WHERE project_id=? ORDER BY id", (pid,))]

    def rotate_due(self, pid, now=None):
        """該不該換下一串：(要不要, 為什麼)。還沒有任何一串時不算輪替——那是第一次開。"""
        live = self.current_conversation(pid)
        if not live or not live["url"]:
            return False, None
        if self.ROTATE_TURNS > 0 and live["turns"] >= self.ROTATE_TURNS:
            return True, f"turns>={self.ROTATE_TURNS}"
        age_days = ((now or time.time()) - live["started_at"]) / 86400
        if self.ROTATE_DAYS > 0 and age_days >= self.ROTATE_DAYS:
            return True, f"age>={self.ROTATE_DAYS}d"
        return False, None

    def request_rotation(self, pid):
        """人說「這串夠了」：把目前那一串的 turns 頂到門檻，下一題派送時就會換。
        現在就開一串空的沒有意義——那會留下一個沒人問過問題的對話。"""
        project_id(pid)
        with self.transaction():
            live = self.current_conversation(pid)
            if not live or not live["url"]:
                raise OBError("no_conversation_yet：這個 project 還沒有對話，下一題就是新的一串")
            self.db.execute("UPDATE conversations SET turns=? WHERE id=?", (max(live["turns"], self.ROTATE_TURNS), live["id"]))
        return {"project_id": pid, "current": live["url"], "rotate_next": True}

    def mark_rotate(self, ident, on=True):
        """派送前記下這一單要換新串；沒有這個旗標的單一律不准換對話。"""
        with self.transaction():
            job = self.get(ident)
            if job["status"] not in ("running", "pending"):
                raise OBError("request_not_running")
            self.db.execute("UPDATE requests SET rotate=? WHERE id=?", (1 if on else 0, ident))

    def _bind_url(self, job, url, now=None):
        """把 url 記成這個 project 目前那一串。換串只在這一單有 `rotate` 旗標時才准。"""
        now = now or time.time()
        pid = job["project_id"]
        live = self.current_conversation(pid)
        if live and live["url"] == url:
            return
        if live and live["url"]:
            if not job["rotate"]:
                raise OBError("conversation_changed")
            self.db.execute("UPDATE conversations SET retired_at=?, retired_why=? WHERE id=?", (now, "rotated", live["id"]))
        owner = self.db.execute("SELECT project_id FROM conversations WHERE url=?", (url,)).fetchone()
        if owner and owner[0] != pid:
            raise OBError("conversation_already_owned：其他 project 已使用此對話")
        if owner:
            self.db.execute("UPDATE conversations SET retired_at=NULL, retired_why=NULL WHERE url=?", (url,))
        else:
            self.db.execute("INSERT INTO conversations(project_id,url,started_at) VALUES (?,?,?)", (pid, url, now))
        self.db.execute("UPDATE projects SET url=? WHERE id=?", (url, pid))

    def project(self, pid):
        row = self.db.execute("SELECT * FROM projects WHERE id=?", (pid,)).fetchone()
        return dict(row) if row else None

    def submit(self, pid, label, rid, question, source=None):
        project_id(pid)
        if not rid or len(rid) > 200 or not question.strip():
            raise OBError("需要非空 request_id 與問題")
        with self.transaction():
            old = self.db.execute("SELECT * FROM requests WHERE project_id=? AND request_id=?", (pid, rid)).fetchone()
            if old:
                if old["question"] != question or old["source_bot_id"] != source:
                    raise OBError("request_mismatch：相同 request_id 不能換問題或寄件者")
                return dict(old)
            self.db.execute("INSERT INTO projects(id,label) VALUES (?,?) ON CONFLICT(id) DO UPDATE SET label=excluded.label", (pid, label))
            ident, now = uuid.uuid4().hex, time.time()
            self.db.execute("INSERT INTO requests(id,project_id,request_id,question,source_bot_id,created_at,updated_at) VALUES (?,?,?,?,?,?,?)",
                            (ident, pid, rid, question, source, now, now))
            return self.get(ident)

    def link(self, pid, label, url):
        project_id(pid)
        conversation_url(url)
        with self.transaction():
            old = self.project(pid)
            if old and old["url"] and old["url"] != url:
                raise OBError("project_already_linked：不覆寫既有對話")
            if self.db.execute("SELECT 1 FROM requests WHERE project_id=? AND status IN ('running','unknown')", (pid,)).fetchone():
                raise OBError("project_busy_or_unknown")
            try:
                self.db.execute("INSERT INTO projects VALUES (?,?,?) ON CONFLICT(id) DO UPDATE SET label=excluded.label,url=excluded.url", (pid, label, url))
                # 手動接回來的那串也要進輪替計時，否則它永遠不會換。
                if not self.db.execute("SELECT 1 FROM conversations WHERE url=?", (url,)).fetchone():
                    self.db.execute("INSERT INTO conversations(project_id,url,started_at) VALUES (?,?,?)", (pid, url, time.time()))
            except sqlite3.IntegrityError:
                raise OBError("conversation_already_owned：其他 project 已使用此對話")

    def recover(self):
        # Only under worker.lock. A crashed operation may already have sent to GPT.
        with self.transaction():
            ids = [r[0] for r in self.db.execute("SELECT id FROM requests WHERE status='running'")]
            self.db.execute("UPDATE requests SET status='unknown',claim_token=NULL,error='worker_interrupted; inspect original conversation',updated_at=? WHERE status='running'", (time.time(),))
        return ids

    def claim(self, token=None):
        with self.transaction():
            if self.setting("retry_after", 0) > time.time():
                return None
            row = self.db.execute(self.CLAIMABLE + " ORDER BY created_at,id LIMIT 1").fetchone()
            if not row:
                return None
            self.db.execute("UPDATE requests SET status='running',claim_token=?,error=NULL,updated_at=? WHERE id=?", (token or uuid.uuid4().hex, time.time(), row["id"]))
            return self.get(row["id"])

    def finish(self, ident, answer, url, token=None):
        conversation_url(url)
        if not answer.strip():
            raise OBError("empty_answer")
        with self.transaction():
            job = self.get(ident)
            if job["status"] == "done":
                return job
            if job["status"] not in ("running", "unknown"):
                raise OBError("request_not_running")
            # running belongs to its claim; only unknown may be finished without one (manual collect).
            if job["status"] == "running" and (not token or job["claim_token"] != token):
                raise OBError("claim_lost：原單已被重新 claim，不覆寫")
            self._bind_url(job, url)
            self.db.execute("UPDATE conversations SET turns=turns+1 WHERE url=?", (url,))
            self.db.execute("UPDATE requests SET status='done',answer=?,url=?,error=NULL,updated_at=? WHERE id=?", (answer, url, time.time(), ident))
            return self.get(ident)

    def remember_url(self, ident, url):
        conversation_url(url)
        with self.transaction():
            job = self.get(ident)
            self._bind_url(job, url)
            self.db.execute("UPDATE requests SET url=? WHERE id=?", (url, ident))

    def fail(self, ident, status, error, token=None):
        if status not in ("failed", "unknown", "waiting_quota"):
            raise OBError("invalid_failure")
        with self.transaction():
            changed = self.db.execute("UPDATE requests SET status=?,error=?,updated_at=? WHERE id=? AND status='running' AND claim_token=?",
                                      (status, error, time.time(), ident, token)).rowcount
            if status == "waiting_quota" and changed:
                self.set_setting("retry_after", time.time() + 1800)

    def requeue(self, ident, error, token=None):
        """暫時性狀況（瀏覽器被別人佔住）：放回佇列等下一輪，不算失敗——`failed` 會叫人去查登入，
        而這筆根本沒送出去（review3 c4 L6）。"""
        with self.transaction():
            self.db.execute("UPDATE requests SET status='pending',error=?,updated_at=? WHERE id=? AND status='running' AND claim_token=?",
                            (error, time.time(), ident, token))

    def retry(self, ident):
        with self.transaction():
            job = self.get(ident)
            if job["status"] not in ("waiting_quota", "failed"):
                raise OBError("只能重試 waiting_quota／failed；unknown 必須先對帳，不可重送")
            self.db.execute("UPDATE requests SET status='pending',error=NULL,updated_at=? WHERE id=?", (time.time(), ident))
            self.set_setting("retry_after", 0)
        return self.get(ident)

    def claimable(self):
        # Same eligibility as claim(), ignoring retry_after (a kicked worker just waits it out).
        return self.db.execute(self.CLAIMABLE + " LIMIT 1").fetchone() is not None

    def list(self, pid=None):
        sql = "SELECT id,project_id,request_id,status,url,error,created_at,updated_at FROM requests"
        return [dict(r) for r in self.db.execute(sql + (" WHERE project_id=?" if pid else "") + " ORDER BY created_at DESC LIMIT 100", (pid,) if pid else ())]
