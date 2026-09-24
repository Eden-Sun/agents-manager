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
import socket
import socketserver
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import agm  # noqa: E402

TOKEN = "test-token-do-not-leak"


class FakeDaemon(BaseHTTPRequestHandler):
    routes: dict = {}
    seen: list = []
    slow: set = set()
    drop: set = set()

    def log_message(self, *_args):  # 別把測試輸出洗掉
        pass

    def _run(self, method: str) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length) or b"null") if length else None
        path = self.path
        type(self).seen.append({
            "method": method, "path": path, "token": self.headers.get("X-AM-Token"), "body": body,
            "bot_id": self.headers.get("X-AM-Bot-Id"), "bot_token": self.headers.get("X-AM-Bot-Token"),
            "caller": self.headers.get("X-AM-Caller"),
        })
        if path in type(self).slow:
            time.sleep(1.5)
        if path in type(self).drop:
            # 收到請求後不回應就把連線掐掉（送達未知的那種故障）。
            self.request.shutdown(socket.SHUT_RDWR)
            return
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

    def do_PATCH(self):  # noqa: N802
        self._run("PATCH")

    def do_DELETE(self):  # noqa: N802
        self._run("DELETE")


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
        FakeDaemon.drop = set()
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
                        # 燈號是 daemon 算好的 lamp。舊 daemon 可能還帶 team／teams，要被忽略。
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
        self.assertIsNone(out["bots"][0]["queued_turns"], "`/api/state` 只給下一筆 turn：知道 ≥1，不知道幾筆")
        self.assertEqual(out["bots"][0]["lamp"], "busy", "daemon 算好的照抄，不自己推")
        self.assertNotIn("team", out["bots"][0])
        self.assertTrue(out["bots"][0]["is_manager"])
        self.assertEqual(set(out), {"projects", "bots", "manager_bot_id"})

    def test_state_bot_without_queue_or_teams(self):
        out = agm.slim_state({"projects": [{"id": "p1", "bots": [{"id": "b1", "lamp": "idle", "queued_turn": None}]}]})
        self.assertIsNone(out["bots"][0]["queued_turn"])
        self.assertEqual(out["bots"][0]["queued_turns"], 0, "明講 null 就是 0，不是不知道")
        self.assertEqual(set(out), {"projects", "bots"})

    def test_state_reads_the_shape_the_supervisor_endpoint_really_returns(self):
        """issue #514：走的是 `/api/supervisor/state`，它沒有 `lamp`、`queued_turn` 叫 `queued_turns`。

        這份 fixture 照 `supervisor::sanitized_state` 真的會給的欄位抄，`/api/state` 的那份留在上面——
        兩種形狀各一份，欄位名之後再漂掉就會在這裡紅。
        """
        out = agm.slim_state(
            {
                "supervisor_id": "agm",
                "projects": [{"id": "p1", "label": "AG Man", "path": "/x", "host": "local"}],
                "bots": [
                    {
                        "id": "b1",
                        "project_id": "p1",
                        "name": "AM-1-L",
                        "kind": "claude",
                        "model": "claude-opus-5-5",
                        "effort": "low",
                        "identity": "cc0",
                        "managed_by": "user",
                        "parent_bot_id": None,
                        "is_supervisor": False,
                        "supervisor_role": None,
                        "cwd": "/x",
                        "host": "local",
                        "host_connected": True,
                        "queued_turns": 2,
                        "asleep": None,
                        # daemon 算好的（`bot_connected` 比 CLI 看得到的 host_connected 細）。
                        "lamp": "working",
                        "run": {
                            "id": "r1",
                            "state": "running",
                            "agent_status": "working",
                            "agent_title": "在跑測試",
                            "native_session_id": "sess-1",
                            "runtime_model": "claude-opus-5-5",
                            "runtime_effort": "low",
                            "pane_id": "w1:p1",
                            "started_at": "2026-09-24T01:00:00.000Z",
                        },
                    },
                    {
                        "id": "b2",
                        "project_id": "p1",
                        "name": "AM-2",
                        "host_connected": True,
                        "queued_turns": 0,
                        # §6.11 收起來省 RAM：run 是 null，但它叫得醒。
                        "asleep": {"since": "2026-09-24T01:03:25.662Z", "idle_minutes": 92},
                        "run": None,
                    },
                ],
            },
            "b1",
        )
        busy, asleep = out["bots"]
        # `lamp` 這個欄位不存在 → 照 daemon 的 `fn lamp` 從 host_connected + run 算，不是空字串。
        self.assertEqual(busy["lamp"], "working")
        self.assertEqual(busy["queued_turns"], 2)
        self.assertIsNone(busy["queued_turn"], "筆數不塞進 turn 欄位")
        self.assertIs(busy["host_connected"], True)
        self.assertIsNone(busy["asleep"])
        self.assertEqual(busy["run"]["native_session_id"], "sess-1")
        # 睡著的跟停掉的不能長一樣：run 都是 null，差別只在 asleep。
        self.assertEqual(asleep["lamp"], "offline")
        self.assertEqual(asleep["asleep"], {"since": "2026-09-24T01:03:25.662Z", "idle_minutes": 92})
        self.assertEqual(asleep["queued_turns"], 0)

    def test_an_old_daemon_without_a_lamp_field_still_gets_one(self):
        """`/api/supervisor/state` 現在會吐 `lamp`；沒吐的是還沒更新的 daemon，那才輪到 CLI 推。"""
        out = agm.slim_state({"bots": [{"id": "b1", "host_connected": False, "run": {"state": "running", "agent_status": "working"}}]})
        self.assertEqual(out["bots"][0]["lamp"], "disconnected")
        self.assertIs(out["bots"][0]["host_connected"], False)
        # daemon 給了就照抄，不要用比較粗的輸入覆寫它。
        out = agm.slim_state({"bots": [{"id": "b1", "lamp": "blocked", "host_connected": False, "run": None}]})
        self.assertEqual(out["bots"][0]["lamp"], "blocked")

    def test_state_does_not_call_an_unknown_queue_empty(self):
        """舊 daemon 兩個欄位都沒有：回 None（不知道），不是 0（沒有人在排隊）。"""
        out = agm.slim_state({"bots": [{"id": "b1"}]})
        self.assertIsNone(out["bots"][0]["queued_turns"])
        self.assertIsNone(out["bots"][0]["host_connected"])

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

    def test_notice_is_marked_as_one(self):
        """通知不是交辦：daemon 要知道它送到就結案，不用你回頭驗收。"""
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a9", "kind": "notice"})
        self.ok("assign", "--bot", "b1", "--text", "收到，進 idle", "--request-id", "n-1", "--notice")
        body = [r["body"] for r in FakeDaemon.seen if r["method"] == "POST"][0]
        self.assertEqual(body["kind"], "notice")
        self.assertIs(body["expects_review"], False)

    def test_a_handover_reply_says_so_and_an_unmarked_one_does_not(self):
        """寄件端明講才算回覆（review 2026-09-16 H1）：`--ack`／`--reply-to` 要進 body；沒帶就不能多出這兩個欄位。"""
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"kind": "handover", "routed": "patrol", "wake": False})
        self.ok("assign", "--bot", "b1", "--text", "收到，我接手", "--request-id", "h-1", "--notice", "--ack", "--reply-to", "ev-7")
        self.ok("assign", "--bot", "b1", "--text", "需要使用者裁示", "--request-id", "h-2", "--notice")
        posted = [r["body"] for r in FakeDaemon.seen if r["method"] == "POST"]
        self.assertIs(posted[0]["ack"], True)
        self.assertEqual(posted[0]["reply_to"], "ev-7")
        self.assertNotIn("ack", posted[1])
        self.assertNotIn("reply_to", posted[1])

    def test_without_the_flag_nothing_changes(self):
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a8"})
        self.ok("assign", "--bot", "b1", "--text", "做這個", "--request-id", "t-1")
        body = [r["body"] for r in FakeDaemon.seen if r["method"] == "POST"][0]
        self.assertNotIn("kind", body)
        self.assertNotIn("expects_review", body)

    def test_timeout_reports_delivery_unknown_and_sends_once(self):
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a1"})
        FakeDaemon.slow = {"/api/supervisor/assignments"}
        err = self.bad("--timeout", "0.3", "assign", "--bot", "b1", "--text", "x", "--request-id", "req-9")
        self.assertEqual(err["error"], "delivery_unknown")
        self.assertEqual(err["client_request_id"], "req-9")
        posts = [r for r in FakeDaemon.seen if r["path"] == "/api/supervisor/assignments"]
        self.assertEqual(len(posts), 1, "逾時不能自己重送——那會派出第二份同樣的工")

    def test_a_non_utf8_text_file_is_a_bad_arg_not_a_traceback(self):
        f = Path(self.dir.name) / "bin.txt"
        f.write_bytes(b"\xff\xfe not utf8 \x80")
        err = self.bad("assign", "--bot", "b1", "--text-file", str(f), "--request-id", "r-bin")
        self.assertEqual(err["error"], "bad_args")
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [], "沒送出任何請求")

    def test_a_connection_dropped_after_sending_is_delivery_unknown_not_connect_failed(self):
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a1"})
        FakeDaemon.drop = {"/api/supervisor/assignments"}
        err = self.bad("assign", "--bot", "b1", "--text", "x", "--request-id", "req-drop")
        self.assertEqual(err["error"], "delivery_unknown")
        self.assertEqual(err["client_request_id"], "req-drop")
        posts = [r for r in FakeDaemon.seen if r["path"] == "/api/supervisor/assignments"]
        self.assertEqual(len(posts), 1, "不能自己重送")

    def test_oversized_text_is_refused_before_any_request(self):
        err = self.bad("assign", "--bot", "b1", "--text", "x" * (agm.MAX_TEXT_CHARS + 1), "--request-id", "r-big")
        self.assertEqual(err["error"], "bad_args")
        self.assertEqual(err["max_chars"], agm.MAX_TEXT_CHARS)
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [])

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
            {
                "assignments": [
                    {"id": "a1", "status": "queued", "client_request_id": "req-1"},
                    {"id": "a2", "status": "completed"},
                    {"id": "a3", "status": "awaiting_review", "turn_status": "completed"},
                    {"id": "a4", "status": "blocked"},
                    {"id": "a5", "status": "quota_blocked", "resume_at": "2026-09-13T01:00:00Z"},
                ]
            },
        )

    def test_filter_by_status(self):
        out = self.ok("assignments", "--status", "queued")
        self.assertEqual([a["id"] for a in out["assignments"]], ["a1"])

    def test_open_includes_awaiting_review_and_blocked(self):
        """回合跑完不等於結案：等驗收與被標阻塞的都還算未結案。"""
        out = self.ok("assignments", "--open")
        # `quota_blocked` 也算未結案：工作沒做完，只是在等額度回來（daemon 會自己重送）。
        self.assertEqual([a["id"] for a in out["assignments"]], ["a1", "a3", "a4", "a5"])
        self.assertEqual(out["awaiting_review"], 1)
        self.assertEqual([a["id"] for a in self.ok("assignments", "--awaiting-review")["assignments"]], ["a3"])

    def test_single_lookup_prefers_the_detail_endpoint(self):
        FakeDaemon.routes["GET /api/supervisor/assignments/a2"] = (
            200,
            {"id": "a2", "status": "completed", "reviews": [{"decision": "accept", "actor": "AGM"}]},
        )
        out = self.ok("assignments", "--id", "a2")
        self.assertEqual(out["reviews"][0]["decision"], "accept")

    def test_single_lookup_falls_back_to_the_list_on_404(self):
        """舊 daemon 沒有 /assignments/{id}：退回清單過濾，不要把 404 當成「查無此筆」。"""
        self.assertEqual(self.ok("assignments", "--id", "req-1")["id"], "a1")

    def test_missing_id_is_not_found(self):
        self.assertEqual(self.bad("assignments", "--id", "nope")["error"], "not_found")

    def test_work_that_keeps_retrying_is_called_out(self):
        """一直重試但從沒送出去的（bot 停了／在忙）最容易看漏：它看起來一直在動。"""
        FakeDaemon.routes["GET /api/supervisor/assignments"] = (
            200,
            {
                "assignments": [
                    {
                        "id": "a9",
                        "status": "queued",
                        "turn_id": None,
                        "attempts": 12,
                        "next_attempt_at": "2026-09-13T01:00:00Z",
                        "error": "bot has no active run",
                        "target_bot_id": "bot-dead",
                    },
                    {"id": "a1", "status": "queued", "turn_id": None, "attempts": 1},
                ]
            },
        )
        out = self.ok("assignments", "--open")
        stuck = out["retrying_undelivered"]
        self.assertEqual([s["id"] for s in stuck], ["a9"], "剛排隊的那筆不算卡住")
        self.assertEqual(stuck[0]["last_error"], "bot has no active run", "要看得到真正的理由，不是 conflict")
        self.assertEqual(stuck[0]["next_attempt_at"], "2026-09-13T01:00:00Z")

    def test_no_retrying_section_when_nothing_is_stuck(self):
        self.assertNotIn("retrying_undelivered", self.ok("assignments", "--open"))


