#!/usr/bin/env python3
"""Isolated OB queue/operator contract tests. Never contact the live daemon/browser."""
import contextlib
import io
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest.mock import patch

import ob
import ob_operator as operator
from ob_store import OBError, Store, exclusive

A = "01M1Y7BNVP843V9MFEDJ2KW9NQ"
B = "01M1Y75JS1G8PZ4EHF6Y98AFB1"
C = "01M1Y7BNVP843V9MFEDJ2KW9NR"
UA = "https://chatgpt.com/c/project-a"
UB = "https://chatgpt.com/c/project-b"


class QueueTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.s = Store(self.tmp.name)
        self.addCleanup(self.s.db.close)

    def ask(self, pid=A, rid="q1", text="Which design?", source="bot-a"):
        return self.s.submit(pid, "same display name", rid, text, source)

    def test_same_name_and_request_id_are_isolated_by_project_id(self):
        a, b = self.ask(), self.ask(B)
        self.assertNotEqual(a["id"], b["id"])
        self.s.link(A, "same", UA)
        self.s.link(B, "same", UB)
        self.assertEqual(self.s.project(A)["url"], UA)
        self.assertEqual(self.s.project(B)["url"], UB)

    def test_request_replay_does_not_requeue_completed_work(self):
        a = self.ask()
        self.s.finish(a["id"], "answer", UA, self.s.claim()["claim_token"])
        again = self.ask()
        self.assertEqual(again["status"], "done")
        self.assertEqual(len(self.s.list()), 1)
        self.assertIsNone(self.s.claim())

    def test_mismatched_body_whitespace_or_source_are_rejected(self):
        self.ask()
        for kwargs in ({"text": "Which design? "}, {"source": "bot-b"}, {"text": "other"}):
            with self.assertRaisesRegex(OBError, "request_mismatch"):
                self.ask(**kwargs)
        self.assertEqual(len(self.s.list()), 1)

    def test_labels_are_rejected_as_project_ids(self):
        for pid in ("agents-manager", "../../oops", "", "01M1Y7BNVP843V9MFEDJ2KW9NQ-prefix"):
            with self.assertRaises(OBError):
                self.ask(pid)

    def test_project_rename_does_not_change_conversation(self):
        self.s.link(A, "old", UA)
        self.s.submit(A, "new", "q", "text")
        self.assertEqual(self.s.project(A), {"id": A, "label": "new", "url": UA})

    def test_conversation_cannot_be_shared_or_silently_replaced(self):
        self.s.link(A, "a", UA)
        with self.assertRaisesRegex(OBError, "conversation_already_owned"):
            self.s.link(B, "b", UA)
        with self.assertRaisesRegex(OBError, "project_already_linked"):
            self.s.link(A, "a", UB)

    def test_external_or_malformed_url_is_refused(self):
        for url in ("https://evil.test/c/id", UA + "?token=x", None, "https://chatgpt.com/"):
            with self.assertRaises(OBError):
                self.s.link(A, "a", url)

    def test_concurrent_submissions_write_one_row(self):
        barrier = threading.Barrier(2)
        def write(_):
            s = Store(self.tmp.name)
            try:
                barrier.wait()
                return s.submit(A, "a", "same", "question")["id"]
            finally:
                s.db.close()
        with ThreadPoolExecutor(2) as pool:
            result = list(pool.map(write, range(2)))
        self.assertEqual(result[0], result[1])
        self.assertEqual(len(self.s.list()), 1)

    def test_concurrent_project_links_do_not_lose_each_other(self):
        def link(pair):
            s = Store(self.tmp.name)
            try:
                s.link(pair[0], "label", pair[1])
            finally:
                s.db.close()
        with ThreadPoolExecutor(2) as pool:
            list(pool.map(link, [(A, UA), (B, UB)]))
        self.assertEqual(self.s.project(A)["url"], UA)
        self.assertEqual(self.s.project(B)["url"], UB)

    def test_crash_blocks_only_the_affected_project_and_never_resends(self):
        a = self.ask()
        self.s.claim()
        self.s.recover()
        self.ask(A, "second")
        b = self.ask(B)
        self.assertEqual(self.s.claim()["id"], b["id"])
        self.assertEqual(self.s.get(a["id"])["status"], "unknown")
        with self.assertRaisesRegex(OBError, "unknown"):
            self.s.retry(a["id"])

    def test_quota_waiting_preserves_queue_and_backs_off_globally(self):
        a = self.ask()
        self.s.fail(a["id"], "waiting_quota", "exhausted", self.s.claim()["claim_token"])
        self.ask(B)
        self.assertIsNone(self.s.claim())
        self.assertEqual(self.s.get(a["id"])["status"], "waiting_quota")
        self.s.set_setting("retry_after", time.time() - 1)
        self.assertEqual(self.s.claim()["id"], a["id"])

    def test_finish_rolls_back_if_conversation_is_owned_by_another_project(self):
        self.s.link(B, "b", UB)
        a = self.ask()
        token = self.s.claim()["claim_token"]
        with self.assertRaises(sqlite3.IntegrityError):
            self.s.finish(a["id"], "answer", UB, token)
        self.assertEqual(self.s.get(a["id"])["status"], "running")
        self.assertIsNone(self.s.project(A)["url"])

    def test_recovery_collect_finishes_unknown_without_new_request(self):
        a = self.ask()
        self.s.claim()
        self.s.recover()
        operator.journal_for(self.s, a["id"]).write_text(json.dumps({"phase": "done", "answer": "recovered", "url": UA}))
        with patch.object(operator, "run_process", side_effect=AssertionError("must not send")):
            r = operator.browser_consult(self.s, a["id"], {}, collect=True)
        self.assertEqual(r["answer"], "recovered")
        self.assertEqual(len(self.s.list()), 1)

    def test_worker_lock_is_exclusive_and_survives_lock_file_reuse(self):
        path = Path(self.tmp.name) / "worker.lock"
        with exclusive(path):
            with self.assertRaisesRegex(OBError, "busy"):
                with exclusive(path):
                    pass
        with exclusive(path):
            self.assertTrue(path.exists())

    def test_unknown_requires_explicit_not_sent_confirmation(self):
        a = self.ask()
        self.s.claim()
        self.s.recover()
        journal = operator.journal_for(self.s, a["id"])
        journal.write_text('{"phase":"dispatching"}')
        with self.assertRaises(OBError):
            ob.resolve_unknown(self.s, a["id"])
        ob.resolve_unknown(self.s, a["id"], True)
        self.assertEqual(self.s.get(a["id"])["status"], "pending")
        self.assertEqual(len(list(Path(self.tmp.name).glob('*.resolved-*'))), 1)


