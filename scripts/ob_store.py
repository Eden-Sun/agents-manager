"""OB durable queue. One SQLite transaction owns each state transition."""
import contextlib
import fcntl
import json
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
def exclusive(path):
    # Never unlink lock files: an old fd and a new inode could both hold the lock.
    with open(path, "a") as fd:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise OBError("busy")
        try:
            yield fd
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)


class Store:
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
        """)

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
            except sqlite3.IntegrityError:
                raise OBError("conversation_already_owned：其他 project 已使用此對話")

    def recover(self):
        # Only under worker.lock. A crashed operation may already have sent to GPT.
        self.db.execute("UPDATE requests SET status='unknown',error='worker_interrupted; inspect original conversation',updated_at=? WHERE status='running'", (time.time(),))

    def claim(self):
        with self.transaction():
            if self.setting("retry_after", 0) > time.time():
                return None
            row = self.db.execute("""SELECT * FROM requests r WHERE status IN ('pending','waiting_quota')
                AND NOT EXISTS (SELECT 1 FROM requests u WHERE u.project_id=r.project_id AND u.status IN ('unknown','running'))
                ORDER BY created_at,id LIMIT 1""").fetchone()
            if not row:
                return None
            self.db.execute("UPDATE requests SET status='running',error=NULL,updated_at=? WHERE id=?", (time.time(), row["id"]))
            return self.get(row["id"])

    def finish(self, ident, answer, url):
        conversation_url(url)
        if not answer.strip():
            raise OBError("empty_answer")
        with self.transaction():
            job = self.get(ident)
            if job["status"] == "done":
                return job
            if job["status"] not in ("running", "unknown"):
                raise OBError("request_not_running")
            current = self.project(job["project_id"])["url"]
            if current and current != url:
                raise OBError("conversation_changed")
            self.db.execute("UPDATE projects SET url=? WHERE id=?", (url, job["project_id"]))
            self.db.execute("UPDATE requests SET status='done',answer=?,url=?,error=NULL,updated_at=? WHERE id=?", (answer, url, time.time(), ident))
            return self.get(ident)

    def remember_url(self, ident, url):
        conversation_url(url)
        with self.transaction():
            job = self.get(ident)
            old = self.project(job["project_id"])["url"]
            if old and old != url:
                raise OBError("conversation_changed")
            self.db.execute("UPDATE projects SET url=? WHERE id=?", (url, job["project_id"]))
            self.db.execute("UPDATE requests SET url=? WHERE id=?", (url, ident))

    def fail(self, ident, status, error):
        if status not in ("failed", "unknown", "waiting_quota"):
            raise OBError("invalid_failure")
        with self.transaction():
            self.db.execute("UPDATE requests SET status=?,error=?,updated_at=? WHERE id=? AND status='running'", (status, error, time.time(), ident))
            if status == "waiting_quota":
                self.set_setting("retry_after", time.time() + 1800)

    def retry(self, ident):
        with self.transaction():
            job = self.get(ident)
            if job["status"] not in ("waiting_quota", "failed"):
                raise OBError("只能重試 waiting_quota／failed；unknown 必須先對帳，不可重送")
            self.db.execute("UPDATE requests SET status='pending',error=NULL,updated_at=? WHERE id=?", (time.time(), ident))
            self.set_setting("retry_after", 0)
        return self.get(ident)

    def list(self, pid=None):
        sql = "SELECT id,project_id,request_id,status,url,error,created_at,updated_at FROM requests"
        return [dict(r) for r in self.db.execute(sql + (" WHERE project_id=?" if pid else "") + " ORDER BY created_at DESC LIMIT 100", (pid,) if pid else ())]