class ReviewCommandTest(CliCase):
    def setUp(self):
        super().setUp()
        FakeDaemon.routes["POST /api/supervisor/assignments/a1/review"] = (
            200,
            {"id": "a1", "status": "completed", "review": {"decision": "accept"}},
        )

    def _body(self):
        return [r for r in FakeDaemon.seen if r["method"] == "POST"][0]["body"]

    def test_accept_carries_actor_reason_and_evidence(self):
        out = self.ok("review", "a1", "--decision", "accept", "--reason", "測試通過", "--evidence", "commit abc")
        self.assertEqual(out["status"], "completed")
        body = self._body()
        self.assertEqual(body["decision"], "accept")
        self.assertEqual(body["reason"], "測試通過")
        self.assertEqual(body["evidence"], "commit abc")
        self.assertEqual(body["actor"], "AGM")
        self.assertEqual(body["source"], "cli")

    def test_followup_needs_text_and_a_stable_request_id(self):
        """續作是新的一筆交辦，不是改寫已經送出去的字，所以它要自己的冪等鍵。"""
        self.assertEqual(self.bad("review", "a1", "--decision", "followup")["error"], "bad_args")
        self.assertEqual(
            self.bad("review", "a1", "--decision", "followup", "--followup-text", "把 A 做完")["error"],
            "bad_args",
        )
        # 兩次都在送出任何請求之前就擋下來。
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [])
        self.ok(
            "review",
            "a1",
            "--decision",
            "followup",
            "--followup-text",
            "把 A 做完",
            "--followup-request-id",
            "agm-follow-1",
        )
        body = self._body()
        self.assertEqual(body["followup_text"], "把 A 做完")
        self.assertEqual(body["followup_request_id"], "agm-follow-1")

    def test_timeout_reports_delivery_unknown_without_retrying(self):
        FakeDaemon.slow.add("/api/supervisor/assignments/a1/review")
        code, _out, err = self.run_cli("--timeout", "0.2", "review", "a1", "--decision", "accept")
        self.assertEqual(code, 7)
        self.assertEqual(json.loads(err)["error"], "delivery_unknown")
        self.assertEqual(len([r for r in FakeDaemon.seen if r["method"] == "POST"]), 1, "逾時不自動重試")

    def test_unknown_decision_is_rejected_by_the_parser(self):
        code, _out, _err = self.run_cli("review", "a1", "--decision", "looks-fine")
        self.assertEqual(code, 2)


class ApprovalLeaseCommandTest(CliCase):
    def setUp(self):
        super().setUp()
        FakeDaemon.routes["POST /api/supervisor/approvals"] = (200, {"id": "ap-1", "status": "pending"})
        FakeDaemon.routes["GET /api/supervisor/approvals"] = (200, {"approvals": [{"id": "ap-1", "status": "approved"}]})
        FakeDaemon.routes["POST /api/supervisor/approvals/ap-1/decide"] = (200, {"id": "ap-1", "status": "approved"})
        FakeDaemon.routes["GET /api/supervisor/maintenance/safety"] = (200, {"safe": True, "working": []})
        FakeDaemon.routes["POST /api/supervisor/leases/rebuild/acquire"] = (200, {"lease": {"fence": 3}})
        FakeDaemon.routes["POST /api/supervisor/leases/rebuild/release"] = (200, {"released": True})

    def _last_body(self):
        return [r for r in FakeDaemon.seen if r["method"] == "POST"][-1]["body"]

    def test_request_records_who_what_and_which_commit(self):
        self.assertEqual(self.ok("approval", "request", "--requester", "bot-a", "--purpose", "rebuild",
                                 "--scope", "daemon/", "--commit", "abc123", "--expires-in", "600")["id"], "ap-1")
        body = self._last_body()
        self.assertEqual(
            (body["requester"], body["purpose"], body["target_commit"], body["expires_in_secs"]),
            ("bot-a", "rebuild", "abc123", 600),
        )

    def test_request_can_supersede_its_own_earlier_request(self):
        """換 commit 重新申請：帶上舊的 id，daemon 才接得起等待（SPEC §18.10）；不帶就不送這個欄位。"""
        self.ok("approval", "request", "--requester", "bot-a", "--purpose", "rebuild", "--scope", "daemon/",
                "--commit", "def456", "--supersedes", "ap-0")
        self.assertEqual(self._last_body()["supersedes"], "ap-0")
        self.ok("approval", "request", "--requester", "bot-a", "--purpose", "rebuild", "--scope", "daemon/", "--commit", "def456")
        self.assertNotIn("supersedes", self._last_body())

    def test_request_needs_its_fields(self):
        self.assertEqual(self.bad("approval", "request", "--requester", "bot-a")["error"], "bad_args")
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [], "缺欄位時不送出請求")

    def test_decide_carries_the_actor(self):
        self.ok("approval", "decide", "ap-1", "--decision", "approve", "--reason", "沒有人在跑")
        body = self._last_body()
        self.assertEqual((body["decision"], body["actor"], body["reason"]), ("approve", "AGM", "沒有人在跑"))

    def test_acquire_needs_an_approval_and_defaults_to_requiring_idle(self):
        """租約不是「我覺得可以」：沒有核准 id 就不該送出。"""
        self.assertEqual(self.bad("lease", "acquire", "rebuild")["error"], "bad_args")
        self.ok("lease", "acquire", "rebuild", "--approval", "ap-1", "--commit", "abc123", "--owner", "bot-a")
        body = self._last_body()
        self.assertEqual((body["approval_id"], body["commit"], body["owner"]), ("ap-1", "abc123", "bot-a"))
        self.assertIs(body["require_idle"], True)

    def test_renew_and_release_need_the_fence(self):
        """fence 是防舊持有人的那道鎖，缺了就不要送。"""
        self.assertEqual(self.bad("lease", "release", "rebuild", "--owner", "bot-a")["error"], "bad_args")
        self.ok("lease", "release", "rebuild", "--owner", "bot-a", "--fence", "3")
        self.assertEqual(self._last_body()["fence"], 3)

    def test_safety_is_a_plain_read(self):
        self.assertIs(self.ok("lease", "safety")["safe"], True)
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [], "等窗口不會改到任何狀態")

    def test_safety_forwards_repeated_exclusions_as_a_query(self):
        self.ok("lease", "safety", "--exclude-bot", "builder", "--exclude-bot", "manager")
        reads = [r for r in FakeDaemon.seen if r["method"] == "GET" and "/maintenance/safety" in r["path"]]
        self.assertEqual(len(reads), 1)
        self.assertEqual(urllib.parse.parse_qs(urllib.parse.urlparse(reads[0]["path"]).query),
                         {"exclude": ["builder,manager"]})
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [])
        self.ok("lease", "acquire", "rebuild", "--approval", "ap-1",
                "--exclude-bot", "builder", "--exclude-bot", "manager")
        self.assertEqual(self._last_body()["exclude_bot_ids"], ["builder", "manager"])