class OperatorTests(unittest.TestCase):
    setUp = QueueTests.setUp
    ask = QueueTests.ask
    def config(self):
        return dict(claude_config_dir=str(Path.home() / ".claude"), claude_binary="/fake/claude", ego_binary="/fake/ego")

    def test_environment_does_not_inherit_account_token_bot_model_or_session(self):
        with patch.dict(os.environ, {"AM_BOT_ID": "secret-bot", "AM_HOOK_TOKEN": "secret", "ANTHROPIC_API_KEY": "key",
                                    "CLAUDE_CONFIG_DIR": "/other-account", "CLAUDECODE": "session", "AM_MODEL": "fable"}):
            env = operator.clean_env(self.config())
        self.assertNotIn("AM_BOT_ID", env)
        self.assertNotIn("AM_HOOK_TOKEN", env)
        self.assertNotIn("ANTHROPIC_API_KEY", env)
        self.assertNotIn("CLAUDE_CONFIG_DIR", env)
        self.assertNotIn("CLAUDECODE", env)
        self.assertNotIn("AM_MODEL", env)
        self.assertEqual(env.get("USER"), os.environ.get("USER"))

    def test_explicit_secondary_account_is_used_without_fallback(self):
        cfg = self.config() | {"claude_config_dir": "/account/cc1"}
        self.assertEqual(operator.clean_env(cfg)["CLAUDE_CONFIG_DIR"], "/account/cc1")
        argv = operator.operator_argv(cfg, "/tmp/mcp")
        self.assertEqual(argv[argv.index("--model") + 1], "sonnet")
        self.assertEqual(argv[argv.index("--tools") + 1], "")
        self.assertNotIn("--fallback-model", argv)
        self.assertNotIn("--resume", argv)
        self.assertIn("--no-session-persistence", argv)

    def test_quota_detection_never_reads_successful_answer_as_an_error(self):
        self.assertFalse(operator.quota_error(json.dumps({"is_error": False, "result": "rate limit / quota"})))
        self.assertTrue(operator.quota_error(json.dumps({"is_error": True, "result": "You've hit your limit"})))
        self.assertFalse(operator.quota_error("invalid json"))

    def test_operator_quota_error_never_calls_browser_or_changes_model(self):
        self.ask()
        job = self.s.claim()
        with patch.object(operator, "run_process", return_value=(1, '{"is_error":true,"result":"usage limit reached"}', "")) as proc:
            operator.operate(self.s, job, self.config())
        self.assertEqual(self.s.get(job["id"])["status"], "waiting_quota")
        self.assertEqual(proc.call_count, 1)

    def test_sonnet_fabricated_answer_is_not_accepted(self):
        self.ask()
        job = self.s.claim()
        with patch.object(operator, "run_process", return_value=(0, '{"result":"Pretend answer"}', "")):
            operator.operate(self.s, job, self.config())
        self.assertEqual(self.s.get(job["id"])["status"], "failed")
        self.assertIsNone(self.s.get(job["id"])["answer"])

    def test_browser_receipt_wins_if_sonnet_later_hits_quota(self):
        self.ask()
        job = self.s.claim()
        def run(*a, **kw):
            self.s.finish(job["id"], "actual web answer", UA, job["claim_token"])
            return (1, '{"is_error":true,"result":"usage limit"}', "")
        with patch.object(operator, "run_process", side_effect=run):
            operator.operate(self.s, job, self.config())
        self.assertEqual(self.s.get(job["id"])["status"], "done")

    def test_timeout_after_dispatch_is_unknown_not_retryable(self):
        self.ask()
        job = self.s.claim()
        def run(*a, **kw):
            operator.journal_for(self.s, job["id"]).write_text('{"phase":"dispatching"}')
            raise subprocess.TimeoutExpired("claude", 900)
        with patch.object(operator, "run_process", side_effect=run):
            operator.operate(self.s, job, self.config())
        self.assertEqual(self.s.get(job["id"])["status"], "unknown")

    def test_mcp_cannot_change_project_or_question(self):
        job = self.ask()
        self.s.claim()
        request = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "consult", "arguments": {"project_id": B}}}
        output = io.StringIO()
        with patch('sys.stdin', io.StringIO(json.dumps(request) + '\n')), contextlib.redirect_stdout(output), patch.object(operator, 'browser_consult') as browser:
            operator.mcp_server(self.tmp.name, job["id"])
        self.assertTrue(json.loads(output.getvalue())["result"]["isError"])
        browser.assert_not_called()


