#!/usr/bin/env python3
"""scripts/agm.py 的測試。`python3 scripts/agm_test.py` 或 `python3 -m unittest`。

真的起一個 loopback HTTP server 來當 daemon：token 交換、proxy/redirect 的防護、
逾時的處理都只有走過真連線才驗得到。
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import socketserver
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import agm  # noqa: E402

TOKEN = "test-token-do-not-leak"


class FakeDaemon(BaseHTTPRequestHandler):
    routes: dict = {}
    seen: list = []
    slow: set = set()

    def log_message(self, *_args):  # 別把測試輸出洗掉
        pass

    def _run(self, method: str) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length) or b"null") if length else None
        path = self.path
        type(self).seen.append({"method": method, "path": path, "token": self.headers.get("X-AM-Token"), "body": body})
        if path in type(self).slow:
            time.sleep(1.5)
        entry = type(self).routes.get(f"{method} {path.split('?')[0]}")
        if entry is None:
            self._send(404, {"error": "not_found"})
            return
        self._send(*entry)

    def _send(self, status: int, payload: object) -> None:
        raw = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):  # noqa: N802
        self._run("GET")

    def do_POST(self):  # noqa: N802
        self._run("POST")

    def do_PUT(self):  # noqa: N802
        self._run("PUT")


class FakeServer(ThreadingHTTPServer):
    # 不設 daemon_threads 的話 `server_close()` 會去 join 還掛在 keep-alive 上的
    # handler 執行緒，每個 TestCase 類別的收尾都要卡好幾秒。
    daemon_threads = True
    allow_reuse_address = True

    def server_bind(self):
        # HTTPServer.server_bind 會呼叫 socket.getfqdn()，在沒有反解的機器上要等
        # 三十秒才逾時。測試不需要 server_name，跳過那一步。
        socketserver.TCPServer.server_bind(self)
        self.server_name = "127.0.0.1"
        self.server_port = self.server_address[1]


class CliCase(unittest.TestCase):
    """跑真的 `agm.main(argv)`，把 stdout/stderr 接下來當 JSON 讀。"""

    @classmethod
    def setUpClass(cls):
        cls.server = FakeServer(("127.0.0.1", 0), FakeDaemon)
        cls.port = cls.server.server_address[1]
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        FakeDaemon.routes = {"GET /api/session": (200, {"token": TOKEN})}
        FakeDaemon.seen = []
        FakeDaemon.slow = set()
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.write_runtime({"daemon_url": f"http://127.0.0.1:{self.port}", "manager_bot_id": "bot-agm"})
        os.environ["AGM_RUNTIME_DIR"] = self.dir.name
        self.addCleanup(lambda: os.environ.pop("AGM_RUNTIME_DIR", None))
        # 若 CLI 忘了關 proxy，這個位址不存在 → 連線失敗，測試會炸。
        for var in ("HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"):
            os.environ[var] = "http://127.0.0.1:9"
            self.addCleanup(lambda v=var: os.environ.pop(v, None))

    def write_runtime(self, cfg: dict) -> None:
        (Path(self.dir.name) / "runtime.json").write_text(json.dumps(cfg), encoding="utf-8")

    def run_cli(self, *argv: str):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = agm.main(list(argv))
            except SystemExit as e:  # argparse 的用法錯誤
                code = e.code if isinstance(e.code, int) else 2
        return code, out.getvalue(), err.getvalue()

    def ok(self, *argv: str):
        code, out, err = self.run_cli(*argv)
        self.assertEqual(code, 0, f"stderr={err}")
        return json.loads(out)

    def bad(self, *argv: str):
        code, out, err = self.run_cli(*argv)
        self.assertNotEqual(code, 0, f"expected failure, stdout={out}")
        return json.loads(err)


# --------------------------------------------------------------- loopback 防護


class LoopbackTest(unittest.TestCase):
    def test_accepts_loopback_forms(self):
        for url in ("http://127.0.0.1:7788", "http://localhost:7788", "http://[::1]:7788"):
            self.assertTrue(agm.check_loopback(url).startswith("http://"))

    def test_rejects_remote_host(self):
        with self.assertRaises(agm.AgmError) as cm:
            agm.check_loopback("http://192.168.1.9:7788")
        self.assertEqual(cm.exception.kind, "not_loopback")

    def test_rejects_userinfo(self):
        # `http://127.0.0.1@evil.tld/` 的 hostname 其實是 evil.tld；帳密形式一律擋掉。
        with self.assertRaises(agm.AgmError):
            agm.check_loopback("http://user:pw@127.0.0.1:7788")
        with self.assertRaises(agm.AgmError):
            agm.check_loopback("http://127.0.0.1@evil.example:7788")

    def test_rejects_non_http_scheme(self):
        with self.assertRaises(agm.AgmError):
            agm.check_loopback("file:///etc/passwd")

    def test_client_ctor_rechecks(self):
        with self.assertRaises(agm.AgmError):
            agm.Client("http://evil.example:7788")


class RuntimeTest(unittest.TestCase):
    def test_env_overrides_explicit(self):
        os.environ["AGM_RUNTIME_DIR"] = "/tmp/agm-env"
        self.addCleanup(lambda: os.environ.pop("AGM_RUNTIME_DIR", None))
        self.assertEqual(agm.runtime_dir("/tmp/agm-flag"), Path("/tmp/agm-env"))

    def test_manager_bot_id_accepts_legacy_key(self):
        self.assertEqual(agm.manager_bot_id({"bot_id": "b1"}), "b1")
        self.assertEqual(agm.manager_bot_id({"manager_bot_id": "b2", "bot_id": "b1"}), "b2")
        with self.assertRaises(agm.AgmError):
            agm.manager_bot_id({})


# ------------------------------------------------------------------- 精簡投影


class SlimTest(unittest.TestCase):
    def test_state_reads_bots_nested_under_projects(self):
        out = agm.slim_state(
            {
                "projects": [
                    {
                        "id": "p1",
                        "label": "AG Man",
                        "path": "/x",
                        # 這裡照 `GET /api/state` 真的會給的欄位：queued_turn 是一筆 turn，
                        # team 的階段叫 phase，燈號是 daemon 算好的 lamp。
                        "bots": [
                            {
                                "id": "b1",
                                "name": "cc0",
                                "model": "opus",
                                "lamp": "busy",
                                "queued_turn": {"id": "q1", "status": "queued", "origin": "web", "created_at": "2026-09-09T01:00:00Z"},
                                "team": {"id": "t1", "label": "登入修復", "role": "worker", "phase": "working"},
                            }
                        ],
                        "teams": [{"id": "t1", "label": "登入修復", "phase": "working", "pause_reason": None}],
                    }
                ]
            },
            "b1",
        )
        self.assertEqual([b["id"] for b in out["bots"]], ["b1"])
        self.assertEqual(out["bots"][0]["project_id"], "p1")
        self.assertEqual(out["bots"][0]["queued_turn"]["id"], "q1")
        self.assertEqual(out["bots"][0]["queued_turn"]["status"], "queued")
        self.assertEqual(out["bots"][0]["lamp"], "busy")
        self.assertEqual(out["bots"][0]["team"]["phase"], "working")
        self.assertTrue(out["bots"][0]["is_manager"])
        self.assertEqual(out["teams"][0]["project_id"], "p1")
        self.assertEqual(out["teams"][0]["phase"], "working")

    def test_state_bot_without_queue_or_team(self):
        out = agm.slim_state({"projects": [{"id": "p1", "bots": [{"id": "b1", "lamp": "idle", "queued_turn": None}]}]})
        self.assertIsNone(out["bots"][0]["queued_turn"])
        self.assertNotIn("team", out["bots"][0])

    def test_state_reads_top_level_bots(self):
        out = agm.slim_state({"bots": [{"id": "b9", "name": "n", "project_id": "p2"}]})
        self.assertEqual(out["bots"][0]["project_id"], "p2")
        self.assertFalse(out["bots"][0]["is_manager"])

    def test_state_dedupes_bot_seen_twice(self):
        out = agm.slim_state({"projects": [{"id": "p1", "bots": [{"id": "b1"}]}], "bots": [{"id": "b1"}]})
        self.assertEqual(len(out["bots"]), 1)

    def test_state_keeps_run_details_and_drops_secrets(self):
        out = agm.slim_state(
            {
                "projects": [
                    {
                        "id": "p1",
                        "bots": [
                            {
                                "id": "b1",
                                "env": {"ANTHROPIC_API_KEY": "sk-secret"},
                                "args": ["--dangerous"],
                                "persona": "long text",
                                "run": {
                                    "id": "r1",
                                    "state": "running",
                                    "agent_status": "working",
                                    "runtime_model": "fable",
                                    "runtime_effort": "low",
                                    "native_session_id": "sess-1",
                                },
                            }
                        ],
                    }
                ]
            }
        )
        bot = out["bots"][0]
        self.assertEqual(bot["run"]["agent_status"], "working")
        self.assertEqual(bot["run"]["runtime_model"], "fable")
        self.assertEqual(bot["run"]["native_session_id"], "sess-1")
        blob = json.dumps(out)
        self.assertNotIn("sk-secret", blob)
        self.assertNotIn("dangerous", blob)
        self.assertNotIn("long text", blob)

    def test_message_keeps_evidence_quality_fields(self):
        m = agm.slim_message(
            {"id": "m1", "role": "assistant", "content": "半截", "source": "terminal_fallback", "incomplete": True}
        )
        self.assertEqual(m["source"], "terminal_fallback")
        self.assertTrue(m["incomplete"])


# -------------------------------------------------------------------- 子命令


class StateCommandTest(CliCase):
    def test_prefers_supervisor_state_and_marks_manager(self):
        FakeDaemon.routes["GET /api/supervisor/state"] = (200, {"bots": [{"id": "bot-agm", "name": "AGM"}]})
        out = self.ok("state")
        self.assertTrue(out["bots"][0]["is_manager"])
        self.assertEqual(out["manager_bot_id"], "bot-agm")

    def test_falls_back_to_api_state_on_404(self):
        FakeDaemon.routes["GET /api/state"] = (200, {"projects": [{"id": "p1", "bots": [{"id": "b1", "env": {"K": "sekrit"}}]}]})
        out = self.ok("state")
        self.assertEqual(out["bots"][0]["id"], "b1")
        self.assertNotIn("sekrit", json.dumps(out))

    def test_token_never_appears_in_output(self):
        FakeDaemon.routes["GET /api/state"] = (200, {"projects": []})
        code, out, err = self.run_cli("state")
        self.assertEqual(code, 0)
        self.assertNotIn(TOKEN, out + err)
        # …但請求上有帶。
        self.assertIn(TOKEN, [r["token"] for r in FakeDaemon.seen if r["path"].startswith("/api/state")])

    def test_no_raw_flag(self):
        # `--raw` 會把 env 原樣倒出來，所以不該存在。
        code, _out, _err = self.run_cli("state", "--raw")
        self.assertEqual(code, 2)


class SearchCommandTest(CliCase):
    def test_uses_evidence_endpoint(self):
        FakeDaemon.routes["GET /api/supervisor/evidence"] = (
            200,
            {"messages": [{"id": "m1", "bot_id": "b1", "turn_id": "t1", "content": "登入"}], "has_more": True, "next_cursor": "m1"},
        )
        out = self.ok("search", "登入", "--project", "p1")
        self.assertEqual(out["source"], "evidence")
        self.assertEqual(out["messages"][0]["turn_id"], "t1")
        self.assertEqual(out["next_cursor"], "m1")
        path = [r["path"] for r in FakeDaemon.seen if "evidence" in r["path"]][0]
        self.assertIn("project_id=p1", path)

    def test_falls_back_to_legacy_search_with_a_warning(self):
        FakeDaemon.routes["GET /api/search/messages"] = (200, {"b1": {"hits": 3, "snippet": "…"}})
        out = self.ok("search", "登入")
        self.assertEqual(out["source"], "legacy_search")
        self.assertIn("沒有 message/turn ID", out["note"])


class MessagesCommandTest(CliCase):
    def test_cursor_comes_from_the_oldest_message(self):
        FakeDaemon.routes["GET /api/bots/b1/messages"] = (
            200,
            {
                "bot_id": "b1",
                "conversation_id": "c1",
                "messages": [{"id": "m5", "role": "user"}, {"id": "m6", "role": "assistant"}],
                # db::Turn 的真欄位：status / completed_at（不是 state / ended_at）。
                "turns": [
                    {
                        "id": "t1",
                        "conversation_id": "c1",
                        "run_id": "r1",
                        "origin": "web",
                        "status": "done",
                        "delivery": "confirmed",
                        "client_request_id": "req-1",
                        "created_at": "2026-09-09T01:00:00Z",
                        "completed_at": "2026-09-09T01:02:00Z",
                    }
                ],
                "has_more": True,
            },
        )
        out = self.ok("messages", "b1")
        # 訊息是正序，往前翻要用第一則（最舊）的 id。
        self.assertEqual(out["next_cursor"], "m5")
        turn = out["turns"][0]
        self.assertEqual(turn["delivery"], "confirmed")
        self.assertEqual(turn["status"], "done")
        self.assertEqual(turn["completed_at"], "2026-09-09T01:02:00Z")
        self.assertEqual(turn["client_request_id"], "req-1")

    def test_no_cursor_when_no_more(self):
        FakeDaemon.routes["GET /api/bots/b1/messages"] = (200, {"messages": [{"id": "m5"}], "has_more": False})
        self.assertIsNone(self.ok("messages", "b1")["next_cursor"])


class AssignCommandTest(CliCase):
    def test_posts_the_given_request_id(self):
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a1", "status": "pending"})
        out = self.ok("assign", "--bot", "b1", "--text", "修登入", "--request-id", "req-1")
        self.assertEqual(out["id"], "a1")
        body = [r["body"] for r in FakeDaemon.seen if r["method"] == "POST"][0]
        self.assertEqual(body, {"target_bot_id": "b1", "text": "修登入", "client_request_id": "req-1"})

    def test_timeout_reports_delivery_unknown_and_sends_once(self):
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a1"})
        FakeDaemon.slow = {"/api/supervisor/assignments"}
        err = self.bad("--timeout", "0.3", "assign", "--bot", "b1", "--text", "x", "--request-id", "req-9")
        self.assertEqual(err["error"], "delivery_unknown")
        self.assertEqual(err["client_request_id"], "req-9")
        posts = [r for r in FakeDaemon.seen if r["path"] == "/api/supervisor/assignments"]
        self.assertEqual(len(posts), 1, "逾時不能自己重送——那會派出第二份同樣的工")

    def test_empty_text_rejected_before_any_request(self):
        err = self.bad("assign", "--bot", "b1", "--text", "   ", "--request-id", "r")
        self.assertEqual(err["error"], "bad_args")
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [])

    def test_text_file(self):
        f = Path(self.dir.name) / "t.md"
        f.write_text("從檔案來的交辦", encoding="utf-8")
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a2"})
        self.ok("assign", "--bot", "b1", "--text-file", str(f), "--request-id", "r2")
        body = [r["body"] for r in FakeDaemon.seen if r["method"] == "POST"][0]
        self.assertEqual(body["text"], "從檔案來的交辦")


class AssignmentsCommandTest(CliCase):
    def setUp(self):
        super().setUp()
        FakeDaemon.routes["GET /api/supervisor/assignments"] = (
            200,
            {"assignments": [{"id": "a1", "status": "pending", "client_request_id": "req-1"}, {"id": "a2", "status": "done"}]},
        )

    def test_filter_by_status(self):
        out = self.ok("assignments", "--status", "pending")
        self.assertEqual([a["id"] for a in out["assignments"]], ["a1"])

    def test_single_lookup_by_id_or_request_id(self):
        self.assertEqual(self.ok("assignments", "--id", "a2")["id"], "a2")
        self.assertEqual(self.ok("assignments", "--id", "req-1")["id"], "a1")
        # 走清單過濾，不打 /assignments/{id}。
        self.assertEqual([r["path"] for r in FakeDaemon.seen if r["path"].startswith("/api/supervisor/assignments/")], [])

    def test_missing_id_is_not_found(self):
        self.assertEqual(self.bad("assignments", "--id", "nope")["error"], "not_found")


class MiscCommandTest(CliCase):
    def test_handoff_read_and_write(self):
        FakeDaemon.routes["GET /api/supervisor/handoff"] = (200, {"summary": "舊摘要"})
        FakeDaemon.routes["PUT /api/supervisor/handoff"] = (200, {"ok": True})
        self.assertEqual(self.ok("handoff")["summary"], "舊摘要")
        self.ok("handoff", "--summary", "新摘要")
        body = [r["body"] for r in FakeDaemon.seen if r["method"] == "PUT"][0]
        self.assertEqual(body, {"summary": "新摘要"})

    def test_ack_and_inbox(self):
        FakeDaemon.routes["GET /api/supervisor/inbox"] = (200, {"events": [{"id": "e1"}]})
        FakeDaemon.routes["POST /api/supervisor/inbox/e1/ack"] = (200, {"ok": True})
        self.assertEqual(self.ok("inbox")["events"][0]["id"], "e1")
        self.assertTrue(self.ok("ack", "e1")["ok"])

    def test_supervisor_actions(self):
        for act in ("setup", "start", "stop", "fallback"):
            FakeDaemon.routes[f"POST /api/supervisor/{act}"] = (200, {"status": act})
            self.assertEqual(self.ok(f"supervisor-{act}")["status"], act)

    def test_bot_create_needs_project_and_name(self):
        self.assertEqual(self.bad("bot", "create")["error"], "bad_args")

    def test_bot_restart(self):
        FakeDaemon.routes["POST /api/bots/b1/restart"] = (200, {"run_id": "r1"})
        self.assertEqual(self.ok("bot", "restart", "b1")["run_id"], "r1")

    def test_http_error_is_structured_and_non_zero(self):
        FakeDaemon.routes["GET /api/supervisor/handoff"] = (409, {"error": "conflict", "reason": "忙碌中"})
        err = self.bad("handoff")
        self.assertEqual(err["error"], "http_error")
        self.assertEqual(err["status"], 409)
        self.assertEqual(err["detail"]["reason"], "忙碌中")

    def test_missing_runtime_json(self):
        os.environ["AGM_RUNTIME_DIR"] = str(Path(self.dir.name) / "nope")
        self.assertEqual(self.bad("state")["error"], "no_runtime")

    def test_remote_daemon_url_refused_before_any_request(self):
        self.write_runtime({"daemon_url": "http://10.0.0.5:7788", "manager_bot_id": "b"})
        self.assertEqual(self.bad("state")["error"], "not_loopback")
        self.assertEqual(FakeDaemon.seen, [])


class HelpTest(unittest.TestCase):
    def test_help_mentions_the_no_retry_rule(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(SystemExit):
            agm.main(["--help"])
        text = out.getvalue()
        self.assertIn("agm assign", text)
        self.assertIn("不要", text)


if __name__ == "__main__":
    unittest.main(verbosity=2)