class PersonaCommandTest(CliCase):
    def setUp(self):
        super().setUp()
        FakeDaemon.routes["GET /api/supervisor/persona"] = (
            200,
            {
                "stored": {"version": 3, "hash": "fnv1a64:abc", "source": "api", "text": "你是 AGM。（全文）"},
                "embedded": {"hash": "fnv1a64:def"},
                "loaded": {"status": "unverified"},
                "upgrade_available": True,
                "needs_restart": False,
            },
        )
        FakeDaemon.routes["PUT /api/supervisor/persona"] = (200, {"stored": {"version": 4}})
        FakeDaemon.routes["POST /api/supervisor/persona/adopt-embedded"] = (200, {"changed": True, "version": 5})

    def test_show_hides_the_full_text_unless_asked(self):
        """人設很長，預設不要整段倒進總管的對話紀錄裡。"""
        out = self.ok("persona", "show")
        self.assertNotIn("text", out["stored"])
        self.assertEqual(out["stored"]["version"], 3)
        self.assertIn("text", self.ok("persona", "show", "--full")["stored"])

    def test_show_never_claims_the_session_loaded_it(self):
        out = self.ok("persona", "show")
        self.assertEqual(out["loaded"]["status"], "unverified")
        self.assertNotEqual(out["loaded"]["status"], "verified")

    def test_set_needs_text_and_passes_the_expected_version(self):
        self.assertEqual(self.bad("persona", "set")["error"], "bad_args")
        self.ok("persona", "set", "--text", "新版人設", "--expected-version", "3")
        body = [r for r in FakeDaemon.seen if r["method"] == "PUT"][-1]["body"]
        self.assertEqual((body["text"], body["expected_version"]), ("新版人設", 3))

    def test_adopt_embedded_is_explicit(self):
        """內嵌版只會透過這支明確的遷移覆蓋持久版，不會是 setup 的副作用。"""
        self.assertEqual(self.ok("persona", "adopt-embedded", "--reason", "跟上新版")["version"], 5)
        body = [r for r in FakeDaemon.seen if r["method"] == "POST"][-1]["body"]
        self.assertEqual((body["actor"], body["reason"]), ("AGM", "跟上新版"))

    def test_old_daemon_says_unsupported(self):
        FakeDaemon.routes.pop("GET /api/supervisor/persona")
        self.assertEqual(self.bad("persona", "show")["error"], "unsupported")


class RemoteCommandTest(CliCase):
    def setUp(self):
        super().setUp()
        FakeDaemon.routes["GET /api/supervisor/remote"] = (
            200,
            {"status": "requested", "source": "argv", "capability": {"status": "unsupported"}},
        )
        FakeDaemon.routes["POST /api/supervisor/remote"] = (200, {"status": "verified"})

    def test_show_reports_the_capability_limit_rather_than_a_connection(self):
        out = self.ok("remote", "show")
        self.assertEqual(out["capability"]["status"], "unsupported")
        self.assertEqual(out["status"], "requested")
        self.assertNotEqual(out["status"], "active")

    def test_claiming_it_works_needs_an_actor(self):
        """「通了」是一個人的宣稱，就要留下是誰宣稱的。"""
        self.assertEqual(self.bad("remote", "observe", "--status", "verified")["error"], "bad_args")
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [], "缺 actor 時不送出")
        self.ok("remote", "observe", "--status", "verified", "--actor", "tony", "--evidence", "手機連過")
        body = [r for r in FakeDaemon.seen if r["method"] == "POST"][-1]["body"]
        self.assertEqual((body["status"], body["source"], body["actor"]), ("verified", "manual", "tony"))

    def test_argv_is_not_an_accepted_source(self):
        code, _out, _err = self.run_cli("remote", "observe", "--source", "argv", "--actor", "tony")
        self.assertEqual(code, 2, "argv 是 daemon 自己的紀錄，不能拿來當觀測來源")


class BuildInputsCommandTest(CliCase):
    def test_lists_the_embedded_paths(self):
        """例行更新判斷「只動到 docs」要吃這份清單，不然 persona 改了會被當成不必重建。"""
        FakeDaemon.routes["GET /api/supervisor/build-inputs"] = (
            200,
            {"paths": ["daemon", "web", "docs/goals/agm-supervisor-persona.md", "scripts/agm.py"]},
        )
        self.assertIn("docs/goals/agm-supervisor-persona.md", self.ok("build-inputs")["paths"])

    def test_old_daemon_says_unsupported(self):
        self.assertEqual(self.bad("build-inputs")["error"], "unsupported")


class IncidentsCommandTest(CliCase):
    def test_lists_open_incidents(self):
        FakeDaemon.routes["GET /api/supervisor/incidents"] = (
            200,
            {"incidents": [{"kind": "host_disconnected", "resource": "mac2"}], "open": 1},
        )
        self.assertEqual(self.ok("incidents")["open"], 1)
        self.ok("incidents", "--all")
        self.assertIn("all=1", FakeDaemon.seen[-1]["path"])

    def test_old_daemon_says_unsupported_rather_than_pretending(self):
        self.assertEqual(self.bad("incidents")["error"], "unsupported")


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

    def test_ack_without_a_role_identity_says_why(self):
        """issue #432：ack 只有 AGM 角色做得到。403 要講「去角色的 pane 裡跑」，不是丟一個 http_error。"""
        FakeDaemon.routes["POST /api/supervisor/inbox/e1/ack"] = (
            403,
            {"error": "forbidden", "reason": "role_required", "message": "這支只給 AGM 角色"},
        )
        err = self.bad("ack", "e1")
        self.assertEqual(err["error"], "role_required")
        self.assertIn("pane", err["message"])
        self.assertIn("AM_HOOK_TOKEN", err["message"])
        # 另一個 reason 的下一步不一樣：人已經在角色 pane 裡了，別再叫他換 pane（i407 審出來的）。
        FakeDaemon.routes["POST /api/supervisor/inbox/e3/ack"] = (
            403,
            {"error": "forbidden", "reason": "bot_proof_mismatch", "message": "對不上"},
        )
        err = self.bad("ack", "e3")
        self.assertEqual(err["error"], "bot_proof_mismatch")
        self.assertIn("token", err["message"])
        self.assertNotIn("去角色", err["message"])
        self.assertNotIn("裡跑 bin/agm", err["message"])
        # 其他 403 不要被這條吃掉：照原本的 http_error 冒出來。
        FakeDaemon.routes["POST /api/supervisor/inbox/e2/ack"] = (403, {"error": "forbidden", "reason": "something_else"})
        self.assertEqual(self.bad("ack", "e2")["error"], "http_error")

    def test_ops_alert_and_approval_lookup_by_id(self):
        """排程腳本卡住時喊得到人，而且查得到被清單擠掉的那筆核准（review 2026-09-16 c1 M1）。"""
        FakeDaemon.routes["POST /api/supervisor/ops-alerts"] = (200, {"queued": True, "inbox_event_id": "e9"})
        out = self.ok("ops-alert", "--source", "daemon-update-kick", "--reason", "stale_lock", "--detail", "鎖清不掉")
        self.assertEqual(out["inbox_event_id"], "e9")
        body = [r["body"] for r in FakeDaemon.seen if r["method"] == "POST"][-1]
        self.assertEqual((body["source"], body["reason"], body["detail"]), ("daemon-update-kick", "stale_lock", "鎖清不掉"))

        # 清單只回最新 100 筆，舊的那筆要用 `--id` 查（query string 走在同一支端點上）。
        FakeDaemon.routes["GET /api/supervisor/approvals"] = (200, {"approvals": [{"id": "ap-1", "status": "approved"}]})
        self.assertEqual(self.ok("approval", "list", "--id", "ap-1")["approvals"][0]["status"], "approved")
        self.assertEqual([r["path"] for r in FakeDaemon.seen if r["method"] == "GET"][-1], "/api/supervisor/approvals?id=ap-1")

    def test_release_triage_submit_show_dispatched(self):
        FakeDaemon.routes["POST /api/release-triage/verdicts"] = (200, {"status": "judged"})
        FakeDaemon.routes["GET /api/release-triage"] = (200, {"rows": []})
        FakeDaemon.routes["POST /api/release-triage/dispatched"] = (200, {"dispatched": 2})
        f = Path(self.dir.name) / "verdicts.json"
        f.write_text(json.dumps({"kind": "claude", "version": "2.1.277", "verdicts": [], "issues": []}), encoding="utf-8")
        self.assertEqual(self.ok("release-triage", "submit", "--file", str(f))["status"], "judged")
        posted = [r["body"] for r in FakeDaemon.seen if r["path"] == "/api/release-triage/verdicts"][-1]
        self.assertEqual((posted["kind"], posted["version"]), ("claude", "2.1.277"))
        self.ok("release-triage", "show", "--kind", "claude", "--version", "2.1.277")
        self.assertEqual([r["path"] for r in FakeDaemon.seen if r["method"] == "GET"][-1], "/api/release-triage?kind=claude&version=2.1.277")
        out = self.ok("release-triage", "dispatched", "--kind", "claude", "--version", "2.1.277", "--version", "2.1.278")
        self.assertEqual(out["dispatched"], 2)
        self.assertEqual([r["body"] for r in FakeDaemon.seen if r["path"] == "/api/release-triage/dispatched"][-1]["versions"], ["2.1.277", "2.1.278"])
        self.assertEqual(self.bad("release-triage", "submit")["error"], "bad_args")
        f.write_text("not json", encoding="utf-8")
        self.assertEqual(self.bad("release-triage", "submit", "--file", str(f))["error"], "bad_args")

    def test_supervisor_actions(self):
        for act in ("setup", "start", "stop", "fallback"):
            FakeDaemon.routes[f"POST /api/supervisor/{act}"] = (200, {"status": act})
            self.assertEqual(self.ok(f"supervisor-{act}")["status"], act)

    def test_bot_create_needs_project_and_name(self):
        self.assertEqual(self.bad("bot", "create")["error"], "bad_args")

    def test_bot_set_patches_only_supplied_fields_and_preserves_daemon_response(self):
        response = {"needs_restart": True, "live_apply": {"applied": False, "reason": "busy"}}
        FakeDaemon.routes["PATCH /api/bots/b1"] = (200, response)
        self.assertEqual(self.ok("bot", "set", "b1", "--model", "claude-opus-5-5", "--identity", "cc1"), response)
        self.assertEqual(FakeDaemon.seen[-1]["method"], "PATCH")
        self.assertEqual(FakeDaemon.seen[-1]["path"], "/api/bots/b1")
        self.assertEqual(FakeDaemon.seen[-1]["body"], {"model": "claude-opus-5-5", "identity": "cc1"})

    def test_bot_set_sends_only_effort_when_that_is_the_only_option(self):
        FakeDaemon.routes["PATCH /api/bots/b1"] = (200, {"needs_restart": False})
        self.ok("bot", "set", "b1", "--effort", "high")
        self.assertEqual(FakeDaemon.seen[-1]["body"], {"effort": "high"})

    def test_bot_set_http_errors_are_structured_for_client_and_server_errors(self):
        for status in (422, 503):
            with self.subTest(status=status):
                FakeDaemon.routes["PATCH /api/bots/b1"] = (status, {"error": "rejected", "reason": "invalid setting"})
                err = self.bad("bot", "set", "b1", "--model", "opus")
                self.assertEqual(err["error"], "http_error")
                self.assertEqual(err["status"], status)
                self.assertEqual(err["detail"]["reason"], "invalid setting")

    def test_bot_set_requires_at_least_one_field(self):
        self.assertEqual(self.bad("bot", "set", "b1")["error"], "bad_args")

    def test_bot_delete_uses_http_delete(self):
        FakeDaemon.routes["DELETE /api/bots/b9"] = (200, {"removed_children": []})
        self.assertEqual(self.ok("bot", "delete", "b9"), {"removed_children": []})
        self.assertEqual([r["method"] for r in FakeDaemon.seen if r["path"] == "/api/bots/b9"], ["DELETE"])
        self.assertEqual(self.bad("bot", "delete")["error"], "bad_args")

    def test_bot_delete_confirm_supervisor_goes_in_the_query_and_every_call_names_agm(self):
        FakeDaemon.routes["DELETE /api/bots/build"] = (200, {"removed_children": []})
        self.ok("bot", "delete", "build", "--confirm-supervisor")
        self.assertEqual(FakeDaemon.seen[-1]["path"], "/api/bots/build?confirm=supervisor")
        self.assertEqual(FakeDaemon.seen[-1]["caller"], "agm")
        self.ok("bot", "delete", "build")
        self.assertEqual(FakeDaemon.seen[-1]["path"], "/api/bots/build", "沒帶旗標就不帶 confirm")

    def test_bot_delete_conflict_is_a_structured_error(self):
        FakeDaemon.routes["DELETE /api/bots/tm"] = (409, {"error": "conflict", "reason": "all bots must be stopped first"})
        err = self.bad("bot", "delete", "tm")
        self.assertEqual(err["status"], 409)
        self.assertEqual(err["detail"]["reason"], "all bots must be stopped first")

    def test_bot_restart(self):
        FakeDaemon.routes["POST /api/bots/b1/restart"] = (200, {"run_id": "r1"})
        self.assertEqual(self.ok("bot", "restart", "b1")["run_id"], "r1")
        self.assertEqual(FakeDaemon.seen[-1]["path"], "/api/bots/b1/restart", "沒帶 --resume 就沒有 query")

    def test_bot_restart_resume_native_goes_in_the_query_and_the_answer_is_passed_through(self):
        FakeDaemon.routes["POST /api/bots/b1/restart"] = (200, {"run_id": "r1", "resumed": True, "session_id": "s-9", "resume_outcome": "native"})
        out = self.ok("bot", "restart", "b1", "--resume", "native")
        self.assertEqual(FakeDaemon.seen[-1]["path"], "/api/bots/b1/restart?resume=native")
        self.assertEqual(FakeDaemon.seen[-1]["body"], {})
        self.assertEqual((out["resumed"], out["session_id"]), (True, "s-9"))
        FakeDaemon.routes["POST /api/bots/b1/start"] = (200, {"resumed": True})
        self.ok("bot", "start", "b1", "--resume", "native")
        self.assertEqual(FakeDaemon.seen[-1]["path"], "/api/bots/b1/start?resume=native")

    def test_bot_resume_native_conflict_is_a_structured_error(self):
        FakeDaemon.routes["POST /api/bots/b1/restart"] = (409, {"error": "resume_failed", "resumed": False, "session_id": "s-9"})
        err = self.bad("bot", "restart", "b1", "--resume", "native")
        self.assertEqual(err["status"], 409)
        self.assertEqual((err["detail"]["resumed"], err["detail"]["session_id"]), (False, "s-9"))

    def test_bot_resume_with_an_explicit_session_goes_in_the_query(self):
        FakeDaemon.routes["POST /api/bots/b1/start"] = (200, {"resumed": True, "session_id": "246fcf93-af39"})
        out = self.ok("bot", "start", "b1", "--resume", "native", "--session", "246fcf93-af39")
        self.assertEqual(FakeDaemon.seen[-1]["path"], "/api/bots/b1/start?resume=native&session=246fcf93-af39")
        self.assertEqual(out["session_id"], "246fcf93-af39")
        self.assertNotEqual(self.run_cli("bot", "start", "b1", "--session", "246fcf93-af39")[0], 0, "沒有 --resume 不收 --session")

    def test_bot_restore_posts_to_the_restore_route(self):
        FakeDaemon.routes["POST /api/bots/kid/restore"] = (200, {"bot": {"id": "kid"}})
        self.assertEqual(self.ok("bot", "restore", "kid")["bot"]["id"], "kid")
        self.assertEqual((FakeDaemon.seen[-1]["path"], FakeDaemon.seen[-1]["body"]), ("/api/bots/kid/restore", {}))
        FakeDaemon.routes["POST /api/bots/kid/restore"] = (409, {"error": "conflict", "reason": "bot is not deleted"})
        self.assertEqual(self.bad("bot", "restore", "kid")["status"], 409)

    def test_bot_resume_rejects_other_ops_and_modes(self):
        self.assertNotEqual(self.run_cli("bot", "stop", "b1", "--resume", "native")[0], 0)
        self.assertNotEqual(self.run_cli("bot", "restart", "b1", "--resume", "fresh")[0], 0)

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