MIGRATE = r"""
import sys, time
from ob_store import Store
root, at = sys.argv[1], float(sys.argv[2])
time.sleep(max(0, at - time.time()))
Store(root).db.close()
"""


class MigrationTests(unittest.TestCase):
    def test_concurrent_first_runs_upgrade_an_old_database_once(self):
        for _ in range(3):
            with tempfile.TemporaryDirectory() as tmp:
                db = sqlite3.connect(Path(tmp) / "ob.sqlite3")
                db.execute("PRAGMA journal_mode=WAL")  # as every existing OB database was created
                db.executescript("""CREATE TABLE projects (id TEXT PRIMARY KEY, label TEXT NOT NULL, url TEXT UNIQUE);
                    CREATE TABLE requests (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
                        request_id TEXT NOT NULL, question TEXT NOT NULL, source_bot_id TEXT,
                        status TEXT NOT NULL DEFAULT 'pending', answer TEXT, url TEXT,
                        error TEXT, created_at REAL NOT NULL, updated_at REAL NOT NULL, UNIQUE(project_id, request_id));
                    CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);""")
                db.execute("INSERT INTO projects VALUES (?,?,NULL)", (A, "AM"))
                db.execute("INSERT INTO requests(id,project_id,request_id,question,status,created_at,updated_at) VALUES ('r',?,'q','x','running',0,0)", (A,))
                db.commit()
                db.close()
                env = dict(os.environ, PYTHONPATH=str(Path(__file__).resolve().parent))
                at = time.time() + 1
                procs = [subprocess.Popen([sys.executable, "-B", "-c", MIGRATE, tmp, str(at)], env=env,
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for _ in range(12)]
                errors = [err for p in procs for _, err in [p.communicate(30)] if p.returncode]
                self.assertEqual(errors, [])
                s = Store(tmp)
                cols = [r[1] for r in s.db.execute("PRAGMA table_info(requests)")]
                self.assertEqual(cols.count("claim_token"), 1)
                self.assertEqual(s.get("r")["status"], "running")  # upgrade alone never touches requests
                s.db.close()


class CLITests(unittest.TestCase):
    def test_configure_preserves_stable_cli_symlinks_across_updates(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / 'version-one'
            binary.write_text('#!/bin/sh\nexit 0\n')
            binary.chmod(0o755)
            link = root / 'stable-cli'
            link.symlink_to(binary)
            with contextlib.redirect_stdout(io.StringIO()):
                ob.main(['--data-dir', str(root / 'db'), 'configure', '--claude-config-dir', tmp,
                         '--claude-binary', str(link), '--ego-binary', str(link)])
            store = Store(root / 'db')
            self.addCleanup(store.db.close)
            self.assertEqual(store.setting('operator')['claude_binary'], str(link))
            self.assertEqual(store.setting('operator')['ego_binary'], str(link))

    def test_ask_resolves_project_from_daemon_not_cwd(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(ob, 'daemon_project', return_value={"id": A, "label": "AM"}), patch.dict(os.environ, {"AM_BOT_ID": "caller"}), contextlib.redirect_stdout(io.StringIO()) as output:
            ob.main(['--data-dir', tmp, 'ask', '--request-id', 'id', '--no-start', 'Question'])
            row = json.loads(output.getvalue())
            self.assertEqual(row['project_id'], A)
            self.assertEqual(row['source_bot_id'], 'caller')
            self.assertFalse(row['operator_configured'])


HOLDER = r"""
import os, sys, json
from pathlib import Path
from ob_store import Store, exclusive
root, journal = sys.argv[1], sys.argv[2]
s = Store(root)
with exclusive(Path(root) / "worker.lock"):
    job = s.claim()
    if journal:
        (Path(root) / (job["id"] + ".browser.json")).write_text(journal)
    print(job["id"], flush=True)
    if sys.stdin.readline().strip() == "die":
        os.kill(os.getpid(), 9)  # no cleanup, no finally: a crashed or rebooted worker
"""


class WorkerLossTests(unittest.TestCase):
    """Real worker processes and flock; only daemon lookup and process spawning are stubbed."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.s = Store(self.tmp.name)
        self.addCleanup(self.s.db.close)
        self.s.set_setting("operator", {"claude_binary": "/fake/claude", "ego_binary": "/fake/ego",
                                        "claude_config_dir": str(Path.home() / ".claude")})
        self.kicks = []
        for p in (patch.object(ob, "daemon_project", return_value={"id": A, "label": "AM"}),
                  patch.object(ob, "kick", side_effect=lambda store: self.kicks.append(1)),
                  patch.object(operator, "run_process", side_effect=AssertionError("must not send")),
                  patch.dict(os.environ, {"AM_BOT_ID": "bot-a"})):
            p.start()
            self.addCleanup(p.stop)

    def cli(self, *argv):
        with contextlib.redirect_stdout(io.StringIO()) as out:
            ob.main(["--data-dir", self.tmp.name, *argv])
        return json.loads(out.getvalue())

    def ask(self, rid="q1"):
        return self.cli("ask", "--request-id", rid, "Which design?")

    def worker(self, journal=""):
        env = dict(os.environ, PYTHONPATH=str(Path(__file__).resolve().parent))
        proc = subprocess.Popen([sys.executable, "-B", "-c", HOLDER, self.tmp.name, journal], env=env, text=True,
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        def cleanup():
            if proc.poll() is None:
                proc.kill()
            proc.wait()
            proc.stdout.close()
            proc.stdin.close()
        self.addCleanup(cleanup)
        ident = proc.stdout.readline().strip()
        self.assertEqual(self.s.get(ident)["status"], "running")
        return proc, ident

    def kill(self, proc):
        proc.stdin.write("die\n")
        proc.stdin.flush()
        self.assertEqual(proc.wait(10), -9)

    def test_replay_after_worker_crash_marks_unknown_and_never_resends(self):
        first = self.ask()
        proc, ident = self.worker('{"phase":"sent","url":"%s"}' % UA)
        self.assertEqual(ident, first["id"])
        self.kill(proc)
        self.kicks.clear()
        again = self.ask()
        self.assertEqual((again["id"], again["status"]), (first["id"], "unknown"))
        self.assertNotIn("claim_token", again)
        self.assertEqual(self.kicks, [])  # nothing claimable: no worker started for a possibly sent request
        self.assertEqual(len(self.s.list()), 1)
        self.assertEqual(self.s.project(A)["url"], UA)  # URL learned before the crash kept for collect
        with self.assertRaisesRegex(OBError, "unknown"):
            self.cli("retry", ident)
        # A restarted worker does not pick it up; a later request of the same project waits behind it.
        self.ask("q2")
        with exclusive(Path(self.tmp.name) / "worker.lock"):
            self.assertIsNone(self.s.claim())
        # collect only reads back the original answer.
        operator.journal_for(self.s, ident).write_text(json.dumps({"phase": "done", "answer": "original", "url": UA}))
        self.assertEqual(self.cli("collect", ident)["answer"], "original")

    def test_live_worker_request_is_not_rewritten_by_replay_status_or_recover(self):
        self.ask()
        proc, ident = self.worker()
        self.assertEqual(self.ask()["status"], "running")
        self.assertEqual(self.cli("status", ident)["status"], "running")
        status = self.cli("status")
        self.assertEqual((status["worker_running"], status["recovered"]), (True, []))
        self.assertEqual(self.cli("recover"), {"worker_running": True, "recovered": []})
        with self.assertRaisesRegex(OBError, "request_not_claimed"):
            self.cli("collect", ident)
        self.assertEqual(self.s.get(ident)["status"], "running")
        proc.stdin.write("exit\n")  # a clean exit that skipped finish/fail also leaves running
        proc.stdin.flush()
        self.assertEqual(proc.wait(10), 0)
        self.assertEqual(self.cli("recover"), {"worker_running": False, "recovered": [ident]})
        self.assertEqual(self.cli("recover"), {"worker_running": False, "recovered": []})

    def test_status_recovers_stale_running_and_recover_kicks_only_claimable_work(self):
        self.ask()
        proc, ident = self.worker()
        self.kill(proc)
        self.s.submit(A, "AM", "q2", "same project, blocked behind the unknown one")
        status = self.cli("status")
        self.assertEqual((status["worker_running"], status["recovered"]), (False, [ident]))
        self.assertEqual(self.s.get(ident)["status"], "unknown")
        self.assertEqual(self.kicks, [1])  # only the original ask kicked; status never starts a worker
        c = self.s.submit(C, "C", "q", "project c")
        proc, claimed = self.worker()
        self.assertEqual(claimed, c["id"])
        self.kill(proc)
        self.assertEqual(self.cli("recover"), {"worker_running": False, "recovered": [c["id"]]})
        self.assertEqual(self.kicks, [1])  # A and C are both blocked: nothing a worker could claim
        b = self.s.submit(B, "B", "q", "project b")
        self.s.db.execute("UPDATE requests SET status='running' WHERE id=?", (c["id"],))  # a second lost claim
        self.assertEqual(self.cli("recover")["recovered"], [c["id"]])
        self.assertEqual(self.kicks, [1, 1])  # queued work of another project is restarted
        self.assertEqual(self.s.get(b["id"])["status"], "pending")

    def test_orphaned_operator_cannot_send_for_a_later_claim(self):
        self.ask()
        proc, ident = self.worker()
        old_token = self.s.get(ident)["claim_token"]
        self.kill(proc)
        self.cli("recover")
        ob.resolve_unknown(self.s, ident, True)  # operator checked the conversation: not sent
        with exclusive(Path(self.tmp.name) / "worker.lock"):
            self.assertEqual(self.s.claim()["id"], ident)
        self.assertNotEqual(self.s.get(ident)["claim_token"], old_token)
        with self.assertRaisesRegex(OBError, "request_not_claimed"):
            operator.browser_consult(self.s, ident, {}, token=old_token)
        with self.assertRaisesRegex(OBError, "request_not_claimed"):
            operator.browser_consult(self.s, ident, {})

    def test_results_of_a_lost_claim_never_overwrite_the_new_claim(self):
        first = self.ask()
        proc, ident = self.worker()
        stale = self.s.get(ident)  # what the dead worker's operate() was holding
        self.kill(proc)
        self.cli("recover")
        ob.resolve_unknown(self.s, ident, True)
        with exclusive(Path(self.tmp.name) / "worker.lock"):
            fresh = self.s.claim()
        self.assertEqual(fresh["id"], first["id"])
        # Late post-processing of the old claim: quota, no receipt, exception with a journal, and a done journal.
        for out in ('{"is_error":true,"result":"usage limit"}', '{"result":"no receipt"}'):
            with patch.object(operator, "run_process", return_value=(1, out, "")):
                operator.operate(self.s, stale, {})
        self.assertEqual(self.s.setting("retry_after", 0), 0)
        operator.journal_for(self.s, ident).write_text('{"phase":"dispatching"}')
        with patch.object(operator, "run_process", side_effect=RuntimeError("late")):
            operator.operate(self.s, stale, {})
        operator.journal_for(self.s, ident).write_text(json.dumps({"phase": "done", "answer": "old", "url": UA}))
        with patch.object(operator, "run_process", return_value=(0, "", "")):
            operator.operate(self.s, stale, {})
        with self.assertRaisesRegex(OBError, "claim_lost"):
            self.s.finish(ident, "old", UA, stale["claim_token"])
        row = self.s.get(ident)
        self.assertEqual((row["status"], row["claim_token"], row["answer"]), ("running", fresh["claim_token"], None))
        self.assertEqual(self.s.finish(ident, "new", UA, fresh["claim_token"])["status"], "done")

    def test_collect_that_loses_the_race_to_resolve_and_reclaim_cannot_finish(self):
        self.ask()
        proc, ident = self.worker()
        self.kill(proc)
        self.cli("recover")
        real_exclusive = operator.exclusive
        @contextlib.contextmanager
        def resolve_first(path, wait=0):
            # collect passed its unlocked unknown check; resolve and a new claim win browser.lock first.
            if path.name == "browser.lock" and self.s.get(ident)["status"] == "unknown":
                ob.resolve_unknown(self.s, ident, True)
                with real_exclusive(Path(self.tmp.name) / "worker.lock"):
                    self.s.claim()
            with real_exclusive(path, wait) as fd:
                yield fd
        operator.journal_for(self.s, ident).write_text(json.dumps({"phase": "done", "answer": "stale", "url": UA}))
        with patch.object(operator, "exclusive", resolve_first), patch.object(ob, "recover"):
            with self.assertRaisesRegex(OBError, "request_not_unknown"):
                self.cli("collect", ident)
        row = self.s.get(ident)
        self.assertEqual((row["status"], row["answer"]), ("running", None))
        with self.assertRaisesRegex(OBError, "claim_lost"):
            self.s.finish(ident, "stale", UA)  # no token: never over a running claim
        self.s.db.execute("UPDATE requests SET status='unknown' WHERE id=?", (ident,))
        self.assertEqual(self.s.finish(ident, "collected", UA)["status"], "done")
        self.assertEqual(self.cli("collect", ident)["answer"], "collected")  # done returns the original result

    def test_no_start_replay_recovers_without_starting_a_worker(self):
        self.ask()
        proc, ident = self.worker()
        self.kill(proc)
        self.s.submit(B, "B", "q", "claimable work of another project")
        self.kicks.clear()
        row = self.cli("ask", "--request-id", "q1", "--no-start", "--wait", "1", "Which design?")
        self.assertEqual(row["status"], "unknown")
        self.assertEqual(self.kicks, [])
        self.cli("ask", "--request-id", "q9", "--no-start", "new question")
        self.assertEqual(self.kicks, [])

    def test_kicked_worker_waits_out_a_short_probe_instead_of_exiting_busy(self):
        lock = Path(self.tmp.name) / "worker.lock"
        held = threading.Event()
        def probe():
            with exclusive(lock):
                held.set()
                time.sleep(0.5)
        t = threading.Thread(target=probe)
        t.start()
        held.wait(5)
        with patch.object(operator, "operate") as op:
            self.s.submit(A, "AM", "q", "question")
            operator.work(self.s, once=True)
        t.join()
        self.assertEqual(op.call_count, 1)
        with self.assertRaisesRegex(OBError, "busy"):
            with exclusive(lock):
                start = time.monotonic()
                with exclusive(lock, wait=0.3):
                    pass
        self.assertGreaterEqual(time.monotonic() - start, 0.3)


if __name__ == '__main__':
    unittest.main()
