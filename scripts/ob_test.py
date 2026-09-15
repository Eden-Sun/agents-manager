#!/usr/bin/env python3
"""Isolated OB queue/operator contract tests. Never contact the live daemon/browser."""
import contextlib
import io
import json
import os
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
        self.s.claim()
        self.s.finish(a["id"], "answer", UA)
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
        self.s.claim()
        self.s.fail(a["id"], "waiting_quota", "exhausted")
        self.ask(B)
        self.assertIsNone(self.s.claim())
        self.assertEqual(self.s.get(a["id"])["status"], "waiting_quota")
        self.s.set_setting("retry_after", time.time() - 1)
        self.assertEqual(self.s.claim()["id"], a["id"])

    def test_finish_rolls_back_if_conversation_is_owned_by_another_project(self):
        self.s.link(B, "b", UB)
        a = self.ask()
        self.s.claim()
        with self.assertRaises(Exception):
            self.s.finish(a["id"], "answer", UB)
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
        job = self.ask()
        self.s.claim()
        with patch.object(operator, "run_process", return_value=(1, '{"is_error":true,"result":"usage limit reached"}', "")) as proc:
            operator.operate(self.s, job, self.config())
        self.assertEqual(self.s.get(job["id"])["status"], "waiting_quota")
        self.assertEqual(proc.call_count, 1)

    def test_sonnet_fabricated_answer_is_not_accepted(self):
        job = self.ask()
        self.s.claim()
        with patch.object(operator, "run_process", return_value=(0, '{"result":"Pretend answer"}', "")):
            operator.operate(self.s, job, self.config())
        self.assertEqual(self.s.get(job["id"])["status"], "failed")
        self.assertIsNone(self.s.get(job["id"])["answer"])

    def test_browser_receipt_wins_if_sonnet_later_hits_quota(self):
        job = self.ask()
        self.s.claim()
        def run(*a, **kw):
            self.s.finish(job["id"], "actual web answer", UA)
            return (1, '{"is_error":true,"result":"usage limit"}', "")
        with patch.object(operator, "run_process", side_effect=run):
            operator.operate(self.s, job, self.config())
        self.assertEqual(self.s.get(job["id"])["status"], "done")

    def test_timeout_after_dispatch_is_unknown_not_retryable(self):
        job = self.ask()
        self.s.claim()
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


if __name__ == '__main__':
    unittest.main()