class DualRoleTest(CliCase):
    """AGM 雙角色（SPEC §18.15）：CLI 知道自己是哪個角色，身分靠 pane 的 bot token，不靠字串。"""

    def setUp(self):
        super().setUp()
        for k in ("AM_BOT_ID", "AM_HOOK_TOKEN"):
            old = os.environ.pop(k, None)
            self.addCleanup(lambda k=k, v=old: os.environ.__setitem__(k, v) if v is not None else os.environ.pop(k, None))
        self.write_runtime({
            "daemon_url": f"http://127.0.0.1:{self.port}", "manager_bot_id": "bot-agm",
            "role": "responder", "self_bot_id": "bot-resp",
        })

    def last(self, method: str, prefix: str) -> dict:
        return [r for r in FakeDaemon.seen if r["method"] == method and r["path"].startswith(prefix)][-1]

    def test_the_role_token_rides_along_only_from_its_own_pane(self):
        FakeDaemon.routes["POST /api/supervisor/inbox/e1/ack"] = (200, {})
        os.environ["AM_BOT_ID"], os.environ["AM_HOOK_TOKEN"] = "bot-resp", "hook-secret"
        self.ok("ack", "e1")
        seen = self.last("POST", "/api/supervisor/inbox/e1/ack")
        self.assertEqual((seen["bot_id"], seen["bot_token"]), ("bot-resp", "hook-secret"))
        # 同一支 CLI 在別顆 bot 的 pane 裡跑：不帶那顆 bot 的 token，也就冒充不了協調者。
        os.environ["AM_BOT_ID"] = "bot-worker"
        self.ok("ack", "e1")
        seen = self.last("POST", "/api/supervisor/inbox/e1/ack")
        self.assertIsNone(seen["bot_token"])
        self.assertNotIn("hook-secret", json.dumps(self.ok("whoami")))

    def test_whoami_and_inbox_mine_follow_the_runtime_role(self):
        self.assertEqual(self.ok("whoami")["role"], "responder")
        FakeDaemon.routes["GET /api/supervisor/inbox"] = (200, {"events": []})
        self.ok("inbox", "--role", "mine")
        q = urllib.parse.parse_qs(urllib.parse.urlparse(self.last("GET", "/api/supervisor/inbox")["path"]).query)
        self.assertEqual(q["role"], ["responder"])
        self.write_runtime({"daemon_url": f"http://127.0.0.1:{self.port}", "manager_bot_id": "bot-agm"})
        self.assertEqual(self.ok("whoami")["role"], "patrol", "舊的 runtime.json 就是巡檢")

    def test_responder_lifecycle_and_review_role(self):
        for op in ("setup", "start", "stop"):
            FakeDaemon.routes[f"POST /api/supervisor/responder/{op}"] = (200, {"status": op})
        FakeDaemon.routes["GET /api/supervisor/responder"] = (200, {"configured": True})
        self.assertTrue(self.ok("responder", "show")["configured"])
        self.ok("responder", "setup", "--model", "opus", "--effort", "high")
        self.assertEqual(self.last("POST", "/api/supervisor/responder/setup")["body"], {"model": "opus", "effort": "high"})
        self.ok("responder", "start")
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a1"})
        self.ok("assign", "--bot", "b1", "--text", "清 chrome", "--request-id", "gc-1", "--review-by", "patrol")
        self.assertEqual(self.last("POST", "/api/supervisor/assignments")["body"]["review_role"], "patrol")
        self.ok("assign", "--bot", "b1", "--text", "x", "--request-id", "r-2")
        self.assertNotIn("review_role", self.last("POST", "/api/supervisor/assignments")["body"])

    def test_a_handover_receipt_is_not_reported_as_a_failed_assignment(self):
        """交辦給另一個角色時 daemon 回的是佇列收據（沒有 assignment id）。CLI 不能因為少了 id
        就當成派工失敗——那會讓 AGM 以為交接沒送到而一直重送。"""
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (
            200,
            {"kind": "handover", "routed": "patrol", "queued": True, "duplicate": False, "wake": True,
             "inbox_event_id": "e1", "delivery": "queued", "turn_id": None, "message_id": None},
        )
        code, out, err = self.run_cli("assign", "--bot", "bot-agm", "--text", "交接：builder 卡住", "--request-id", "h-1")
        self.assertEqual(code, 0, f"stderr={err}")
        body = json.loads(out)
        self.assertEqual(body["kind"], "handover")
        self.assertEqual(body["inbox_event_id"], "e1")
        self.assertNotIn("error", body)

    def test_reports_are_attributed_to_this_role_not_the_patrol(self):
        FakeDaemon.routes["POST /api/missions/m1/events"] = (200, {"id": "e1"})
        self.ok("mission", "event", "m1", "--kind", "note", "--text", "已核准")
        self.assertEqual(self.last("POST", "/api/missions/m1/events")["body"]["relay_from"], "bot-resp")

    def test_responder_persona_goes_to_its_own_endpoint(self):
        FakeDaemon.routes["PUT /api/supervisor/responder/persona"] = (200, {"role": "responder"})
        self.ok("persona", "set", "--role", "responder", "--text", "你是 AGM 的協調者")
        self.assertEqual(self.last("PUT", "/api/supervisor/responder/persona")["body"], {"text": "你是 AGM 的協調者"})
        self.assertEqual(self.bad("persona", "adopt-embedded", "--role", "responder")["error"], "bad_args")


class MissionCommandTest(CliCase):
    """群組任務：每個 op 打對一個端點，回報預設標成總管說的。"""

    def posts(self, path: str) -> list:
        return [r for r in FakeDaemon.seen if r["method"] == "POST" and r["path"] == path]

    def test_list_needs_project_and_passes_status(self):
        self.assertEqual(self.bad("mission", "list")["error"], "bad_args")
        FakeDaemon.routes["GET /api/projects/p1/missions"] = (200, {"missions": [{"id": "m1"}]})
        out = self.ok("mission", "list", "--project", "p1", "--status", "done", "--limit", "5")
        self.assertEqual(out["missions"][0]["id"], "m1")
        gets = [r["path"] for r in FakeDaemon.seen if r["method"] == "GET" and r["path"].startswith("/api/projects/")]
        q = urllib.parse.parse_qs(urllib.parse.urlparse(gets[0]).query)
        self.assertEqual(q, {"status": ["done"], "limit": ["5"]})

    def test_get_and_events(self):
        FakeDaemon.routes["GET /api/missions/m1"] = (200, {"id": "m1", "phase": "executing", "events": [{"kind": "instruction"}]})
        self.assertEqual(self.ok("mission", "get", "m1")["phase"], "executing")
        self.assertEqual(self.ok("mission", "events", "m1"), {"mission_id": "m1", "events": [{"kind": "instruction"}]})
        self.assertEqual(self.bad("mission", "get")["error"], "bad_args", "沒給 mission id 要先擋")

    def test_event_is_attributed_to_the_manager_by_default(self):
        FakeDaemon.routes["POST /api/missions/m1/events"] = (200, {"id": "e1"})
        self.ok("mission", "event", "m1", "--kind", "verified", "--text", "cargo test 644 passed", "--sha", "abc1234")
        body = self.posts("/api/missions/m1/events")[0]["body"]
        self.assertEqual(body, {"kind": "verified", "text": "cargo test 644 passed", "sha": "abc1234", "relay_from": "bot-agm"})
        self.ok("mission", "event", "m1", "--kind", "note", "--text", "排程", "--as-daemon")
        self.assertEqual(self.posts("/api/missions/m1/events")[1]["body"]["relay_from"], "daemon")

    def test_verified_names_the_commit_it_verified(self):
        """交付只放行驗過的那個 commit：verified 沒帶 --worktree／--sha 就不送（review3 c1 M9）。"""
        FakeDaemon.routes["POST /api/missions/m1/events"] = (200, {"id": "e1"})
        self.assertEqual(self.bad("mission", "event", "m1", "--kind", "verified", "--text", "全過")["error"], "bad_args")
        self.assertEqual(self.posts("/api/missions/m1/events"), [], "缺 commit 時什麼都不送")
        self.ok("mission", "event", "m1", "--kind", "verified", "--text", "全過", "--worktree", "/tmp/wt-exec")
        self.assertEqual(self.posts("/api/missions/m1/events")[0]["body"]["worktree"], "/tmp/wt-exec")

    def test_question_answer_revise_need_an_idempotency_key(self):
        """三支都會改變狀態，沒有穩定的 request id 就不要送——重送會變成第二個問題／第二輪續作。"""
        for op in ("question", "answer", "revise"):
            self.assertEqual(self.bad("mission", op, "m1", "--text", "x")["error"], "bad_args")
            self.assertEqual(self.bad("mission", op, "m1", "--request-id", "r1")["error"], "bad_args")
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [], "缺欄位時什麼都不送")

    def test_question_and_revise_keep_the_manager_as_the_source(self):
        """總管代問／代開續作時要留下來源，不然時間軸上會長得像使用者自己說的。"""
        FakeDaemon.routes["POST /api/missions/m1/question"] = (200, {"event": {"id": "e1"}})
        FakeDaemon.routes["POST /api/missions/m1/revise"] = (200, {"id": "m2"})
        self.ok("mission", "question", "m1", "--text", "這會影響登入嗎", "--request-id", "q1")
        self.assertEqual(self.posts("/api/missions/m1/question")[0]["body"]["relay_from"], "bot-agm")
        self.ok("mission", "revise", "m1", "--text", "順便改標題", "--request-id", "rev1")
        self.assertEqual(self.posts("/api/missions/m1/revise")[0]["body"]["relay_from"], "bot-agm")

    def test_answering_for_the_user_does_not_wear_the_manager_identity(self):
        """明確 --as-user 才代表依使用者指示回答暫停任務。"""
        FakeDaemon.routes["POST /api/missions/m1/answer"] = (200, {"resumed": True})
        self.ok("mission", "answer", "m1", "--text", "照你說的做", "--request-id", "a1", "--as-user")
        self.assertNotIn("relay_from", self.posts("/api/missions/m1/answer")[0]["body"])

    def test_answer_without_reply_to_does_not_silently_claim_to_be_the_user(self):
        FakeDaemon.routes["POST /api/missions/m1/answer"] = (200, {})
        self.ok("mission", "answer", "m1", "--text", "hello", "--request-id", "a1")
        self.assertEqual(self.posts("/api/missions/m1/answer")[0]["body"]["relay_from"], "bot-agm")

    def test_answer_carries_the_manager_identity_when_replying(self):
        """AGM 回覆追問要指回那一則，並用自己的身分；不帶 --reply-to 就是使用者回答暫停那條路。"""
        FakeDaemon.routes["POST /api/missions/m1/answer"] = (200, {"resumed": True})
        self.ok("mission", "answer", "m1", "--text", "不會", "--request-id", "a1", "--reply-to", "e9")
        body = self.posts("/api/missions/m1/answer")[0]["body"]
        self.assertEqual(body["reply_to"], "e9")
        self.assertEqual(body["relay_from"], "bot-agm")

    def test_revise_posts_to_its_own_endpoint(self):
        FakeDaemon.routes["POST /api/missions/m1/revise"] = (200, {"id": "m2", "parent_mission_id": "m1"})
        out = self.ok("mission", "revise", "m1", "--text", "順便改標題", "--request-id", "rev1")
        self.assertEqual(out["parent_mission_id"], "m1", "續作是新的一筆，指回原成果")
        body = self.posts("/api/missions/m1/revise")[0]["body"]
        self.assertEqual((body["text"], body["client_request_id"]), ("順便改標題", "rev1"))

    def test_event_without_kind_or_text_sends_nothing(self):
        self.assertEqual(self.bad("mission", "event", "m1", "--text", "x")["error"], "bad_args")
        self.assertEqual(self.bad("mission", "event", "m1", "--kind", "report", "--text", "  ")["error"], "bad_args")
        self.assertEqual(self.posts("/api/missions/m1/events"), [])

    def test_event_refuses_to_speak_without_a_manager_id(self):
        """沒有 bot id 就不帶來源，daemon 會把它當成使用者本人說的——寧可報錯。"""
        self.write_runtime({"daemon_url": f"http://127.0.0.1:{self.port}"})
        self.assertEqual(self.bad("mission", "event", "m1", "--kind", "report", "--text", "x")["error"], "bad_args")
        self.assertEqual(self.posts("/api/missions/m1/events"), [])

    def test_pause_resume_cancel_round(self):
        for op in ("pause", "resume", "cancel", "round"):
            FakeDaemon.routes[f"POST /api/missions/m1/{op}"] = (200, {"status": op})
        self.assertEqual(self.bad("mission", "pause", "m1")["error"], "bad_args")
        self.ok("mission", "pause", "m1", "--reason", "waiting_user", "--detail", "等使用者選交付方式")
        self.assertEqual(self.posts("/api/missions/m1/pause")[0]["body"], {"reason": "waiting_user", "detail": "等使用者選交付方式"})
        for op in ("resume", "cancel", "round"):
            self.assertEqual(self.ok("mission", op, "m1")["status"], op)

    def test_round_over_the_cap_surfaces_the_conflict(self):
        FakeDaemon.routes["POST /api/missions/m1/round"] = (409, {"error": "conflict", "reason": "max_rounds", "rounds_used": 2})
        err = self.bad("mission", "round", "m1")
        self.assertEqual(err["status"], 409)
        self.assertEqual(err["detail"]["reason"], "max_rounds")

    def test_complete_sends_the_summary_as_the_manager(self):
        FakeDaemon.routes["POST /api/missions/m1/complete"] = (200, {"status": "done"})
        f = Path(self.dir.name) / "summary.md"
        f.write_text("修好了，commit abc123", encoding="utf-8")
        self.ok("mission", "complete", "m1", "--text-file", str(f))
        body = self.posts("/api/missions/m1/complete")[0]["body"]
        self.assertEqual(body, {"result_summary": "修好了，commit abc123", "relay_from": "bot-agm"})

    def test_complete_without_delivery_states_the_reason(self):
        FakeDaemon.routes["POST /api/missions/m1/complete"] = (200, {"status": "done"})
        self.ok("mission", "complete", "m1", "--text", "只查了原因", "--no-delivery", "no_changes", "--worktree", "/tmp/wt")
        body = self.posts("/api/missions/m1/complete")[0]["body"]
        self.assertEqual(
            body,
            {"result_summary": "只查了原因", "no_delivery": "no_changes", "worktree": "/tmp/wt", "relay_from": "bot-agm"},
        )
        # 理由只收兩種，打錯字在 CLI 就擋下來，不送出一個 daemon 看不懂的值。
        code, _, _ = self.run_cli("mission", "complete", "m1", "--text", "x", "--no-delivery", "later")
        self.assertEqual(code, 2)
        self.assertEqual(len(self.posts("/api/missions/m1/complete")), 1, "打錯的那次沒有送出")

    def test_pick_passes_role_and_exclude(self):
        self.assertEqual(self.bad("mission", "pick", "m1")["error"], "bad_args")
        FakeDaemon.routes["GET /api/missions/m1/pick"] = (200, {"pick": {"decision": "use", "identity": "cc1"}})
        out = self.ok("mission", "pick", "m1", "--role", "reviewer", "--exclude", "cc2")
        self.assertEqual(out["pick"]["identity"], "cc1")
        path = [r["path"] for r in FakeDaemon.seen if r["path"].startswith("/api/missions/m1/pick")][0]
        self.assertEqual(urllib.parse.parse_qs(urllib.parse.urlparse(path).query), {"role": ["reviewer"], "exclude": ["cc2"]})

    def test_deliver_needs_a_worktree(self):
        self.assertEqual(self.bad("mission", "deliver", "m1")["error"], "bad_args")
        FakeDaemon.routes["POST /api/missions/m1/deliver"] = (200, {"mode": "push_main", "sha": "abc"})
        self.ok("mission", "deliver", "m1", "--worktree", "/tmp/wt", "--title", "修錯字")
        body = self.posts("/api/missions/m1/deliver")[0]["body"]
        self.assertEqual(body, {"worktree": "/tmp/wt", "title": "修錯字", "relay_from": "bot-agm"})

    def test_deliver_failure_is_a_non_zero_structured_error(self):
        FakeDaemon.routes["POST /api/missions/m1/deliver"] = (409, {"error": "conflict", "reason": "not_fast_forward"})
        err = self.bad("mission", "deliver", "m1", "--worktree", "/tmp/wt")
        self.assertEqual(err["detail"]["reason"], "not_fast_forward")


class AssignMissionFlagsTest(CliCase):
    def test_mission_and_role_go_into_the_body(self):
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a1", "mission_id": "m1", "role": "executor"})
        self.ok("assign", "--bot", "b1", "--text", "做 X", "--request-id", "r1", "--mission", "m1", "--role", "executor")
        body = [r["body"] for r in FakeDaemon.seen if r["method"] == "POST"][0]
        self.assertEqual(body["mission_id"], "m1")
        self.assertEqual(body["role"], "executor")

    def test_one_without_the_other_is_refused_before_sending(self):
        self.assertEqual(self.bad("assign", "--bot", "b1", "--text", "x", "--request-id", "r", "--mission", "m1")["error"], "bad_args")
        self.assertEqual(self.bad("assign", "--bot", "b1", "--text", "x", "--request-id", "r", "--role", "verifier")["error"], "bad_args")
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [])

    def test_plain_assign_stays_unchanged(self):
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a2"})
        self.ok("assign", "--bot", "b1", "--text", "做這個", "--request-id", "t-1")
        body = [r["body"] for r in FakeDaemon.seen if r["method"] == "POST"][0]
        self.assertNotIn("mission_id", body)
        self.assertNotIn("role", body)


class HelpTest(unittest.TestCase):
    def test_help_mentions_the_no_retry_rule(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(SystemExit):
            agm.main(["--help"])
        text = out.getvalue()
        self.assertIn("agm assign", text)
        self.assertIn("不要", text)


class QuotaCommandTest(CliCase):
    def test_plain_quota_only_reads(self):
        FakeDaemon.routes["GET /api/quota"] = (200, {"claude": None})
        self.assertEqual(self.ok("quota"), {"claude": None})
        self.assertEqual([(r["method"], r["path"]) for r in FakeDaemon.seen if r["path"] != "/api/session"], [("GET", "/api/quota")])

    def test_probe_posts_kind_account_and_host(self):
        """#404：statusLine 鎖住的錯值要有手動出口——強制探測、結果直接覆寫 cache。"""
        FakeDaemon.routes["POST /api/quota/probe"] = (200, {"key": "claude", "quota": {"source": "claude-usage"}})
        out = self.ok("quota", "--probe", "--account", "cc0")
        self.assertEqual(out["quota"]["source"], "claude-usage")
        self.ok("quota", "--probe", "--account", "cc1", "--host", "m4p")
        posts = [r for r in FakeDaemon.seen if r["method"] == "POST"]
        self.assertEqual(len(posts), 2)
        self.assertEqual(urllib.parse.parse_qs(urllib.parse.urlsplit(posts[0]["path"]).query), {"kind": ["claude"], "account": ["cc0"]})
        self.assertEqual(
            urllib.parse.parse_qs(urllib.parse.urlsplit(posts[1]["path"]).query),
            {"kind": ["claude"], "account": ["cc1"], "host": ["m4p"]},
        )
        self.assertEqual(posts[0]["token"], TOKEN)

    def test_probe_without_account_asks_for_the_default_account(self):
        FakeDaemon.routes["POST /api/quota/probe"] = (200, {"key": "claude"})
        self.ok("quota", "--probe")
        post = [r for r in FakeDaemon.seen if r["method"] == "POST"][0]
        self.assertEqual(post["path"], "/api/quota/probe?kind=claude")

    def test_probe_failure_surfaces_the_daemon_error(self):
        FakeDaemon.routes["POST /api/quota/probe"] = (404, {"error": "not_found", "what": "identity"})
        err = self.bad("quota", "--probe", "--account", "cc9")
        self.assertIn("identity", json.dumps(err))

    def test_account_without_probe_is_a_usage_error(self):
        err = self.bad("quota", "--account", "cc0")
        self.assertEqual(err["error"], "bad_args")
        self.assertFalse([r for r in FakeDaemon.seen if r["path"] != "/api/session"], "用法錯誤不該打任何 API")


class OpsSyncTest(CliCase):
    """issue #418：已安裝的 ops 腳本跟 repo 比對。真的 git repo，安裝目錄就是 AGM_RUNTIME_DIR。"""

    def git(self, *args: str) -> str:
        import subprocess
        return subprocess.run(["git", "-C", str(self.repo), *args], check=True, capture_output=True, text=True).stdout.strip()

    def put(self, rel: str, text: str) -> None:
        p = self.repo / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")

    def install(self, rel: str, text: str) -> None:
        p = Path(self.dir.name) / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")

    def setUp(self):
        super().setUp()
        # `ops_sync_report` 會掃 `~/Library/LaunchAgents` 找沒有版控的 `com.agm.*`（issue #487）。
        # 每個測試都先指到自己的空目錄：不指的話會讀到這台機器真的 plist，測試結果跟著機器跑。
        self.agents = Path(self.dir.name) / "LaunchAgents"
        self.agents.mkdir(parents=True, exist_ok=True)
        os.environ["AGM_LAUNCHAGENTS_DIR"] = str(self.agents)
        self.addCleanup(os.environ.pop, "AGM_LAUNCHAGENTS_DIR", None)
        self.repo_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.repo_dir.cleanup)
        self.repo = Path(self.repo_dir.name)
        self.git("init", "-q")
        self.git("config", "user.email", "t@t")
        self.git("config", "user.name", "t")
        self.put("scripts/ops/install-manifest.tsv",
                 "# comment\nscripts/ops/a.sh bin/a.sh\nscripts/ops/b.sh bin/b.sh\nscripts/ops/c.sh bin/c.sh\nscripts/ops/t.md t.md\n")
        for n in "abc":
            self.put(f"scripts/ops/{n}.sh", f"{n} v1\n")
        self.put("scripts/ops/t.md", "task\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "v1")
        self.put("scripts/ops/a.sh", "a v2\n")
        self.git("commit", "-qam", "a 改第二版")
        self.put("scripts/ops/a.sh", "a v3\n")
        self.git("commit", "-qam", "a 改第三版")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")

    def test_each_kind_of_gap_is_reported_separately_and_exits_nonzero(self):
        self.install("bin/a.sh", "a v1\n")            # 落後兩個 commit
        self.install("bin/b.sh", "b 有人直接改了\n")    # repo 任何一版都不是
        self.install("bin/c.sh", "c v1\n")            # 最新
        self.install("bin/agm", "cli")                 # daemon 部署的，不算多出來
        self.install("bin/a.sh.bak-20260920", "old")   # 備份不算
        self.install("bin/dev-server-kick.ts", "x")    # 沒有版控
        code, out, err = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1, err)
        r = json.loads(out)
        self.assertFalse(r["in_sync"])
        self.assertEqual([x["target"] for x in r["ok"]], ["bin/c.sh"])
        self.assertEqual([x["target"] for x in r["drift"]], ["bin/b.sh"])
        self.assertEqual([x["target"] for x in r["missing"]], ["t.md"])
        self.assertEqual([x["target"] for x in r["extra"]], ["bin/dev-server-kick.ts"])
        [behind] = r["behind"]
        self.assertEqual((behind["target"], behind["behind"]), ("bin/a.sh", 2))
        self.assertEqual([c.split(" ", 1)[1] for c in behind["commits"]], ["a 改第三版", "a 改第二版"])
        self.assertFalse([x for x in FakeDaemon.seen if x["path"] != "/api/session"], "沒帶 --alert 不打 API")
        # 只讀：安裝檔一個字都沒動。
        self.assertEqual((Path(self.dir.name) / "bin/b.sh").read_text(encoding="utf-8"), "b 有人直接改了\n")

    # ── launchd plist（issue #487）────────────────────────────────────────────
    PLIST = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
        '<plist version="1.0"><dict>{}</dict></plist>\n'
    )

    def plist(self, label: str, interval: int, extra: str = "", program: str = "/bin/zsh") -> str:
        return self.PLIST.format(
            f"<key>Label</key><string>{label}</string>"
            f"<key>ProgramArguments</key><array><string>{program}</string></array>"
            f"<key>StartInterval</key><integer>{interval}</integer>{extra}"
        )

    def with_plists(self):
        """對照表多一列 plist（`self.agents` 在 setUp 已經指到暫存目錄）。"""
        self.put("scripts/ops/install-manifest.tsv",
                 "scripts/ops/a.sh bin/a.sh\nscripts/ops/b.sh bin/b.sh\nscripts/ops/c.sh bin/c.sh\nscripts/ops/t.md t.md\n"
                 "scripts/ops/launchd/com.agm.x.plist LaunchAgents/com.agm.x.plist\n")
        self.put("scripts/ops/launchd/com.agm.x.plist",
                 self.plist("com.agm.x", 1800,
                            extra="<key>StandardOutPath</key><string>/tmp/x.log</string>"
                                  "<key>EnvironmentVariables</key><dict><key>PATH</key><string>/opt/other</string></dict>"))
        self.git("add", "-A")
        self.git("commit", "-qam", "加 plist")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        for n in "abc":
            self.install(f"bin/{n}.sh", "a v3\n" if n == "a" else f"{n} v1\n")
        self.install("t.md", "task\n")

    def test_key_order_and_env_var_values_are_not_drift(self):
        """鍵的順序不影響；`EnvironmentVariables` 的**值**每台機器不同，忽略（但鍵要在，見下一條）。"""
        self.with_plists()
        (self.agents / "com.agm.x.plist").write_text(
            self.PLIST.format(
                # 故意打亂順序，而且多一個只有安裝端才有的 EnvironmentVariables。
                "<key>StartInterval</key><integer>1800</integer>"
                "<key>EnvironmentVariables</key><dict><key>PATH</key><string>/usr/bin</string></dict>"
                "<key>StandardOutPath</key><string>/tmp/x.log</string>"
                "<key>ProgramArguments</key><array><string>/bin/zsh</string></array>"
                "<key>Label</key><string>com.agm.x</string>"
            ), encoding="utf-8")
        r = self.ok("ops-sync", "--check", "--repo", str(self.repo))
        self.assertTrue(r["in_sync"], r)
        self.assertIn("LaunchAgents/com.agm.x.plist", [x["target"] for x in r["ok"]])

    def test_a_dropped_env_vars_key_is_drift_even_though_its_value_is_ignored(self):
        """issue #499（i264 review）：只忽略**值**。安裝端整份掉了 `EnvironmentVariables`，
        那個 job 就少了 `PATH`——那是落差，不能因為「這個鍵不比」就報成同步。"""
        self.with_plists()
        self.install_plist_without_env()
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        r = json.loads(out)
        [row] = [x for x in r["drift"] if x["target"] == "LaunchAgents/com.agm.x.plist"]
        self.assertEqual(row["diff"]["EnvironmentVariables"], {"repo": "<ignored>", "installed": None})

    def install_plist_without_env(self) -> None:
        (self.agents / "com.agm.x.plist").write_text(
            self.plist("com.agm.x", 1800, extra="<key>StandardOutPath</key><string>/tmp/x.log</string>"),
            encoding="utf-8")

    def test_log_path_drift_is_reported(self):
        """issue #499：`StandardOutPath`／`StandardErrorPath` 是 log 的落點。browser-gc 這種沒有
        ops-alert 管道的，失敗只留 log——路徑漂掉卻報同步，等於證據來源斷了還顯示綠燈。
        白名單那版會靜默忽略這兩個鍵，所以這條當時是綠的。"""
        self.with_plists()
        (self.agents / "com.agm.x.plist").write_text(
            self.plist("com.agm.x", 1800,
                       extra="<key>StandardOutPath</key><string>/tmp/moved.log</string>"
                             "<key>EnvironmentVariables</key><dict/>"),
            encoding="utf-8")
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        r = json.loads(out)
        [row] = [x for x in r["drift"] if x["target"] == "LaunchAgents/com.agm.x.plist"]
        self.assertEqual(row["diff"]["StandardOutPath"], {"repo": "/tmp/x.log", "installed": "/tmp/moved.log"})

    def test_a_key_missing_on_the_installed_side_is_drift(self):
        """少一個鍵跟改一個值同樣是落差——只看其中一邊會漏掉「安裝端整個少了 StandardErrorPath」。"""
        self.with_plists()
        (self.agents / "com.agm.x.plist").write_text(self.plist("com.agm.x", 1800), encoding="utf-8")
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        r = json.loads(out)
        [row] = [x for x in r["drift"] if x["target"] == "LaunchAgents/com.agm.x.plist"]
        self.assertEqual(row["diff"]["StandardOutPath"], {"repo": "/tmp/x.log", "installed": None})

    def test_plist_interval_drift_is_reported_with_both_values(self):
        """#487 的本體：實機把間隔改掉（或文件跟排程不一致）要看得出來，而且要講出兩邊的值。"""
        self.with_plists()
        (self.agents / "com.agm.x.plist").write_text(
            self.plist("com.agm.x", 600,
                       extra="<key>StandardOutPath</key><string>/tmp/x.log</string>"
                             "<key>EnvironmentVariables</key><dict/>"),
            encoding="utf-8")
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        r = json.loads(out)
        [row] = [x for x in r["drift"] if x["target"] == "LaunchAgents/com.agm.x.plist"]
        self.assertEqual(row["diff"]["StartInterval"], {"repo": 1800, "installed": 600})

    def test_an_unlisted_com_agm_job_is_reported_as_unversioned(self):
        """`~/Library/LaunchAgents` 有、對照表沒有的 job＝沒有版控的排程，正是 #487 要抓的。"""
        self.with_plists()
        (self.agents / "com.agm.x.plist").write_text(
            self.plist("com.agm.x", 1800,
                       extra="<key>StandardOutPath</key><string>/tmp/x.log</string>"
                             "<key>EnvironmentVariables</key><dict/>"),
            encoding="utf-8")
        (self.agents / "com.agm.ghost.plist").write_text(self.plist("com.agm.ghost", 60), encoding="utf-8")
        (self.agents / "com.other.thing.plist").write_text(self.plist("com.other.thing", 60), encoding="utf-8")
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        r = json.loads(out)
        extras = [x["target"] for x in r["extra"]]
        self.assertIn("LaunchAgents/com.agm.ghost.plist", extras)
        self.assertNotIn("LaunchAgents/com.other.thing.plist", extras, "只管 com.agm.*，別人的 job 不碰")

    def test_an_unreadable_plist_is_drift_not_a_crash(self):
        """壞掉的 plist 要報成落差，不是讓整份報告炸掉。"""
        self.with_plists()
        (self.agents / "com.agm.x.plist").write_text("not a plist", encoding="utf-8")
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        r = json.loads(out)
        [row] = [x for x in r["drift"] if x["target"] == "LaunchAgents/com.agm.x.plist"]
        self.assertIn("_error", row["diff"])

    def test_in_sync_exits_zero_and_alert_pushes_one_ops_alert(self):
        for n in "bc":
            self.install(f"bin/{n}.sh", f"{n} v1\n")
        self.install("bin/a.sh", "a v3\n")
        self.install("t.md", "task\n")
        r = self.ok("ops-sync", "--check", "--repo", str(self.repo), "--alert")
        self.assertTrue(r["in_sync"])
        self.assertFalse([x for x in FakeDaemon.seen if x["path"] != "/api/session"], "一致就不喊人")
        FakeDaemon.routes["POST /api/supervisor/ops-alerts"] = (200, {"queued": True})
        self.install("bin/a.sh", "a v2\n")
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo), "--alert")
        self.assertEqual(code, 1)
        [sent] = [x for x in FakeDaemon.seen if x["path"] == "/api/supervisor/ops-alerts"]
        self.assertEqual((sent["body"]["source"], sent["body"]["reason"]), ("ops-sync", "installed_out_of_sync"))
        self.assertIn("bin/a.sh", sent["body"]["detail"])

    def test_the_real_manifest_names_existing_sources(self):
        repo = Path(__file__).resolve().parent.parent
        for line in (repo / agm.OPS_MANIFEST).read_text(encoding="utf-8").splitlines():
            if line.strip() and not line.startswith("#"):
                source, _target = line.split()
                self.assertTrue((repo / source).is_file(), f"對照表指到不存在的來源：{source}")


# ------------------------------------------------------------- issue 認領（#425）

# 假 gh：`issue view` 吐 $GH_STATE/issue-<n>.json，其餘子命令只記進 $GH_STATE/calls.log。
# GH_FAIL=<子命令> 讓那一個子命令失敗；GH_NO_LABEL=1 模擬 label 還不存在（`issue edit --add-label` 先失敗）。
# 真的 gh 不可達：PATH 只留這個目錄與 /usr/bin:/bin，而且這支 stub 不認得的子命令一律 exit 2。
FAKE_GH = r"""#!/usr/bin/env python3
import json, os, sys
state = os.environ["GH_STATE"]
argv = sys.argv[1:]
with open(os.path.join(state, "calls.log"), "a") as f:
    f.write(json.dumps(argv) + "\n")   # 一行一筆 JSON：--body 裡有換行也不會把記錄切斷
sub = " ".join(argv[:2])
if os.environ.get("GH_FAIL") == sub:
    sys.stderr.write("gh: boom\n")
    sys.exit(1)
if sub == "issue view":
    n = argv[2]
    try:
        sys.stdout.write(open(os.path.join(state, "issue-%s.json" % n)).read())
    except FileNotFoundError:
        sys.stderr.write("gh: no issue %s\n" % n)
        sys.exit(1)
elif sub in ("issue comment", "issue edit", "label create"):
    if sub == "label create" and os.environ.get("GH_NO_LABEL"):
        sys.stderr.write("gh: label already exists\n")
        sys.exit(1)
    sys.stdout.write("ok\n")
else:
    sys.stderr.write("gh: unknown %s\n" % sub)
    sys.exit(2)
"""


def _iso(delta_secs: float) -> str:
    import datetime
    t = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=delta_secs)
    return t.replace(microsecond=0).isoformat().replace("+00:00", "Z")


class IssueClaimTest(unittest.TestCase):
    """`agm issue claim/release` 不連 daemon，只跟 gh 說話。"""

    ENV_KEYS = ("PATH", "GH_STATE", "GH_FAIL", "GH_NO_LABEL", "AM_AGENT_NAME", "AM_BOT_ID", "AGM_RUNTIME_DIR")

    def setUp(self):
        # 先存原值再改：PATH 指向的是等一下會被刪掉的暫存目錄，收尾一定要還原。
        saved = {k: os.environ.get(k) for k in self.ENV_KEYS}
        self.addCleanup(lambda: [os.environ.pop(k, None) if v is None else os.environ.__setitem__(k, v) for k, v in saved.items()])
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        bindir = Path(self.dir.name) / "bin"
        bindir.mkdir()
        (bindir / "gh").write_text(FAKE_GH, encoding="utf-8")
        (bindir / "gh").chmod(0o755)
        self.state = Path(self.dir.name) / "state"
        self.state.mkdir()
        # 真的 gh 不可達：PATH 只有假的那一個目錄加上系統工具。
        os.environ["PATH"] = f"{bindir}:/usr/bin:/bin"
        os.environ["GH_STATE"] = str(self.state)
        os.environ["AM_AGENT_NAME"] = "vvyyg1"
        for k in ("GH_FAIL", "GH_NO_LABEL", "AM_BOT_ID"):
            os.environ.pop(k, None)
        # runtime.json 故意不存在：issue 這條路不該去讀它。
        os.environ["AGM_RUNTIME_DIR"] = str(Path(self.dir.name) / "no-such-runtime")

    # --- 造題 ---

    def issue(self, number: int, *, labels=(), comments=(), updated: str | None = None, title="某張票"):
        body = {
            "number": number, "title": title, "state": "OPEN", "url": f"https://x/{number}",
            "labels": [{"name": n} for n in labels],
            "updatedAt": updated or _iso(-60),
            "comments": list(comments),
        }
        (self.state / f"issue-{number}.json").write_text(json.dumps(body), encoding="utf-8")

    def claim_comment(self, bot: str, *, age_secs: float, child=None, worktree=None, branch=None):
        payload = {"bot": bot, "at": _iso(-age_secs)}
        for k, v in (("child", child), ("worktree", worktree), ("branch", branch)):
            if v:
                payload[k] = v
        return {"createdAt": _iso(-age_secs),
                "body": f"派給 {bot}。\n\n<!-- agm:issue-claim {json.dumps(payload, ensure_ascii=False, sort_keys=True)} -->"}

    def release_comment(self, bot: str, *, age_secs: float):
        return {"createdAt": _iso(-age_secs),
                "body": f"{bot} 交回。\n\n<!-- agm:issue-release {json.dumps({'bot': bot, 'at': _iso(-age_secs)})} -->"}

    def close(self, number: int) -> None:
        raw = json.loads((self.state / f"issue-{number}.json").read_text())
        raw["state"] = "CLOSED"
        (self.state / f"issue-{number}.json").write_text(json.dumps(raw), encoding="utf-8")

    def calls(self):
        try:
            return [json.loads(l) for l in (self.state / "calls.log").read_text().splitlines()]
        except FileNotFoundError:
            return []

    def run_cli(self, *argv: str):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = agm.main(list(argv))
            except SystemExit as e:
                code = e.code if isinstance(e.code, int) else 2
        return code, out.getvalue(), err.getvalue()

    # --- 測試 ---

    def test_claim_labels_and_comments_with_worktree_and_branch(self):
        self.issue(425)
        code, out, err = self.run_cli("issue", "claim", "425", "--child", "i425",
                                      "--worktree", "/w/i425", "--branch", "feat/i425", "--repo", "o/r")
        self.assertEqual(code, 0, err)
        got = json.loads(out)
        self.assertEqual((got["claimed"], got["already"], got["bot"]), (True, False, "vvyyg1"))
        self.assertEqual(got["label"], "wip")
        comment = [c for c in self.calls() if c[:2] == ["issue", "comment"]]
        self.assertEqual(len(comment), 1, self.calls())
        body = comment[0][comment[0].index("--body") + 1]
        self.assertIn("派給 vvyyg1", body)
        self.assertIn("child i425", body)
        self.assertIn("/w/i425", body)
        self.assertIn("feat/i425", body)
        self.assertIn("agm:issue-claim", body)
        edit = [c for c in self.calls() if c[:2] == ["issue", "edit"]]
        self.assertEqual(edit[0][-2:], ["--add-label", "wip"], edit)
        for call in self.calls():
            self.assertEqual(call[call.index("-R") + 1], "o/r", f"--repo 要傳給每一次 gh：{call}")

    def test_claim_taken_by_another_bot_exits_3_and_names_it(self):
        self.issue(413, labels=["wip"], comments=[self.claim_comment("kd61te", age_secs=600, child="life")])
        code, out, err = self.run_cli("issue", "claim", "413")
        self.assertEqual(code, 3, out)
        e = json.loads(err)
        self.assertEqual(e["error"], "issue_claimed")
        self.assertEqual(e["claimed_by"], "kd61te")
        self.assertIn("kd61te", e["message"])
        self.assertIn("life", e["message"])
        self.assertFalse([c for c in self.calls() if c[:2] in (["issue", "comment"], ["issue", "edit"])],
                         "被別人認領時什麼都不能寫")

    def test_a_claim_with_no_activity_for_over_24h_can_be_taken_over(self):
        old = _iso(-30 * 3600)
        self.issue(413, labels=["wip"], comments=[self.claim_comment("kd61te", age_secs=30 * 3600)], updated=old)
        code, out, err = self.run_cli("issue", "claim", "413", "--child", "i413")
        self.assertEqual(code, 0, err)
        got = json.loads(out)
        self.assertEqual(got["took_over_stale_claim_from"], "kd61te")
        body = [c for c in self.calls() if c[:2] == ["issue", "comment"]][0][-1]
        self.assertIn("超過 24 小時沒動靜", body)

    def test_a_stale_claim_with_recent_activity_is_still_held(self):
        # 認領留言是三天前，但這張票一小時前還被動過：那顆 bot 還在做。
        self.issue(413, labels=["wip"], comments=[self.claim_comment("kd61te", age_secs=72 * 3600)], updated=_iso(-3600))
        code, _out, err = self.run_cli("issue", "claim", "413")
        self.assertEqual(code, 3, err)
        self.assertEqual(json.loads(err)["claimed_by"], "kd61te")

    def test_reclaiming_my_own_issue_does_not_comment_twice(self):
        self.issue(425, labels=["wip"], comments=[self.claim_comment("vvyyg1", age_secs=300)])
        got = json.loads(self.run_cli("issue", "claim", "425")[1])
        self.assertTrue(got["already"])
        self.assertFalse([c for c in self.calls() if c[:2] == ["issue", "comment"]], "重跑不洗版")

    def test_reclaiming_my_own_issue_puts_a_missing_label_back(self):
        self.issue(425, comments=[self.claim_comment("vvyyg1", age_secs=300)])
        self.assertTrue(json.loads(self.run_cli("issue", "claim", "425")[1])["already"])
        self.assertEqual([c[-2:] for c in self.calls() if c[:2] == ["issue", "edit"]], [["--add-label", "wip"]])

    def test_a_released_issue_is_free_again(self):
        self.issue(425, comments=[self.claim_comment("kd61te", age_secs=7200),
                                  self.release_comment("kd61te", age_secs=3600)])
        code, out, err = self.run_cli("issue", "claim", "425")
        self.assertEqual(code, 0, err)
        self.assertFalse(json.loads(out)["already"])

    def test_release_removes_the_label_and_leaves_a_marker(self):
        self.issue(425, labels=["wip"], comments=[self.claim_comment("vvyyg1", age_secs=300)])
        code, out, err = self.run_cli("issue", "release", "425")
        self.assertEqual(code, 0, err)
        self.assertTrue(json.loads(out)["released"])
        body = [c for c in self.calls() if c[:2] == ["issue", "comment"]][0][-1]
        self.assertIn("agm:issue-release", body)
        self.assertEqual([c[-2:] for c in self.calls() if c[:2] == ["issue", "edit"]], [["--remove-label", "wip"]])

    def test_release_does_not_take_someone_elses_issue_off(self):
        self.issue(413, labels=["wip"], comments=[self.claim_comment("kd61te", age_secs=600)])
        code, _out, err = self.run_cli("issue", "release", "413")
        self.assertEqual(code, 3)
        self.assertEqual(json.loads(err)["claimed_by"], "kd61te")
        self.assertFalse([c for c in self.calls() if c[:2] == ["issue", "edit"]], "別人的票不准把 label 拿掉")

    def test_release_on_an_unclaimed_issue_is_a_no_op(self):
        self.issue(425)
        got = json.loads(self.run_cli("issue", "release", "425")[1])
        self.assertTrue(got["already"])
        self.assertFalse([c for c in self.calls() if c[:2] in (["issue", "comment"], ["issue", "edit"])])

    def test_a_label_that_already_exists_does_not_fail_the_claim(self):
        os.environ["GH_NO_LABEL"] = "1"
        self.issue(425)
        code, _out, err = self.run_cli("issue", "claim", "425")
        self.assertEqual(code, 0, err)

    def test_a_broken_claim_marker_is_ignored_rather_than_trusted(self):
        self.issue(425, labels=["wip"], comments=[{"createdAt": _iso(-600), "body": "派給 x\n<!-- agm:issue-claim {oops -->"}])
        code, out, err = self.run_cli("issue", "claim", "425")
        self.assertEqual(code, 0, err)
        self.assertFalse(json.loads(out)["already"])

    def test_gh_failure_is_reported_and_nothing_is_written(self):
        os.environ["GH_FAIL"] = "issue view"
        self.issue(425)
        code, _out, err = self.run_cli("issue", "claim", "425")
        self.assertEqual(code, 1)
        self.assertEqual(json.loads(err)["error"], "gh_failed")
        self.assertFalse([c for c in self.calls() if c[:2] == ["issue", "comment"]])

    def test_without_a_bot_identity_it_refuses_before_talking_to_gh(self):
        os.environ.pop("AM_AGENT_NAME", None)
        os.environ.pop("AM_BOT_ID", None)
        self.issue(425)
        code, _out, err = self.run_cli("issue", "claim", "425")
        self.assertEqual(code, 2)
        self.assertEqual(json.loads(err)["error"], "no_identity")
        self.assertFalse(self.calls(), "認不出自己就不要打 gh")

    def test_explicit_bot_flag_wins_over_the_pane_environment(self):
        self.issue(425)
        got = json.loads(self.run_cli("issue", "claim", "425", "--bot", "kd61te")[1])
        self.assertEqual(got["bot"], "kd61te")

    def test_a_closed_issue_is_not_claimed(self):
        # 派工前才 claim；票關掉之後再貼 wip 只會誤導下一個人。
        self.issue(406)
        self.close(406)
        code, _out, err = self.run_cli("issue", "claim", "406")
        self.assertEqual(code, 2)
        self.assertEqual(json.loads(err)["error"], "issue_closed")
        self.assertFalse([c for c in self.calls() if c[:2] in (["issue", "comment"], ["issue", "edit"])],
                         "關掉的票不貼 label、不留言")

    def test_release_still_works_on_a_closed_issue(self):
        # 關票之後清掉留著的 wip 是正常收尾，不能被上面那道擋住。
        self.issue(406, labels=["wip"], comments=[self.claim_comment("vvyyg1", age_secs=300)])
        self.close(406)
        code, out, err = self.run_cli("issue", "release", "406")
        self.assertEqual(code, 0, err)
        self.assertTrue(json.loads(out)["released"])
        self.assertEqual([c[-2:] for c in self.calls() if c[:2] == ["issue", "edit"]], [["--remove-label", "wip"]])

    def test_claim_never_touches_the_daemon_runtime(self):
        # AGM_RUNTIME_DIR 指向不存在的目錄：真的去讀 runtime.json 就會是 no_runtime／exit 2。
        self.issue(425)
        self.assertEqual(self.run_cli("issue", "claim", "425")[0], 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
