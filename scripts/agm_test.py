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
import re
import socket
import socketserver
import subprocess
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


class Raw:
    def __init__(self, text: str, content_type: str = "text/html") -> None:
        self.text = text
        self.content_type = content_type


class FakeDaemon(BaseHTTPRequestHandler):
    routes: dict = {}
    seen: list = []
    slow: set = set()
    drop: set = set()
    # slow 的 handler 等這個 Event（最多 1.5 秒）：測試收尾時放行，handler 才不會活過它的測試（#560）。
    release: threading.Event = threading.Event()

    def log_message(self, *_args):  # 別把測試輸出洗掉
        pass

    def _run(self, method: str) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length) or b"null") if length else None
        path = self.path
        type(self).seen.append({
            "method": method, "path": path, "token": self.headers.get("X-AM-Token"), "body": body,
            "bot_id": self.headers.get("X-AM-Bot-Id"), "bot_token": self.headers.get("X-AM-Bot-Token"),
            "service_id": self.headers.get("X-AM-Service-Id"), "service_token": self.headers.get("X-AM-Service-Token"),
            "caller": self.headers.get("X-AM-Caller"),
        })
        if path in type(self).slow:
            type(self).release.wait(1.5)
        if path in type(self).drop:
            # 收到請求後不回應就把連線掐掉（送達未知的那種故障）。
            self.request.shutdown(socket.SHUT_RDWR)
            return
        entry = type(self).routes.get(f"{method} {path.split('?')[0]}")
        if entry is None:
            self._send(404, {"error": "not_found"})
            return
        # 分頁要看 query 才答得出來，所以路由也收一個 `path -> (status, payload)` 的函式。
        if callable(entry):
            entry = entry(path)
        self._send(*entry)

    def _send(self, status: int, payload: object) -> None:
        # `Raw` = 原樣送出的文字（HTML、純文字…），測「daemon 回非 JSON」用。
        if isinstance(payload, Raw):
            raw, ctype = payload.text.encode(), payload.content_type
        else:
            raw, ctype = json.dumps(payload).encode(), "application/json"
        self.send_response(status)
        self.send_header("Content-Type", ctype)
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

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._idle = threading.Condition()
        self.inflight = 0
        self.errors: list = []

    # daemon_threads 的 handler 沒人等：數著還在跑的，收尾才能等它們跑完（#560）。
    def process_request(self, request, client_address):
        with self._idle:
            self.inflight += 1
        super().process_request(request, client_address)

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            with self._idle:
                self.inflight -= 1
                self._idle.notify_all()

    def handle_error(self, request, client_address):
        # 標準版把 traceback 印到**呼叫當下的** `sys.stderr`：handler 執行緒出錯時若別的指令正在
        # `bad()` 裡接 stderr，那段字會混在 CLI 的 JSON 前面（#560）。逾時／掐線測試本來就會讓
        # handler 寫到關掉的連線，記在這裡給要看的測試查。
        self.errors.append(sys.exc_info()[1])

    def drain(self, timeout: float) -> bool:
        with self._idle:
            return self._idle.wait_for(lambda: self.inflight == 0, timeout)

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
        FakeDaemon.release = threading.Event()
        self.addCleanup(self.release_server)
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.write_runtime({"daemon_url": f"http://127.0.0.1:{self.port}", "manager_bot_id": "bot-agm"})
        os.environ["AGM_RUNTIME_DIR"] = self.dir.name
        self.addCleanup(lambda: os.environ.pop("AGM_RUNTIME_DIR", None))
        # 若 CLI 忘了關 proxy，這個位址不存在 → 連線失敗，測試會炸。
        for var in ("HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"):
            os.environ[var] = "http://127.0.0.1:9"
            self.addCleanup(lambda v=var: os.environ.pop(v, None))
        # 在 bot 的 pane 裡跑測試時這些都有值：agm 一看到 AM_BOT_ID 就改用 Bot 身分（#556），
        # 期望「一般 shell＝User token」的測試會紅。每個測試從乾淨的身分環境開始，要的自己設。
        for var in ("AM_BOT_ID", "AM_BOT_TOKEN", "AM_HOOK_TOKEN", "AM_SERVICE_ID", "AM_SERVICE_TOKEN_FILE"):
            old = os.environ.pop(var, None)
            if old is not None:
                self.addCleanup(lambda v=var, o=old: os.environ.__setitem__(v, o))

    def release_server(self) -> None:
        """收尾：放行卡在 slow 的 handler，等 in-flight 的全部跑完（#560）。

        不等的話，逾時測試留下的 handler 會在**下一個**測試裡醒來、往關掉的連線寫回應，
        那時的錯誤輸出會落進下一個測試 `bad()` 正在接的 stderr。
        """
        FakeDaemon.release.set()
        self.assertTrue(self.server.drain(5), f"假 daemon 還有 {self.server.inflight} 個 handler 沒跑完")

    def write_runtime(self, cfg: dict) -> None:
        (Path(self.dir.name) / "runtime.json").write_text(json.dumps(cfg), encoding="utf-8")

    def run_cli(self, *argv: str, stdin: str | None = None):
        out, err = io.StringIO(), io.StringIO()
        real_stdin = sys.stdin
        if stdin is not None:
            sys.stdin = io.StringIO(stdin)
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                try:
                    code = agm.main(list(argv))
                except SystemExit as e:  # argparse 的用法錯誤
                    code = e.code if isinstance(e.code, int) else 2
        finally:
            sys.stdin = real_stdin
        return code, out.getvalue(), err.getvalue()

    def ok(self, *argv: str):
        code, out, err = self.run_cli(*argv)
        self.assertEqual(code, 0, f"stderr={err}")
        return json.loads(out)

    def bad(self, *argv: str):
        code, out, err = self.run_cli(*argv)
        self.assertNotEqual(code, 0, f"expected failure, stdout={out}")
        return json.loads(err)


def _wait_until(cond, timeout: float = 5.0) -> bool:
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if cond():
            return True
        time.sleep(0.01)
    return cond()


class FakeServerIsolationTest(CliCase):
    """假 daemon 的 handler 執行緒不能漏到別的指令、別的測試（issue #560）。

    CI run 36124164820：逾時測試的 slow handler 活過了自己的測試，1.5 秒後在下一個測試裡醒來、
    往關掉的連線寫回應而出錯，`socketserver` 把 traceback 印到當下的 `sys.stderr`——正好是
    `test_missing_id_is_not_found` 的 `bad()` 在接的那個，JSON 解析失敗。這裡用 Event 把時序釘死重現。
    """

    def _fire(self, path: str) -> None:
        """不經 agm、不等回應地打一發（proxy 環境變數在 setUp 被設成壞的，這裡直接開 socket）。"""
        with contextlib.suppress(OSError), socket.create_connection(("127.0.0.1", self.port), timeout=10) as s:
            s.sendall(f"GET {path} HTTP/1.0\r\n\r\n".encode())
            while s.recv(4096):
                pass

    def test_a_late_handler_error_does_not_land_in_another_commands_stderr(self):
        gate = threading.Event()

        def late(_path):
            gate.wait(5)
            raise RuntimeError("late handler blew up")

        FakeDaemon.routes["GET /late"] = late
        threading.Thread(target=self._fire, args=("/late",), daemon=True).start()
        self.assertTrue(_wait_until(lambda: self.server.inflight == 1), "那一發要先卡在 handler 裡")

        def listing(_path):
            # 在 agm 正在跑、stderr 被接走的這個時間點放行那個 handler，並等它連錯誤處理都做完。
            gate.set()
            _wait_until(lambda: self.server.inflight == 1)
            return (200, {"assignments": [], "has_more": False})

        FakeDaemon.routes["GET /api/supervisor/assignments"] = listing
        self.assertEqual(self.bad("assignments", "--id", "nope")["error"], "not_found")
        self.assertTrue(_wait_until(lambda: len(self.server.errors) == 1), "handler 的錯要記在伺服器上")
        self.assertIn("late handler blew up", str(self.server.errors[0]))

    def test_a_slow_handler_does_not_outlive_its_test(self):
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a1"})
        FakeDaemon.slow = {"/api/supervisor/assignments"}
        self.assertEqual(
            self.bad("--timeout", "0.2", "assign", "--bot", "b1", "--text", "x", "--request-id", "r-slow")["error"],
            "delivery_unknown",
        )
        self.assertGreaterEqual(self.server.inflight, 1, "客戶端逾時走了，handler 還在等")
        started = time.monotonic()
        self.release_server()
        self.assertEqual(self.server.inflight, 0)
        self.assertLess(time.monotonic() - started, 1.0, "收尾要放行它，不是乾等 1.5 秒")


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

    def test_message_without_relay_metadata_keeps_source_unknown(self):
        m = agm.slim_message({"id": "m1", "role": "user", "content": "交辦"})
        self.assertIsNone(m["relay_from"])
        self.assertIsNone(m["relay_unverified"])

    def test_message_preserves_relay_metadata(self):
        m = agm.slim_message(
            {"id": "m1", "role": "user", "relay_from": "manager", "relay_unverified": True}
        )
        self.assertEqual(m["relay_from"], "manager")
        self.assertTrue(m["relay_unverified"])

    def test_message_keeps_the_rewind_mark(self):
        m = agm.slim_message({"id": "m1", "role": "assistant", "rewound_at": "2026-09-10T00:00:00.000Z"})
        self.assertEqual(m["rewound_at"], "2026-09-10T00:00:00.000Z")
        self.assertIsNone(agm.slim_message({"id": "m2", "role": "user"})["rewound_at"])


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
        self.assertIsNone(out["messages"][0]["relay_from"])
        self.assertIsNone(out["messages"][0]["relay_unverified"])
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

    def test_a_token_fetch_that_times_out_is_not_delivery_unknown(self):
        """取 token（GET /api/session）逾時時，交辦根本還沒送出：不能報「送達未知」叫人去對帳，
        也不能讓呼叫端以為同一個 request id 已經可能派出去。"""
        FakeDaemon.routes["POST /api/supervisor/assignments"] = (200, {"id": "a1"})
        FakeDaemon.slow = {"/api/session"}
        code, _out, err = self.run_cli("--timeout", "0.3", "assign", "--bot", "b1", "--text", "x", "--request-id", "req-tok")
        self.assertNotEqual(code, 0)
        self.assertNotEqual(json.loads(err)["error"], "delivery_unknown")
        self.assertNotEqual(code, 7)
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [], "token 都沒拿到，不會有任何 POST")

    def test_a_review_whose_token_fetch_times_out_is_not_delivery_unknown(self):
        FakeDaemon.slow = {"/api/session"}
        code, _out, err = self.run_cli("--timeout", "0.3", "review", "a1", "--decision", "accept", "--actor", "AGM", "--source", "test")
        self.assertNotEqual(json.loads(err)["error"], "delivery_unknown")
        self.assertNotEqual(code, 7)

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

    def test_single_lookup_does_not_scan_the_whole_list_when_the_detail_endpoint_answers(self):
        """`--id` 先問 `/assignments/{id}`；答得出來就不該先把整份清單（可能上萬筆、上百頁）翻完再丟掉。
        清單那支壞掉時，詳情那支好好的，指令也不該跟著失敗。"""
        FakeDaemon.routes["GET /api/supervisor/assignments/a2"] = (200, {"id": "a2", "status": "completed"})
        FakeDaemon.routes["GET /api/supervisor/assignments"] = (500, {"error": "boom"})
        out = self.ok("assignments", "--id", "a2")
        self.assertEqual(out["id"], "a2")
        paths = [r["path"] for r in FakeDaemon.seen if r["path"].startswith("/api/supervisor/assignments")]
        self.assertEqual(paths, ["/api/supervisor/assignments/a2"], "詳情答得出來，清單一頁都不用讀")

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


class AssignmentsPagingTest(CliCase):
    """issue #515：過濾要對**全量**做。

    daemon 的清單預設只回最新一頁，而 `blocked`（＝還在等，保持未結案）天生活得比一頁久；
    只對第一頁過濾的話，卡最久的那幾筆會被 `--open` 講成「沒有」。
    """

    ROWS = [
        {"id": "a5", "client_request_id": "crid-5", "status": "completed", "created_at": "2026-09-24T00:00:00.000Z"},
        {"id": "a4", "client_request_id": "crid-4", "status": "completed", "created_at": "2026-09-23T00:00:00.000Z"},
        {"id": "a3", "client_request_id": "crid-3", "status": "completed", "created_at": "2026-09-22T00:00:00.000Z"},
        # 頁外的兩筆：舊、而且還沒結案。
        {"id": "a2", "client_request_id": "crid-2", "status": "blocked", "created_at": "2026-09-19T00:00:00.000Z"},
        {"id": "a1", "client_request_id": "crid-1", "status": "awaiting_review", "created_at": "2026-09-16T00:00:00.000Z"},
    ]

    def setUp(self):
        super().setUp()
        FakeDaemon.routes["GET /api/supervisor/assignments"] = self._page

    @classmethod
    def _page(cls, path: str):
        q = urllib.parse.parse_qs(urllib.parse.urlparse(path).query)
        # 這台 daemon 的每頁上限是 2（比呼叫端要的小）：客戶端要照 `has_more` 翻，
        # 不能因為自己要了 200 筆就當成一次拿完。
        limit = min(int((q.get("limit") or ["2"])[0]), 2)
        before = (q.get("before") or [None])[0]
        rows = cls.ROWS
        if before:
            key = tuple(json.loads(before))
            rows = [r for r in rows if (r["created_at"], r["id"]) < key]
        chunk, more = rows[:limit], len(rows) > limit
        nxt = json.dumps([chunk[-1]["created_at"], chunk[-1]["id"]]) if more and chunk else None
        return (200, {"assignments": chunk, "has_more": more, "next_cursor": nxt, "limit": limit})

    def _reads(self):
        return [r for r in FakeDaemon.seen if r["method"] == "GET" and r["path"].startswith("/api/supervisor/assignments")]

    def test_open_keeps_paging_until_the_oldest_blocked_work_shows_up(self):
        out = self.ok("assignments", "--open")
        self.assertEqual([a["id"] for a in out["assignments"]], ["a2", "a1"])
        self.assertEqual(out["open"], 2)
        self.assertIs(out["complete"], True)
        self.assertNotIn("note", out)
        self.assertGreater(len(self._reads()), 1, "第一頁看不到 a2／a1，一定要翻頁")

    def test_the_status_filter_is_sent_to_the_daemon(self):
        """issue #543：為了 6 筆未結案把一千多筆搬回來再篩，成本正好落在最需要的那個答案上。"""
        for argv, want in (
            (("assignments", "--open"), "open"),
            (("assignments", "--awaiting-review"), "awaiting_review"),
            (("assignments", "--status", "blocked"), "blocked"),
            # 兩個一起給：送比較寬的那個，剩下的交給客戶端那層收。
            (("assignments", "--open", "--awaiting-review"), "open"),
        ):
            FakeDaemon.seen.clear()
            self.ok(*argv)
            q = urllib.parse.parse_qs(urllib.parse.urlparse(self._reads()[0]["path"]).query)
            self.assertEqual(q.get("status"), [want], f"{argv}")
        # `--id` 要掃全部，不能先被狀態篩掉。
        FakeDaemon.seen.clear()
        self.ok("assignments", "--id", "crid-1")
        q = urllib.parse.parse_qs(urllib.parse.urlparse(self._reads()[0]["path"]).query)
        self.assertNotIn("status", q)
        # 沒有過濾條件時也不要送。
        FakeDaemon.seen.clear()
        self.ok("assignments", "--all")
        q = urllib.parse.parse_qs(urllib.parse.urlparse(self._reads()[0]["path"]).query)
        self.assertNotIn("status", q)

    def test_an_old_daemon_that_ignores_status_still_gets_filtered_here(self):
        """伺服器端過濾是省搬運，不是把客戶端那層換掉：舊 daemon 忽略 status，答案還是要對。"""
        FakeDaemon.routes["GET /api/supervisor/assignments"] = (
            200,
            {"assignments": self.ROWS, "has_more": False, "next_cursor": None},
        )
        out = self.ok("assignments", "--open")
        self.assertEqual([a["id"] for a in out["assignments"]], ["a2", "a1"])
        self.assertEqual(out["open"], 2)

    def test_paging_stops_when_the_cursor_stops_moving(self):
        """拿掉頁數上限（#543）之後，防無窮迴圈的是「游標必須往前走」。"""
        calls = {"n": 0}

        def stuck(_path: str):
            calls["n"] += 1
            # 守衛壞掉時要**紅**，不能掛在這裡讓 CI 跑到逾時：第 6 次之後自己收手，
            # 讓下面的次數斷言去講話。
            more = calls["n"] < 6
            return (200, {"assignments": [self.ROWS[0]], "has_more": more, "next_cursor": "same-cursor" if more else None})

        FakeDaemon.routes["GET /api/supervisor/assignments"] = stuck
        out = self.ok("assignments", "--open")
        self.assertEqual(calls["n"], 2, "第二頁游標沒動就停，不要一直問下去")
        self.assertIs(out["complete"], False)
        self.assertIn("note", out)

    def test_a_client_request_id_outside_the_first_page_is_found(self):
        """`/assignments/{id}` 只吃 assignment id，crid 一定 404 —— 退路掃的必須是全量。"""
        out = self.ok("assignments", "--id", "crid-1")
        self.assertEqual(out["id"], "a1")

    def test_a_plain_listing_still_reads_one_page(self):
        out = self.ok("assignments")
        self.assertEqual([a["id"] for a in out["assignments"]], ["a5", "a4"])
        self.assertEqual(len(self._reads()), 1, "沒有過濾條件就不要多打幾輪")
        self.assertIs(out["complete"], False)
        self.assertIn("note", out)

    def test_all_pages_everything(self):
        out = self.ok("assignments", "--all")
        self.assertEqual(len(out["assignments"]), 5)
        self.assertIs(out["complete"], True)

    def test_an_old_daemon_without_paging_says_so_instead_of_pretending(self):
        FakeDaemon.routes["GET /api/supervisor/assignments"] = (200, {"assignments": self.ROWS[:2]})
        out = self.ok("assignments", "--open")
        self.assertIs(out["complete"], False)
        self.assertIn("note", out)
        err = self.bad("assignments", "--id", "crid-1")
        self.assertEqual(err["error"], "not_found")
        self.assertIs(err["complete"], False)
        self.assertEqual(err["searched"], 2)


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

    def test_followup_from_a_file_gets_the_same_checks_as_assign(self):
        """issue #516：續作會變成一筆新的交辦，就該跟 `assign` 走同一組檢查。"""
        good = Path(self.dir.name) / "follow.md"
        good.write_text("  接著把 B 做完  ", encoding="utf-8")
        self.ok("review", "a1", "--decision", "followup", "--followup-file", str(good), "--followup-request-id", "f-1")
        self.assertEqual(self._body()["followup_text"], "接著把 B 做完")

        binary = Path(self.dir.name) / "bin.bin"
        binary.write_bytes(b"\xff\xfe not utf8 \x80")
        err = self.bad("review", "a1", "--decision", "followup", "--followup-file", str(binary), "--followup-request-id", "f-2")
        self.assertEqual(err["error"], "bad_args", "非 UTF-8 是參數錯（exit 2），不是 internal（exit 1）")
        self.assertEqual(err["path"], str(binary))

        err = self.bad("review", "a1", "--decision", "followup", "--followup-file", str(self.dir.name) + "/nope", "--followup-request-id", "f-3")
        self.assertEqual(err["error"], "bad_args")

        err = self.bad("review", "a1", "--decision", "followup", "--followup-text", "   ", "--followup-request-id", "f-4")
        self.assertEqual(err["error"], "bad_args", "只有空白的續作不要送出去")

        err = self.bad("review", "a1", "--decision", "followup", "--followup-text", "x" * (agm.MAX_TEXT_CHARS + 1), "--followup-request-id", "f-5")
        self.assertEqual((err["error"], err["max_chars"]), ("bad_args", agm.MAX_TEXT_CHARS))

        posts = [r for r in FakeDaemon.seen if r["method"] == "POST"]
        self.assertEqual(len(posts), 1, "只有第一次合法的續作送得出去")

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

    def test_acquire_always_carries_a_request_id_so_a_lost_response_can_be_replayed(self):
        """`lease_token` 只在 acquire 的回應出現一次；回應丟了要靠同一個 request_id 拿回同一張租約。
        自己給的照送，沒給就自己產一個（高熵、每次不同）。"""
        self.ok("lease", "acquire", "rebuild", "--approval", "ap-1", "--request-id", "my-req-1")
        self.assertEqual(self._last_body()["request_id"], "my-req-1")
        self.ok("lease", "acquire", "rebuild", "--approval", "ap-1")
        first = self._last_body()["request_id"]
        self.assertRegex(first, r"^[0-9a-f]{32}$")
        self.ok("lease", "acquire", "rebuild", "--approval", "ap-1")
        self.assertNotEqual(self._last_body()["request_id"], first)

    def test_an_acquire_that_times_out_is_delivery_unknown_and_names_the_request_id_to_replay(self):
        """逾時＝租約可能已經開了、token 卻沒收到：不能只回含糊的 timeout，要說「用同一個 request_id 重送拿回同一張」，
        而且 CLI 自己不重送（換一個 id 重送會被 lease_held 擋住、窗口握到 TTL）。"""
        FakeDaemon.slow = {"/api/supervisor/leases/rebuild/acquire"}
        err = self.bad("--timeout", "0.3", "lease", "acquire", "rebuild", "--approval", "ap-1", "--request-id", "req-t1")
        self.assertEqual(err["error"], "delivery_unknown")
        self.assertEqual(err["request_id"], "req-t1")
        self.assertIn("req-t1", err["message"])
        posts = [r for r in FakeDaemon.seen if r["path"].endswith("/leases/rebuild/acquire")]
        self.assertEqual(len(posts), 1, "不能自己重送")

    def test_an_acquire_whose_connection_drops_after_sending_is_delivery_unknown(self):
        FakeDaemon.drop = {"/api/supervisor/leases/rebuild/acquire"}
        err = self.bad("lease", "acquire", "rebuild", "--approval", "ap-1", "--request-id", "req-d1")
        self.assertEqual(err["error"], "delivery_unknown")
        self.assertEqual(err["request_id"], "req-d1")

    def test_a_token_fetch_timeout_before_acquire_is_not_delivery_unknown(self):
        """取 token 就逾時＝acquire 根本沒送出（跟 assign 同一條規則）。"""
        FakeDaemon.slow = {"/api/session"}
        code, _out, err = self.run_cli("--timeout", "0.3", "lease", "acquire", "rebuild", "--approval", "ap-1")
        self.assertNotEqual(json.loads(err)["error"], "delivery_unknown")
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "POST"], [])

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


class CliSyncReportTest(unittest.TestCase):
    """issue #532：`ops-sync --check` 也要看 `bin/agm`。

    兩段落差的修法完全不同（一個換檔案、一個非重建＋重啟不可），所以報告要分得出來。
    這裡不連真 daemon：`cli_sync_report` 只用到 client 的 `get`／`post`。
    """

    class Fake:
        def __init__(self, payload=None, error=None):
            self.payload = payload
            self.error = error
            self.posted = []

        def get(self, path, query=None):
            if self.error:
                raise RuntimeError(self.error)
            return self.payload

        def post(self, path, body=None):
            self.posted.append((path, body))
            return {"refreshed": True}

    def repo(self, content: bytes) -> Path:
        d = tempfile.TemporaryDirectory()
        self.addCleanup(d.cleanup)
        root = Path(d.name)
        (root / "scripts").mkdir()
        (root / "scripts" / "agm.py").write_bytes(content)
        for args in (["init", "-q"], ["config", "user.email", "t@t"], ["config", "user.name", "t"],
                     ["add", "-A"], ["commit", "-qm", "init"]):
            subprocess.run(["git", "-C", str(root), *args] if args[0] != "init" else ["git", "init", "-q", str(root)],
                           check=True, capture_output=True)
        return root

    def test_the_repo_hash_is_the_same_algorithm_as_the_daemon(self):
        # daemon 的 cli_refresh::short_hash 有一樣的兩個向量；算錯就永遠對不上。
        self.assertEqual(agm.short_hash(b""), "cbf29ce48422")
        self.assertEqual(agm.short_hash(b"a"), "af63dc4c8601")

    def test_an_installed_cli_that_is_not_the_embedded_one_is_drift(self):
        body = b"# repo version\n"
        repo = self.repo(body)
        embedded = agm.short_hash(body)  # binary 是照這一版建的
        client = self.Fake({"embedded_hash": embedded,
                            "roles": [{"role": "patrol", "state": "stale", "installed_hash": "deadbeef1234"}]})
        r = agm.cli_sync_report(client, repo, "HEAD")
        self.assertEqual(r["state"], "drift")
        self.assertEqual([x["role"] for x in r["stale_roles"]], ["patrol"])
        self.assertFalse(r["binary_behind"], "binary 本身是跟得上的")
        self.assertIn("--refresh-cli", r["action"])
        self.assertNotIn("重啟", r["action"], "這一種換個檔案就好，不該叫人去重啟 daemon")

    def test_a_binary_older_than_the_repo_needs_a_rebuild_and_restart(self):
        repo = self.repo(b"# repo version\n")
        client = self.Fake({"embedded_hash": agm.short_hash(b"# older\n"),
                            "roles": [{"role": "patrol", "state": "ok", "installed_hash": agm.short_hash(b"# older\n")}]})
        r = agm.cli_sync_report(client, repo, "HEAD")
        self.assertEqual(r["state"], "drift")
        self.assertTrue(r["binary_behind"])
        self.assertIn("重啟 daemon", r["action"])

    def test_everything_matching_is_in_sync(self):
        body = b"# repo version\n"
        repo = self.repo(body)
        client = self.Fake({"embedded_hash": agm.short_hash(body),
                            "roles": [{"role": "patrol", "state": "ok", "installed_hash": agm.short_hash(body)}]})
        r = agm.cli_sync_report(client, repo, "HEAD")
        self.assertEqual(r["state"], "ok")
        self.assertEqual(r["stale_roles"], [])

    def test_a_daemon_that_cannot_be_reached_is_unknown_not_a_failure(self):
        # 本業是比對安裝端：daemon 沒開是另一件事，不該讓整支 ops-sync 變紅。
        r = agm.cli_sync_report(self.Fake(error="connection refused"), self.repo(b"x\n"), "HEAD")
        self.assertEqual(r["state"], "unknown")
        self.assertIn("connection refused", r["error"])


class LeaseTokenTest(CliCase):
    """issue #517（守 #477）：`lease_token` 不可以走 argv，而守衛本身以前一條測試都沒有。

    它是「只在 acquire 回應出現一次、任何 API 都查不到」的一次性憑證：拿到就能把別人正在
    換 binary 的窗口收掉。守衛全靠 `lease_token_of` 那幾行，失效了不會有任何人發現。
    """

    def setUp(self):
        super().setUp()
        FakeDaemon.routes["POST /api/supervisor/leases/rebuild/release"] = (200, {"released": True})
        FakeDaemon.routes["POST /api/supervisor/leases/rebuild/renew"] = (200, {"lease": {"fence": 3}})
        FakeDaemon.routes["GET /api/supervisor/maintenance/safety"] = (200, {"safe": True})
        self.tok = Path(self.dir.name) / "lease-token"
        self.tok.write_text("one-shot-secret\n", encoding="utf-8")
        os.chmod(self.tok, 0o600)

    def _bodies(self):
        return [r["body"] for r in FakeDaemon.seen if r["method"] == "POST"]

    def test_a_0600_file_gets_the_token_into_the_body_and_never_into_argv(self):
        argv = ["lease", "release", "rebuild", "--owner", "bot-a", "--fence", "3", "--lease-token-file", str(self.tok)]
        code, _out, err = self.run_cli(*argv)
        self.assertEqual(code, 0, err)
        self.assertEqual(self._bodies()[-1]["lease_token"], "one-shot-secret")
        # argv 同一台機器上誰都看得到（`ps`）：檔案路徑可以在裡面，token 本身不行。
        self.assertNotIn("one-shot-secret", " ".join(argv))

    def test_a_file_anyone_else_can_read_is_treated_as_already_leaked(self):
        for mode in (0o640, 0o604, 0o644, 0o666):
            os.chmod(self.tok, mode)
            err = self.bad("lease", "release", "rebuild", "--owner", "b", "--fence", "3", "--lease-token-file", str(self.tok))
            self.assertEqual(err["error"], "bad_args", f"{mode:o}")
            self.assertIn(f"{mode:o}", err["message"], "訊息要講出實際權限，不然不知道要 chmod 什麼")
        self.assertEqual(self._bodies(), [], "被拒的憑證一個請求都不送出去")

    def test_the_path_itself_may_not_be_a_symlink(self):
        """O_NOFOLLOW：驗過的跟讀到的要是同一個檔，而 `os.stat` 會跟著 symlink 走到別人的 0600。"""
        link = Path(self.dir.name) / "link-to-token"
        link.symlink_to(self.tok)
        self.assertTrue(link.is_symlink() and link.exists(), "前提：symlink 真的建起來而且指得到東西")
        err = self.bad("lease", "release", "rebuild", "--owner", "b", "--fence", "3", "--lease-token-file", str(link))
        self.assertEqual(err["error"], "bad_args")
        # 「路徑打錯」也是 bad_args：擋下來的必須是 symlink 這件事本身，不是找不到檔。
        self.assertNotIn("No such file", err["message"])
        self.assertEqual(self._bodies(), [])
        # 對照組：同一個目標用真路徑讀得到，證明擋的是 symlink 不是內容或權限。
        self.ok("lease", "release", "rebuild", "--owner", "b", "--fence", "3", "--lease-token-file", str(self.tok))
        self.assertEqual(self._bodies()[-1]["lease_token"], "one-shot-secret")

    def test_something_that_is_not_a_regular_file_is_a_bad_arg_not_a_traceback(self):
        """目錄 `fdopen` 會丟 IsADirectoryError、fifo 會讓 open 一直等寫端（整支 CLI 卡住）。"""
        fifo = Path(self.dir.name) / "fifo"
        os.mkfifo(fifo, 0o600)
        for path in (self.dir.name, str(fifo)):
            err = self.bad("lease", "release", "rebuild", "--owner", "b", "--fence", "3", "--lease-token-file", path)
            self.assertEqual(err["error"], "bad_args", path)
        self.assertEqual(self._bodies(), [])

    def test_an_empty_file_is_refused(self):
        empty = Path(self.dir.name) / "empty"
        empty.write_text("", encoding="utf-8")
        os.chmod(empty, 0o600)
        err = self.bad("lease", "release", "rebuild", "--owner", "b", "--fence", "3", "--lease-token-file", str(empty))
        self.assertEqual(err["error"], "bad_args")
        self.assertEqual(self._bodies(), [])

    def test_the_two_ways_of_giving_it_are_mutually_exclusive(self):
        err = self.bad(
            "lease", "release", "rebuild", "--owner", "b", "--fence", "3",
            "--lease-token-file", str(self.tok), "--lease-token", "inline",
        )
        self.assertEqual(err["error"], "bad_args")
        self.assertEqual(self._bodies(), [], "含糊不清的時候什麼都不送")

    def test_a_dash_reads_the_token_from_stdin(self):
        code, _out, _err = self.run_cli(
            "lease", "renew", "rebuild", "--owner", "b", "--fence", "3", "--lease-token", "-", stdin="from-stdin\n"
        )
        self.assertEqual(code, 0)
        self.assertEqual(self._bodies()[-1]["lease_token"], "from-stdin")
        code, _out, err = self.run_cli(
            "lease", "renew", "rebuild", "--owner", "b", "--fence", "3", "--lease-token", "-", stdin=""
        )
        self.assertEqual(code, 2)
        self.assertEqual(json.loads(err)["error"], "bad_args")

    def test_release_without_a_token_still_sends_the_owner_and_fence(self):
        """舊的租約（token 出現之前拿的）還交得回來：沒帶就是不帶這個欄位，不是空字串。"""
        self.ok("lease", "release", "rebuild", "--owner", "b", "--fence", "3")
        self.assertNotIn("lease_token", self._bodies()[-1])

    def test_safety_forwards_the_owner_and_the_approval_it_is_waiting_on(self):
        """SPEC §18.10「自己的租約不擋自己」：`--owner` 要真的進 query，不然等的是別人的答案。"""
        self.ok("lease", "safety", "--owner", "bot-a", "--approval", "ap-1", "--exclude-bot", "builder")
        read = [r for r in FakeDaemon.seen if r["method"] == "GET" and "/maintenance/safety" in r["path"]][-1]
        self.assertEqual(
            urllib.parse.parse_qs(urllib.parse.urlparse(read["path"]).query),
            {"exclude": ["builder"], "approval": ["ap-1"], "owner": ["bot-a"]},
        )
        # 不帶就不要送：那是「每一把租約都算擋」的舊行為，送空字串會變成另一個意思。
        self.ok("lease", "safety")
        read = [r for r in FakeDaemon.seen if r["method"] == "GET" and "/maintenance/safety" in r["path"]][-1]
        self.assertEqual(urllib.parse.parse_qs(urllib.parse.urlparse(read["path"]).query), {})

    def test_a_forced_release_needs_a_reason(self):
        err = self.bad("lease", "release", "rebuild", "--owner", "b", "--fence", "3", "--force")
        self.assertEqual(err["error"], "bad_args")
        self.assertEqual(self._bodies(), [])
        self.ok("lease", "release", "rebuild", "--owner", "b", "--fence", "3", "--force", "--reason", "持有者的 pane 沒了")
        self.assertEqual((self._bodies()[-1]["force"], self._bodies()[-1]["reason"]), (True, "持有者的 pane 沒了"))


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

    def test_set_from_a_file_and_a_non_utf8_one_is_a_bad_arg(self):
        """issue #516：`--file` 跟 `--text-file` 是同一件事，錯法也該一樣。"""
        good = Path(self.dir.name) / "persona.md"
        good.write_text("新的人設", encoding="utf-8")
        self.ok("persona", "set", "--file", str(good))
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "PUT"][-1]["body"]["text"], "新的人設")
        binary = Path(self.dir.name) / "persona.bin"
        binary.write_bytes(b"\xff\xfe\x00")
        err = self.bad("persona", "set", "--file", str(binary))
        self.assertEqual(err["error"], "bad_args")
        self.assertEqual(err["path"], str(binary))

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

    def test_handoff_from_a_file_and_a_non_utf8_one_is_a_bad_arg(self):
        """issue #516：`--summary-file` 也走同一個讀檔 helper。"""
        FakeDaemon.routes["PUT /api/supervisor/handoff"] = (200, {"summary_version": 9})
        good = Path(self.dir.name) / "handoff.md"
        good.write_text("交接摘要", encoding="utf-8")
        self.ok("handoff", "--summary-file", str(good))
        self.assertEqual([r for r in FakeDaemon.seen if r["method"] == "PUT"][-1]["body"]["summary"], "交接摘要")
        binary = Path(self.dir.name) / "handoff.bin"
        binary.write_bytes(b"\x80\x81")
        err = self.bad("handoff", "--summary-file", str(binary))
        self.assertEqual(err["error"], "bad_args")
        self.assertEqual(err["path"], str(binary))

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
        for k in ("AM_BOT_ID", "AM_BOT_TOKEN", "AM_HOOK_TOKEN", "AM_SERVICE_ID", "AM_SERVICE_TOKEN_FILE"):
            old = os.environ.pop(k, None)
            self.addCleanup(lambda k=k, v=old: os.environ.__setitem__(k, v) if v is not None else os.environ.pop(k, None))
        self.write_runtime({
            "daemon_url": f"http://127.0.0.1:{self.port}", "manager_bot_id": "bot-agm",
            "role": "responder", "self_bot_id": "bot-resp",
        })

    def last(self, method: str, prefix: str) -> dict:
        return [r for r in FakeDaemon.seen if r["method"] == method and r["path"].startswith(prefix)][-1]

    def test_each_bot_request_uses_its_pane_identity_without_the_shared_user_token(self):
        FakeDaemon.routes["POST /api/supervisor/inbox/e1/ack"] = (200, {})
        os.environ["AM_BOT_ID"], os.environ["AM_BOT_TOKEN"] = "bot-resp", "hook-secret"
        self.ok("ack", "e1")
        seen = self.last("POST", "/api/supervisor/inbox/e1/ack")
        self.assertEqual((seen["bot_id"], seen["bot_token"]), ("bot-resp", "hook-secret"))
        self.assertIsNone(seen["token"], "a bot request must not also carry the shared User token")
        # 同一支 CLI 在別顆 bot 的 pane 裡跑：改用那顆 bot 自己的 proof，不能借 AGM 的身分。
        os.environ["AM_BOT_ID"], os.environ["AM_BOT_TOKEN"] = "bot-worker", "worker-secret"
        self.ok("ack", "e1")
        seen = self.last("POST", "/api/supervisor/inbox/e1/ack")
        self.assertEqual((seen["bot_id"], seen["bot_token"]), ("bot-worker", "worker-secret"))
        self.assertIsNone(seen["token"], "a Bot principal must never carry the shared User token")
        # A mismatched token remains a Bot request and must fail at the daemon; it is never retried as User.
        os.environ["AM_BOT_TOKEN"] = "hook-secret"
        self.ok("ack", "e1")
        seen = self.last("POST", "/api/supervisor/inbox/e1/ack")
        self.assertEqual((seen["bot_id"], seen["bot_token"]), ("bot-worker", "hook-secret"))
        self.assertIsNone(seen["token"])
        self.assertNotIn("hook-secret", json.dumps(self.ok("whoami")))

    def test_missing_bot_token_sends_partial_identity_instead_of_downgrading_to_user(self):
        FakeDaemon.routes["POST /api/supervisor/inbox/e1/ack"] = (200, {})
        os.environ["AM_BOT_ID"] = "bot-resp"
        self.ok("ack", "e1")
        seen = self.last("POST", "/api/supervisor/inbox/e1/ack")
        self.assertEqual(seen["bot_id"], "bot-resp")
        self.assertIsNone(seen["bot_token"])
        self.assertIsNone(seen["token"], "a missing bot proof must be rejected by daemon, never retried as User")

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
        for op in ("resume", "cancel"):
            self.assertEqual(self.ok("mission", op, "m1")["status"], op)
        self.assertEqual(self.bad("mission", "round", "m1")["error"], "bad_args")
        self.assertEqual(self.ok("mission", "round", "m1", "--request-id", "rnd-1")["status"], "round")
        self.assertEqual(self.posts("/api/missions/m1/round")[0]["body"], {"client_request_id": "rnd-1"})

    def test_round_over_the_cap_surfaces_the_conflict(self):
        FakeDaemon.routes["POST /api/missions/m1/round"] = (409, {"error": "conflict", "reason": "max_rounds", "rounds_used": 2})
        err = self.bad("mission", "round", "m1", "--request-id", "rnd-cap")
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


class ServicePrincipalClientTest(CliCase):
    def setUp(self):
        super().setUp()
        for k in ("AM_SERVICE_ID", "AM_SERVICE_TOKEN_FILE", "AM_BOT_ID", "AM_BOT_TOKEN", "AM_HOOK_TOKEN"):
            old = os.environ.pop(k, None)
            self.addCleanup(lambda k=k, v=old: os.environ.__setitem__(k, v) if v is not None else os.environ.pop(k, None))

    def service_token_file(self, mode=0o600):
        path = Path(self.dir.name) / "daemon-swap.token"
        path.write_text("service-test-token\n", encoding="utf-8")
        path.chmod(mode)
        os.environ["AM_SERVICE_ID"] = "daemon-swap"
        os.environ["AM_SERVICE_TOKEN_FILE"] = str(path)
        return path

    def test_service_identity_uses_private_file_without_fetching_ui_token(self):
        self.service_token_file()
        FakeDaemon.routes["GET /api/supervisor/health"] = (200, {"status": "healthy"})
        self.ok("health")
        seen = next(r for r in FakeDaemon.seen if r["path"] == "/api/supervisor/health")
        self.assertEqual((seen["service_id"], seen["service_token"]), ("daemon-swap", "service-test-token"))
        self.assertIsNone(seen["token"])
        self.assertFalse(any(r["path"] == "/api/session" for r in FakeDaemon.seen))

    def test_service_identity_rejects_a_group_or_world_readable_token_file(self):
        self.service_token_file(0o644)
        err = self.bad("health")
        self.assertEqual(err["error"], "bad_service_auth")
        self.assertEqual(FakeDaemon.seen, [], "unsafe credentials must fail before sending any request")

    def test_service_identity_rejects_a_group_or_world_accessible_token_directory(self):
        path = self.service_token_file()
        path.parent.chmod(0o755)
        err = self.bad("health")
        self.assertEqual(err["error"], "bad_service_auth")
        self.assertEqual(FakeDaemon.seen, [], "an exposed service-token directory must fail before sending any request")


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
        # Linux 那一邊（issue #677）同理：systemd user 目錄也指到暫存目錄，平台預設 darwin——
        # 既有的 plist 測試寫的是 macOS 的行為，不能因為換到 Linux 機器跑就變成「plist 列被略過」。
        self.units = Path(self.dir.name) / "systemd-user"
        self.units.mkdir(parents=True, exist_ok=True)
        for var, val in (("AGM_SYSTEMD_USER_DIR", str(self.units)), ("AGM_OPS_PLATFORM", "darwin")):
            os.environ[var] = val
            self.addCleanup(os.environ.pop, var, None)
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

    def test_an_install_stage_file_left_by_a_killed_ops_install_is_not_an_extra_script(self):
        """`ops-install.sh` 換檔前先寫 `<檔名>.new.<pid>`；被 SIGKILL 時它會留在 bin/。那是 ops-install 的殘件
        （下次執行會清），不是「沒有版控的腳本」：不能因此報成 `extra`、把 ops-sync 弄紅。名字不是 `.new.<數字>` 的照舊算。"""
        self.install("bin/c.sh", "c v1\n")
        self.install("bin/c.sh.new.4242", "half written")
        self.install("bin/c.sh.new.notapid", "someone's own file")
        code, out, err = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        r = json.loads(out)
        extras = [x["target"] for x in r["extra"]]
        self.assertNotIn("bin/c.sh.new.4242", extras)
        self.assertIn("bin/c.sh.new.notapid", extras)

    def test_systemd_history_search_skips_a_commit_where_the_source_was_deleted(self):
        """歷史路徑可能經過刪除再新增；那個刪除 commit 沒有可供語意解析的 blob，應繼續找更舊版本。"""
        source = "scripts/ops/example.service"
        target = "systemd/com.agm.example.service"
        v1 = "[Unit]\nDescription=version one\n[Service]\nExecStart=/bin/true\n"
        v2 = "[Unit]\nDescription=version two\n[Service]\nExecStart=/bin/true\n"
        v3 = "[Unit]\nDescription=version three\n[Service]\nExecStart=/bin/true\n"
        self.put("scripts/ops/install-manifest.tsv",
                 "# comment\nscripts/ops/a.sh bin/a.sh\nscripts/ops/b.sh bin/b.sh\n"
                 "scripts/ops/c.sh bin/c.sh\nscripts/ops/t.md t.md\n"
                 f"{source} {target} linux\n")
        self.put(source, v1)
        self.git("add", "-A")
        self.git("commit", "-qm", "service version one")
        v1_commit = self.git("rev-parse", "--short=8", "HEAD")
        installed = self.units / "com.agm.example.service"
        installed.write_text(v1, encoding="utf-8")
        self.put(source, v2)
        self.git("commit", "-qam", "service version two")
        (self.repo / source).unlink()
        self.git("add", "-A")
        self.git("commit", "-qm", "temporarily remove service")
        self.put(source, v3)
        self.git("add", "-A")
        self.git("commit", "-qm", "restore service at version three")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        os.environ["AGM_OPS_PLATFORM"] = "linux"

        code, out, err = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1, err)
        report = json.loads(out)
        [row] = [row for row in report["behind"] if row["target"] == target]
        self.assertEqual(row["behind"], 3)
        self.assertEqual(row["installed_commit"], v1_commit)
        self.assertEqual({commit.split(" ", 1)[1] for commit in row["commits"]}, {
            "service version two", "temporarily remove service", "restore service at version three",
        })

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
        # 這支現在多問一件**唯讀**的事：已安裝的 bin/agm 跟不跟得上（issue #532）。除此之外不碰 API。
        self.assertFalse([x for x in FakeDaemon.seen if x["method"] != "GET"], "沒帶 --alert 不寫任何東西")
        self.assertEqual(
            {x["path"] for x in FakeDaemon.seen if x["path"] != "/api/session"},
            {"/api/supervisor/cli"},
        )
        # 只讀：安裝檔一個字都沒動。
        self.assertEqual((Path(self.dir.name) / "bin/b.sh").read_text(encoding="utf-8"), "b 有人直接改了\n")

    def test_a_stale_installed_cli_alone_turns_the_check_red(self):
        """issue #532：ops 腳本全對、但 `bin/agm` 不是這顆 binary 內嵌的那份——以前一律回報 ok。

        那正是最會痛的組合：kick 派出的正文用 `--lease-token-file`，舊 CLI argparse rc 2，
        rebuild 窗口沒交還、握到 TTL。
        """
        for n in "abc":
            self.install(f"bin/{n}.sh", f"{n} v3\n" if n == "a" else f"{n} v1\n")
        self.install("t.md", "task\n")
        FakeDaemon.routes["GET /api/supervisor/cli"] = (200, {
            "embedded_hash": "aaaaaaaaaaaa",
            "roles": [{"role": "patrol", "state": "stale", "installed_hash": "bbbbbbbbbbbb"}],
        })
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        r = json.loads(out)
        self.assertFalse(r["in_sync"])
        self.assertEqual(r["cli"]["state"], "drift")
        self.assertEqual([x["role"] for x in r["cli"]["stale_roles"]], ["patrol"])
        # 沒給 --refresh-cli 就不會去換（唯讀）。
        self.assertFalse([x for x in FakeDaemon.seen if x["method"] != "GET"])

        # 給了才換，而且換完重問一次。
        FakeDaemon.routes["POST /api/supervisor/cli"] = (200, {"embedded_hash": "aaaaaaaaaaaa", "roles": []})
        FakeDaemon.seen = []
        FakeDaemon.routes["GET /api/supervisor/cli"] = (200, {
            "embedded_hash": "aaaaaaaaaaaa",
            "roles": [{"role": "patrol", "state": "ok", "installed_hash": "aaaaaaaaaaaa"}],
        })
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo), "--refresh-cli")
        self.assertEqual(code, 0, out)

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
                 "scripts/ops/launchd/com.agm.x.plist LaunchAgents/com.agm.x.plist darwin\n")
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
        # 這支現在多問一件**唯讀**的事：已安裝的 bin/agm 跟不跟得上（issue #532）。除此之外不碰 API。
        self.assertFalse([x for x in FakeDaemon.seen if x["method"] != "GET"], "一致就不喊人")
        self.assertEqual(
            {x["path"] for x in FakeDaemon.seen if x["path"] != "/api/session"},
            {"/api/supervisor/cli"},
        )
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
                source = line.split()[0]
                self.assertTrue((repo / source).is_file(), f"對照表指到不存在的來源：{source}")

    # ── systemd user unit（issue #677，Linux 主機）──────────────────────────────
    SERVICE = (
        "# 註解不算語意\n[Unit]\nDescription=x\n\n[Service]\nType=oneshot\n"
        "Environment=PATH=/opt/repo\nExecStart=/bin/bash %h/AGM/bin/x.sh\n"
        "StandardOutput=append:%h/AGM/x.systemd.log\n"
    )
    TIMER = "[Unit]\nDescription=x\n\n[Timer]\nOnActiveSec={0}s\nOnUnitActiveSec={0}s\n\n[Install]\nWantedBy=timers.target\n"

    def with_units(self):
        """對照表同時有 plist（darwin）、unit（linux）與一支只在 darwin 裝的腳本，平台切到 linux。"""
        os.environ["AGM_OPS_PLATFORM"] = "linux"
        self.put("scripts/ops/install-manifest.tsv",
                 "scripts/ops/a.sh bin/a.sh\nscripts/ops/b.sh bin/b.sh\nscripts/ops/c.sh bin/c.sh darwin\n"
                 "scripts/ops/t.md t.md\n"
                 "scripts/ops/launchd/com.agm.x.plist LaunchAgents/com.agm.x.plist darwin\n"
                 "scripts/ops/systemd/com.agm.x.service systemd/com.agm.x.service linux\n"
                 "scripts/ops/systemd/com.agm.x.timer systemd/com.agm.x.timer linux\n")
        self.put("scripts/ops/launchd/com.agm.x.plist", self.plist("com.agm.x", 1800))
        self.put("scripts/ops/systemd/com.agm.x.service", self.SERVICE)
        self.put("scripts/ops/systemd/com.agm.x.timer", self.TIMER.format(1800))
        self.git("add", "-A")
        self.git("commit", "-qam", "加 unit")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.install("bin/a.sh", "a v3\n")
        self.install("bin/b.sh", "b v1\n")
        self.install("t.md", "task\n")

    def test_linux_compares_units_and_skips_the_darwin_rows(self):
        """Linux 上只看 unit 與不分平台的列：plist 與 `darwin` 列報成 skipped、不是 missing。
        unit 的註解、空行、`Environment=` 的值都不算落差（安裝端 PATH 是那台機器的）。"""
        self.with_units()
        (self.units / "com.agm.x.service").write_text(
            self.SERVICE.replace("# 註解不算語意\n", "").replace("PATH=/opt/repo", "PATH=/home/u/.local/bin:/usr/bin")
            .replace("[Service]\n", "\n[Service]\n; 另一種註解\n"), encoding="utf-8")
        (self.units / "com.agm.x.timer").write_text(self.TIMER.format(1800), encoding="utf-8")
        r = self.ok("ops-sync", "--check", "--repo", str(self.repo))
        self.assertTrue(r["in_sync"], r)
        self.assertEqual(r["platform"], "linux")
        self.assertEqual(sorted(x["target"] for x in r["ok"]),
                         ["bin/a.sh", "bin/b.sh", "systemd/com.agm.x.service", "systemd/com.agm.x.timer", "t.md"])
        self.assertEqual(sorted(x["target"] for x in r["skipped"]), ["LaunchAgents/com.agm.x.plist", "bin/c.sh"])

    def test_linux_unit_interval_and_dropped_environment_are_drift(self):
        """#487 的本體換到 systemd：間隔改掉要講出兩邊的值；`Environment=` 整行掉了（job 少了 PATH）也是落差。"""
        self.with_units()
        (self.units / "com.agm.x.service").write_text(
            self.SERVICE.replace("Environment=PATH=/opt/repo\n", ""), encoding="utf-8")
        (self.units / "com.agm.x.timer").write_text(self.TIMER.format(600), encoding="utf-8")
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        r = json.loads(out)
        rows = {x["target"]: x["diff"] for x in r["drift"]}
        self.assertEqual(rows["systemd/com.agm.x.timer"]["Timer.OnUnitActiveSec"], {"repo": ["1800s"], "installed": ["600s"]})
        self.assertEqual(rows["systemd/com.agm.x.service"]["Service.Environment"], {"repo": "<ignored>", "installed": None})

    def report(self):
        code, out, err = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertIn(code, (0, 1), err)
        return json.loads(out)

    def test_a_unit_or_plist_that_is_an_older_repo_version_is_behind_not_drift(self):
        """repo 把 unit 加了一行 `Environment=`（browser-gc.service 的 HERDR_SESSION，420ac4ab），安裝端還是上一版：
        那是「repo 較新、還沒裝」（behind，附落後的 commit），不是「有人直接改了安裝檔」（drift）。以前 unit／plist 只跟最新一版
        比語意，沒有像腳本那樣回頭找歷史，於是 ops-install 的 drift 保護（絕不覆蓋被手改的檔）會把這種正常落後的檔也擋下來。"""
        self.with_units()
        old_service = self.SERVICE.replace("Environment=PATH=/opt/repo\n", "")
        old_plist = self.plist("com.agm.x", 900)
        # 歷史：unit 先沒有 Environment、plist 先是 900 秒；之後才改成現在的樣子。
        self.put("scripts/ops/systemd/com.agm.x.service", old_service)
        self.put("scripts/ops/launchd/com.agm.x.plist", old_plist)
        self.git("commit", "-qam", "unit/plist 舊版")
        self.put("scripts/ops/systemd/com.agm.x.service", self.SERVICE)
        self.put("scripts/ops/launchd/com.agm.x.plist", self.plist("com.agm.x", 1800))
        self.git("commit", "-qam", "unit 加 Environment、plist 改 1800")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        (self.units / "com.agm.x.service").write_text(old_service, encoding="utf-8")
        (self.units / "com.agm.x.timer").write_text(self.TIMER.format(1800), encoding="utf-8")
        r = self.report()
        self.assertEqual(r["drift"], [], r)
        [behind] = r["behind"]
        self.assertEqual((behind["target"], behind["behind"]), ("systemd/com.agm.x.service", 1))
        self.assertEqual([c.split(" ", 1)[1] for c in behind["commits"]], ["unit 加 Environment、plist 改 1800"])
        self.assertTrue(behind["installed_commit"])
        self.assertFalse(r["in_sync"])

        # 手改過的（不是 repo 任何一版）仍然是 drift，跟 behind 分得開。
        (self.units / "com.agm.x.service").write_text(old_service.replace("Type=oneshot", "Type=simple"), encoding="utf-8")
        r = self.report()
        self.assertEqual([x["target"] for x in r["drift"]], ["systemd/com.agm.x.service"])
        self.assertEqual(r["behind"], [])

        # plist 同理（macOS）。
        os.environ["AGM_OPS_PLATFORM"] = "darwin"
        (self.agents / "com.agm.x.plist").write_text(old_plist, encoding="utf-8")
        r = self.report()
        [behind] = [x for x in r["behind"] if x["target"] == "LaunchAgents/com.agm.x.plist"]
        self.assertEqual(behind["behind"], 1)
        self.assertEqual([x["target"] for x in r["drift"]], [])

    def test_ops_sync_follows_a_renamed_source_when_classifying_an_installed_version(self):
        """來源檔改名但安裝目標沒變時，舊安裝內容仍是 repo 曾發布過的版本，應判 behind 而非 drift。"""
        old_source = "scripts/ops/old-a.sh"
        new_source = "scripts/ops/new-a.sh"
        manifest = lambda source: (
            f"{source} bin/a.sh\nscripts/ops/b.sh bin/b.sh\nscripts/ops/c.sh bin/c.sh\nscripts/ops/t.md t.md\n"
        )
        self.put("scripts/ops/install-manifest.tsv", manifest(old_source))
        self.put(old_source, "#!/bin/sh\necho old release\nexit 0\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "publish old source")
        old_commit = self.git("rev-parse", "--short=8", "HEAD")
        self.install("bin/a.sh", "#!/bin/sh\necho old release\nexit 0\n")

        (self.repo / old_source).rename(self.repo / new_source)
        self.put(new_source, "#!/bin/sh\necho new release\nexit 0\n")
        self.put("scripts/ops/install-manifest.tsv", manifest(new_source))
        self.git("add", "-A")
        self.git("commit", "-qm", "rename and update source")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")

        report = self.report()
        [row] = [row for row in report["behind"] if row["target"] == "bin/a.sh"]
        self.assertEqual(row["installed_commit"], old_commit)
        self.assertEqual(row["behind"], 1)
        self.assertNotIn("bin/a.sh", [item["target"] for item in report["drift"]])

    def test_ops_sync_uses_manifest_history_when_git_cannot_detect_a_source_rename(self):
        """大幅重寫時 Git 不一定判成 rename；同一安裝目標的舊來源仍要算 behind。"""
        old_source = "scripts/ops/old-a.sh"
        new_source = "scripts/ops/new-a.sh"
        manifest = lambda source: (
            f"{source} bin/a.sh\nscripts/ops/b.sh bin/b.sh\nscripts/ops/c.sh bin/c.sh\nscripts/ops/t.md t.md\n"
        )
        old_contents = "#!/bin/sh\n" + ("echo legacy-version-1234567890\n" * 20)
        self.put("scripts/ops/install-manifest.tsv", manifest(old_source))
        self.put(old_source, old_contents)
        self.git("add", "-A")
        self.git("commit", "-qm", "publish old source")
        old_commit = self.git("rev-parse", "--short=8", "HEAD")
        self.install("bin/a.sh", old_contents)

        (self.repo / old_source).unlink()
        self.put(new_source, "#!/bin/sh\nprintf 'unrelated rewritten source\\n'\n" * 20)
        self.put("scripts/ops/install-manifest.tsv", manifest(new_source))
        self.git("add", "-A")
        self.git("commit", "-qm", "replace source without recognizable rename")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")

        report = self.report()
        [row] = [row for row in report["behind"] if row["target"] == "bin/a.sh"]
        self.assertEqual(row["installed_commit"], old_commit)
        self.assertEqual(row["behind"], 1)
        self.assertNotIn("bin/a.sh", [item["target"] for item in report["drift"]])

    def test_ops_sync_does_not_count_an_unmapped_reused_source_as_behind(self):
        old_source = "scripts/ops/old-a.sh"
        new_source = "scripts/ops/new-a.sh"
        manifest = lambda source: (
            f"{source} bin/a.sh\nscripts/ops/b.sh bin/b.sh\nscripts/ops/c.sh bin/c.sh\nscripts/ops/t.md t.md\n"
        )
        self.put("scripts/ops/install-manifest.tsv", manifest(old_source))
        self.put(old_source, "#!/bin/sh\necho published\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "publish old source")
        (self.repo / old_source).unlink()
        self.put(new_source, "#!/bin/sh\necho current source\n")
        self.put("scripts/ops/install-manifest.tsv", manifest(new_source))
        self.git("add", "-A")
        self.git("commit", "-qm", "move target mapping")

        unpublished = "#!/bin/sh\nprintf 'hand edited bytes\\n'\n"
        self.put(old_source, unpublished)
        self.git("add", "-A")
        self.git("commit", "-qm", "reuse retired source path")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.install("bin/a.sh", unpublished)

        report = self.report()
        self.assertIn("bin/a.sh", [item["target"] for item in report["drift"]])
        self.assertNotIn("bin/a.sh", [item["target"] for item in report["behind"]])

    def test_linux_reports_unlisted_agm_units_and_darwin_only_scripts_as_extra(self):
        """Linux 上沒版控的 `com.agm.*` unit＝extra；`~/Library/LaunchAgents` 不掃（那不是這台的排程）。
        只在 darwin 裝的 browser-gc 腳本出現在 Linux 的 bin/，也算 extra——它不該被裝在這裡。"""
        self.with_units()
        (self.units / "com.agm.x.service").write_text(self.SERVICE, encoding="utf-8")
        (self.units / "com.agm.x.timer").write_text(self.TIMER.format(1800), encoding="utf-8")
        (self.units / "com.agm.ghost.timer").write_text(self.TIMER.format(60), encoding="utf-8")
        (self.units / "other.service").write_text(self.SERVICE, encoding="utf-8")
        (self.agents / "com.agm.ghost.plist").write_text(self.plist("com.agm.ghost", 60), encoding="utf-8")
        self.install("bin/c.sh", "c v1\n")
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        self.assertEqual(sorted(x["target"] for x in json.loads(out)["extra"]), ["bin/c.sh", "systemd/com.agm.ghost.timer"])

    def test_an_unreadable_unit_is_drift_not_a_crash(self):
        self.with_units()
        (self.units / "com.agm.x.service").write_text("ExecStart=before any section\n", encoding="utf-8")
        (self.units / "com.agm.x.timer").write_text(self.TIMER.format(1800), encoding="utf-8")
        code, out, _ = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
        self.assertEqual(code, 1)
        [row] = [x for x in json.loads(out)["drift"] if x["target"] == "systemd/com.agm.x.service"]
        self.assertIn("_error", row["diff"])

    def test_a_scheduler_row_on_the_wrong_platform_is_a_bad_manifest(self):
        """plist 沒標 darwin、unit 標成 darwin 都是對照表寫錯，不是「這台不裝」：拒絕，別報一堆 missing。"""
        for line in ("scripts/ops/launchd/com.agm.x.plist LaunchAgents/com.agm.x.plist\n",
                     "scripts/ops/systemd/com.agm.x.timer systemd/com.agm.x.timer darwin\n",
                     "scripts/ops/a.sh bin/a.sh windows\n"):
            with self.subTest(line=line):
                self.put("scripts/ops/install-manifest.tsv", line)
                self.git("commit", "-qam", f"壞對照表 {line}")
                self.git("update-ref", "refs/remotes/origin/main", "HEAD")
                code, _out, err = self.run_cli("ops-sync", "--check", "--repo", str(self.repo))
                self.assertEqual(code, 2, err)
                self.assertIn("bad_manifest", err)


class SystemdParityTest(unittest.TestCase):
    """issue #677：每支 launchd job 都有 systemd .service＋.timer；GUI 工作可用 Linux 專用 worker。

    排程、log 與環境需對齊；一般 job 跑同一支安裝好的 kick，browser-gc 使用 Linux 非 GUI worker。
    StartInterval、RunAtLoad 與 plist 的 EnvironmentVariables 都由測試釘住。
    改了其中一邊忘了另一邊，這裡就紅。
    """

    REPO = Path(__file__).resolve().parent.parent
    #: Linux 與 macOS 的工作內容不同，但排程仍成對；GUI child 留在 macOS。
    LINUX_EXEC = {
        "browser-gc": ["/usr/bin/python3", "%h/.config/agents-manager/supervisor/AGM/bin/browser_gc_linux.py"],
    }
    DARWIN_ONLY_SOURCES = {
        "browser-gc": ("scripts/ops/browser-gc-kick.sh", "scripts/ops/browser-gc-task.md",
                       "scripts/ops/launchd/com.agm.browser-gc.plist"),
    }
    #: 只有 systemd 需要講的屬性。dev-server 的 kick 用 detached 拉起 vite：launchd 看程序群，脫離了就活著；
    #: systemd 看 cgroup，預設 KillMode=control-group 會在 kick 一結束就把 vite 一起收掉。
    SYSTEMD_ONLY = {"dev-server": {"Service.KillMode": ["process"]}}
    #: 只有 unit 帶的環境變數。Linux 主機只跑 daemon 用的 `agents-manager` session，沒有 default server；
    #: 排程沒有 pane 環境，daemon-swap 的 `herdr pane list` 不指定 session 會回 server_not_running。
    #: macOS 使用者自己的 default session 一直開著，plist 不需要。
    #: browser-gc 的 Linux worker 尾端會跑 pane-gc.sh（`herdr pane list`），同樣需要指到那個 session。
    SYSTEMD_ONLY_ENV = {"daemon-update": {"HERDR_SESSION"}, "browser-gc": {"HERDR_SESSION"}}

    @staticmethod
    def home_relative(arg: str) -> str:
        return re.sub(r"^/Users/[^/]+/", "%h/", arg)

    def units(self, name: str) -> tuple[dict, dict]:
        d = self.REPO / "scripts/ops/systemd"
        return (agm._unit_semantics((d / f"com.agm.{name}.service").read_bytes()),
                agm._unit_semantics((d / f"com.agm.{name}.timer").read_bytes()))

    def test_linux_browser_gc_uses_the_non_gui_worker(self):
        rows = {}
        for line in (self.REPO / agm.OPS_MANIFEST).read_text(encoding="utf-8").splitlines():
            if line.strip() and not line.startswith("#"):
                source, target, *platform = line.split()
                rows[source] = (target, platform[0] if platform else None)
        self.assertEqual(rows.get("scripts/ops/browser_gc_linux.py"), ("bin/browser_gc_linux.py", "linux"))
        for source in ("scripts/ops/browser-gc-kick.sh", "scripts/ops/browser-gc-task.md",
                       "scripts/ops/launchd/com.agm.browser-gc.plist"):
            self.assertEqual(rows.get(source, (None, None))[1], "darwin")

        service, timer = self.units("browser-gc")
        self.assertEqual(service["Service.Type"], ["oneshot"])
        self.assertEqual(service["Service.ExecStart"],
                         ["/usr/bin/python3 %h/.config/agents-manager/supervisor/AGM/bin/browser_gc_linux.py"])
        self.assertEqual(timer["Timer.OnUnitActiveSec"], ["1800s"])
        self.assertEqual(timer["Timer.OnActiveSec"], ["1800s"])

    def test_every_launchd_job_has_an_equivalent_unit_pair(self):
        import plistlib
        plists = sorted((self.REPO / "scripts/ops/launchd").glob("com.agm.*.plist"))
        self.assertEqual(len(plists), 8)
        for p in plists:
            name = p.name[len("com.agm."):-len(".plist")]
            with self.subTest(job=name):
                pl = plistlib.loads(p.read_bytes())
                svc, tmr = self.units(name)
                self.assertEqual(svc["Service.Type"], ["oneshot"])
                [exec_start] = svc["Service.ExecStart"]
                want = [self.home_relative(a) for a in pl["ProgramArguments"]]
                got = exec_start.split()
                if name in self.LINUX_EXEC:
                    self.assertEqual(got, self.LINUX_EXEC[name])
                else:
                    # 直譯器在兩個平台的位置不同（Homebrew 的 bun vs ~/.bun/bin），比名字；其餘逐字。
                    self.assertEqual(Path(got[0]).name, Path(want[0]).name)
                    self.assertEqual(got[1:], want[1:])
                self.assertEqual(tmr["Timer.OnUnitActiveSec"], [f"{pl['StartInterval']}s"])
                self.assertEqual(tmr["Timer.OnActiveSec"], ["1s" if pl.get("RunAtLoad") else f"{pl['StartInterval']}s"])
                self.assertEqual(tmr["Install.WantedBy"], ["timers.target"])
                log = self.home_relative(pl["StandardOutPath"]).replace(".launchd.log", ".systemd.log")
                self.assertEqual(svc["Service.StandardOutput"], [f"append:{log}"])
                self.assertEqual(svc["Service.StandardError"], [f"append:{log}"])
                # 值被 `_unit_semantics` 忽略了，這裡直接讀原文比變數名。
                raw = (self.REPO / f"scripts/ops/systemd/com.agm.{name}.service").read_text(encoding="utf-8")
                env = sorted(m.split("=", 1)[0] for m in re.findall(r"^Environment=(\S+)", raw, re.M))
                extra = self.SYSTEMD_ONLY_ENV.get(name, set())
                self.assertTrue(extra <= set(env), f"{name} 的 unit 少了 {extra - set(env)}")
                self.assertEqual([k for k in env if k not in extra], sorted(pl.get("EnvironmentVariables", {})))
                for key, want_value in self.SYSTEMD_ONLY.get(name, {}).items():
                    self.assertEqual(svc.get(key), want_value, key)

    def test_every_unit_is_in_the_manifest_as_linux_and_darwin_only_rows_are_marked(self):
        rows = {}
        for line in (self.REPO / agm.OPS_MANIFEST).read_text(encoding="utf-8").splitlines():
            if line.strip() and not line.startswith("#"):
                parts = line.split()
                rows[parts[0]] = parts[2] if len(parts) == 3 else None
        for u in sorted((self.REPO / "scripts/ops/systemd").iterdir()):
            with self.subTest(unit=u.name):
                self.assertEqual(rows.get(f"scripts/ops/systemd/{u.name}"), "linux")
        for sources in self.DARWIN_ONLY_SOURCES.values():
            for src in sources:
                with self.subTest(src=src):
                    self.assertEqual(rows.get(src), "darwin")


# ------------------------------------------------------------- issue 認領（#425）

# 假 gh：`issue view` 吐 $GH_STATE/issue-<n>.json，其餘子命令只記進 $GH_STATE/calls.log。
# GH_FAIL=<子命令> 讓那一個子命令失敗；GH_NO_LABEL=1 模擬 label 還不存在（`issue edit --add-label` 先失敗）。
# GH_INJECT_* 模擬 claim/release 的留言與 label API 之間有另一個操作插隊。
# 真的 gh 不可達：PATH 只留這個目錄與 /usr/bin:/bin，而且這支 stub 不認得的子命令一律 exit 2。
FAKE_GH = r"""#!/usr/bin/env python3
import datetime, json, os, re, sys
state = os.environ["GH_STATE"]
argv = sys.argv[1:]
with open(os.path.join(state, "calls.log"), "a") as f:
    f.write(json.dumps(argv) + "\n")   # 一行一筆 JSON：--body 裡有換行也不會把記錄切斷
sub = " ".join(argv[:2])
fail = os.environ.get("GH_FAIL")
if fail in (sub, argv[0]) or (fail == "api_after_comment" and argv[0] == "api"
                               and os.path.exists(os.path.join(state, "comment-written"))):
    sys.stderr.write(os.environ.get("GH_FAIL_MSG", "gh: boom") + "\n")
    sys.exit(1)
if sub == "issue view":
    n = argv[2]
    try:
        sys.stdout.write(open(os.path.join(state, "issue-%s.json" % n)).read())
    except FileNotFoundError:
        sys.stderr.write("gh: no issue %s\n" % n)
        sys.exit(1)
elif argv[0] == "api":
    path = next((a for a in argv if "/issues/" in a and "/comments" in a), "")
    match = re.search(r"/issues/(\d+)/comments", path)
    if not match:
        sys.stderr.write("gh: unsupported api path\n")
        sys.exit(2)
    raw = json.load(open(os.path.join(state, "issue-%s.json" % match.group(1))))
    sys.stdout.write(json.dumps(raw.get("comments", [])))
elif sub in ("issue comment", "issue edit", "label create"):
    if sub == "label create" and os.environ.get("GH_NO_LABEL"):
        sys.stderr.write("gh: label already exists\n")
        sys.exit(1)
    if sub == "issue comment":
        n = argv[2]
        path = os.path.join(state, "issue-%s.json" % n)
        raw = json.load(open(path))
        comments = raw.setdefault("comments", [])
        now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
        next_id = max((int(c.get("id", 0)) for c in comments), default=100) + 1
        injected = os.environ.get("GH_INJECT_COMPETITOR")
        if injected and not os.path.exists(os.path.join(state, "injected")):
            payload = {"bot": injected, "at": now, "claim_id": "race-competitor"}
            comments.append({"id": next_id, "created_at": now, "body": "<!-- agm:issue-claim %s -->" % json.dumps(payload)})
            next_id += 1
            open(os.path.join(state, "injected"), "w").close()
        body = argv[argv.index("--body") + 1]
        comments.append({"id": next_id, "created_at": now, "body": body})
        raw["updatedAt"] = now
        json.dump(raw, open(path, "w"))
        open(os.path.join(state, "comment-written"), "w").close()
    elif sub == "issue edit":
        n = argv[2]
        path = os.path.join(state, "issue-%s.json" % n)
        raw = json.load(open(path))
        labels = [x.get("name") for x in raw.get("labels", [])]
        original_labels = list(labels)
        comments = raw.setdefault("comments", [])
        now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
        next_id = max((int(c.get("id", 0)) for c in comments), default=100) + 1
        if "--remove-label" in argv:
            injected = os.environ.get("GH_INJECT_NEW_CLAIM_ON_REMOVE")
            if injected and not os.path.exists(os.path.join(state, "new-claim-injected")):
                payload = {"bot": injected, "at": now, "claim_id": "race-new-claim", "observed_updated_at": raw.get("updatedAt")}
                comments.append({"id": next_id, "created_at": now, "body": "<!-- agm:issue-claim %s -->" % json.dumps(payload)})
                if "wip" not in labels:
                    labels.append("wip")
                raw["updatedAt"] = now
                open(os.path.join(state, "new-claim-injected"), "w").close()
            labels = [name for name in labels if name != argv[argv.index("--remove-label") + 1]]
        elif "--add-label" in argv:
            injected = os.environ.get("GH_INJECT_RELEASE_ON_ADD")
            if injected and not os.path.exists(os.path.join(state, "release-injected")):
                claim = next((c for c in reversed(comments) if "agm:issue-claim" in (c.get("body") or "")), None)
                match = re.search(r"<!-- agm:issue-claim (\{.*\}) -->", claim.get("body", "")) if claim else None
                if match:
                    payload = json.loads(match.group(1))
                    target = payload.get("claim_id") or str(claim.get("id"))
                    release = {"bot": "race-release", "at": now, "claim_id": str(target)}
                    comments.append({"id": next_id, "created_at": now, "body": "<!-- agm:issue-release %s -->" % json.dumps(release)})
                    raw["updatedAt"] = now
                    labels = [name for name in labels if name != "wip"]
                    open(os.path.join(state, "release-injected"), "w").close()
            label = argv[argv.index("--add-label") + 1]
            if label not in labels:
                labels.append(label)
        raw["labels"] = [{"name": name} for name in labels]
        if labels != original_labels:
            raw["updatedAt"] = now
        json.dump(raw, open(path, "w"))
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

    ENV_KEYS = ("PATH", "GH_STATE", "GH_FAIL", "GH_FAIL_MSG", "GH_NO_LABEL", "GH_INJECT_COMPETITOR",
                "GH_INJECT_NEW_CLAIM_ON_REMOVE", "GH_INJECT_RELEASE_ON_ADD", "AM_AGENT_NAME", "AM_BOT_ID", "AGM_RUNTIME_DIR")

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
        # canary 的護欄（`check.sh ops` arm 起來的那個）要留在最前面：python 的 subprocess 走
        # execvp，不經過 shell 的指令查找，所以三層防護只剩 PATH 這一層；整個蓋掉就等於在沒有
        # 保護的情況下跑（scripts/ops/destructive-canary.sh）。沒 arm 時這個變數是空的，照舊。
        guard = os.environ.get("AM_CANARY_DIR", "")
        os.environ["PATH"] = ":".join(p for p in (guard, str(bindir), "/usr/bin", "/bin") if p)
        os.environ["GH_STATE"] = str(self.state)
        os.environ["AM_AGENT_NAME"] = "vvyyg1"
        for k in ("GH_FAIL", "GH_FAIL_MSG", "GH_NO_LABEL", "GH_INJECT_NEW_CLAIM_ON_REMOVE", "GH_INJECT_RELEASE_ON_ADD", "AM_BOT_ID"):
            os.environ.pop(k, None)
        # runtime.json 故意不存在：issue 這條路不該去讀它。
        os.environ["AGM_RUNTIME_DIR"] = str(Path(self.dir.name) / "no-such-runtime")

    # --- 造題 ---

    def issue(self, number: int, *, labels=(), comments=(), updated: str | None = None, title="某張票"):
        comments = [dict(c, id=c.get("id", 100 + i), created_at=c.get("created_at", c.get("createdAt")))
                    for i, c in enumerate(comments)]
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
            if call[0] == "api":
                self.assertTrue(any(a.startswith("repos/o/r/issues/425/comments?") for a in call),
                                f"gh api 要把 repo 放進 endpoint：{call}")
            else:
                self.assertEqual(call[call.index("-R") + 1], "o/r", f"--repo 要傳給每一次 gh：{call}")

    def test_same_second_claims_are_ordered_by_comment_id_and_first_claim_wins(self):
        stamp = _iso(-10)
        comments = [
            {"id": 202, "created_at": stamp, "body": '<!-- agm:issue-claim {"bot":"second"} -->'},
            {"id": 201, "created_at": stamp, "body": '<!-- agm:issue-claim {"bot":"first"} -->'},
        ]
        claim = agm.current_claim({"comments": comments, "updatedAt": stamp})
        self.assertEqual(claim["bot"], "first")

    def test_only_repo_insiders_can_write_claim_or_release_markers(self):
        """公開 repo 上誰都能留言：陌生人貼的認領標記不能鎖票，貼的交回標記不能撤掉別人的認領。"""
        stamp = _iso(-10)
        outsider = lambda i, body, who="NONE": {"id": i, "created_at": stamp, "author_association": who, "body": body}
        insider = lambda i, body, who="COLLABORATOR": {"id": i, "created_at": _iso(-60), "author_association": who, "body": body}
        # 只有陌生人的標記：沒人認領。
        self.assertIsNone(agm.current_claim({"comments": [outsider(1, '<!-- agm:issue-claim {"bot":"evil"} -->')], "updatedAt": stamp}))
        self.assertIsNone(agm.current_claim({"comments": [outsider(1, '<!-- agm:issue-claim {"bot":"evil"} -->', "CONTRIBUTOR")], "updatedAt": stamp}))
        # 自己人認領之後，陌生人貼交回／搶認領都沒用。
        comments = [
            insider(10, '<!-- agm:issue-claim {"bot":"real"} -->', "OWNER"),
            outsider(11, '<!-- agm:issue-release {"bot":"real"} -->'),
            outsider(12, '<!-- agm:issue-claim {"bot":"evil"} -->', "FIRST_TIME_CONTRIBUTOR"),
        ]
        claim = agm.current_claim({"comments": comments, "updatedAt": stamp})
        self.assertEqual(claim["bot"], "real")
        # MEMBER／COLLABORATOR／OWNER 都算自己人；沒帶欄位的舊格式照舊採信。
        for who in ("MEMBER", "COLLABORATOR", "OWNER"):
            self.assertEqual(agm.current_claim({"comments": [insider(1, '<!-- agm:issue-claim {"bot":"x"} -->', who)], "updatedAt": stamp})["bot"], "x")

    def test_first_claim_after_release_wins_the_new_claim_epoch(self):
        stamp = _iso(-10)
        comments = [
            {"id": 301, "created_at": _iso(-40), "body": '<!-- agm:issue-claim {"bot":"old"} -->'},
            {"id": 302, "created_at": _iso(-30), "body": '<!-- agm:issue-release {"bot":"old"} -->'},
            {"id": 304, "created_at": stamp, "body": '<!-- agm:issue-claim {"bot":"second"} -->'},
            {"id": 303, "created_at": stamp, "body": '<!-- agm:issue-claim {"bot":"first"} -->'},
        ]
        claim = agm.current_claim({"comments": comments, "updatedAt": stamp})
        self.assertEqual(claim["bot"], "first")

    def test_claim_rereads_comments_and_loser_releases_its_claim_with_exit_3(self):
        self.issue(425)
        os.environ["GH_INJECT_COMPETITOR"] = "race-winner"
        code, _out, err = self.run_cli("issue", "claim", "425")
        self.assertEqual(code, 3, err)
        self.assertEqual(json.loads(err)["claimed_by"], "race-winner")
        calls = self.calls()
        self.assertGreaterEqual(sum(call[0] == "api" for call in calls), 2, "認領前後都要讀留言")
        bodies = [call[call.index("--body") + 1] for call in calls if call[:2] == ["issue", "comment"]]
        self.assertEqual(len(bodies), 2, "輸家要追加交回標記")
        self.assertIn("agm:issue-release", bodies[-1])
        self.assertIn('"claim_id"', bodies[-1])
        issue = json.loads((self.state / "issue-425.json").read_text())
        self.assertEqual(agm.current_claim(issue)["bot"], "race-winner", "輸家的交回不可清掉勝者")

    def test_claim_readback_failure_never_reports_success(self):
        self.issue(425)
        os.environ["GH_FAIL"] = "api_after_comment"
        code, _out, err = self.run_cli("issue", "claim", "425")
        self.assertNotEqual(code, 0, err)
        bodies = [call[call.index("--body") + 1] for call in self.calls() if call[:2] == ["issue", "comment"]]
        self.assertEqual(len(bodies), 2, "回讀失敗後要用 nonce 追加補償交回標記")
        self.assertIn("agm:issue-release", bodies[-1])

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

    def test_stale_claim_without_wip_label_can_still_be_taken_over(self):
        old = _iso(-30 * 3600)
        self.issue(413, comments=[self.claim_comment("kd61te", age_secs=30 * 3600)], updated=old)

        code, out, err = self.run_cli("issue", "claim", "413")

        self.assertEqual(code, 0, err)
        self.assertEqual(json.loads(out)["took_over_stale_claim_from"], "kd61te")
        self.assertEqual(agm.current_claim(json.loads((self.state / "issue-413.json").read_text()))["bot"], "vvyyg1")

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

    def test_same_bot_cannot_hand_one_issue_to_a_second_child(self):
        self.issue(425, labels=["wip"], comments=[self.claim_comment("vvyyg1", age_secs=300, child="child-a", worktree="/w/a", branch="fix/a")])
        code, out, err = self.run_cli("issue", "claim", "425", "--child", "child-b", "--worktree", "/w/b", "--branch", "fix/b")
        self.assertEqual(code, 3, err)
        e = json.loads(err)
        self.assertEqual(e["error"], "issue_claimed")
        self.assertEqual(e["claimed_by"], "vvyyg1")
        self.assertIn("child-a", e["message"])
        self.assertFalse([c for c in self.calls() if c[:2] in (["issue", "comment"], ["issue", "edit"])], "第二個 child 不能再寫一筆認領")

    def test_same_child_reclaim_stays_idempotent(self):
        self.issue(425, labels=["wip"], comments=[self.claim_comment("vvyyg1", age_secs=300, child="child-a")])
        got = json.loads(self.run_cli("issue", "claim", "425", "--child", "child-a")[1])
        self.assertTrue(got["already"])

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
        self.assertIn('"claim_id"', body)
        self.assertEqual([c[-2:] for c in self.calls() if c[:2] == ["issue", "edit"]], [["--remove-label", "wip"]])

    def test_release_reconciles_wip_when_a_new_claim_lands_during_label_removal(self):
        self.issue(425, labels=["wip"], comments=[self.claim_comment("vvyyg1", age_secs=300)])
        os.environ["GH_INJECT_NEW_CLAIM_ON_REMOVE"] = "race-winner"

        code, out, err = self.run_cli("issue", "release", "425")

        self.assertEqual(code, 0, err)
        self.assertTrue(json.loads(out)["released"])
        issue = json.loads((self.state / "issue-425.json").read_text())
        self.assertEqual(agm.current_claim(issue)["bot"], "race-winner")
        self.assertIn("wip", [label["name"] for label in issue["labels"]], "release must not remove the winner's label")

    def test_claim_does_not_report_success_if_released_before_its_label_write(self):
        self.issue(425)
        os.environ["GH_INJECT_RELEASE_ON_ADD"] = "1"

        code, _out, err = self.run_cli("issue", "claim", "425")

        self.assertEqual(code, 3, err)
        self.assertEqual(json.loads(err)["error"], "issue_claim_lost")
        issue = json.loads((self.state / "issue-425.json").read_text())
        self.assertIsNone(agm.current_claim(issue))
        self.assertNotIn("wip", [label["name"] for label in issue["labels"]], "a released marker must not leave a stale label")

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

    def test_a_broken_claim_marker_with_wip_fails_closed_until_released(self):
        self.issue(425, labels=["wip"], comments=[{"createdAt": _iso(-600), "body": "派給 x\n<!-- agm:issue-claim {oops -->"}])
        code, out, err = self.run_cli("issue", "claim", "425")
        self.assertEqual(code, 3, out)
        self.assertEqual(json.loads(err)["error"], "issue_claim_state_unknown")
        self.assertFalse([c for c in self.calls() if c[:2] in (["issue", "comment"], ["issue", "edit"])],
                         "marker 壞掉但 wip 還在時不能再派一顆")

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

    # --- 對抗式審查（認領協定） ---

    def test_a_quoted_marker_in_a_reply_is_not_a_claim_or_a_release(self):
        """GitHub 的「引用回覆」會把原留言的原始 markdown（含 HTML 註解）貼進新留言。引用別人的交回標記不能把那顆的認領清掉，
        引用認領標記也不能自己變成一筆認領。只有『標記自己一行』的留言才算（agm 自己寫的就是這個形狀）。"""
        quoted_release = {"createdAt": _iso(-100), "body": "> 交回。\n>\n> <!-- agm:issue-release {\"bot\":\"kd61te\"} -->\n\n我覺得還沒做完"}
        self.issue(413, labels=["wip"], comments=[self.claim_comment("kd61te", age_secs=600), quoted_release])
        code, _out, err = self.run_cli("issue", "claim", "413")
        self.assertEqual(code, 3, "引用的交回標記不算交回：kd61te 還按著")
        self.assertEqual(json.loads(err)["claimed_by"], "kd61te")

        quoted_claim = {"createdAt": _iso(-100), "body": "> <!-- agm:issue-claim {\"bot\":\"ghost\"} -->\n這個我來吧"}
        self.issue(414, comments=[quoted_claim])
        self.assertEqual(self.run_cli("issue", "claim", "414")[0], 0, "引用的認領標記不算認領")

        inline = {"createdAt": _iso(-100), "body": "文字 <!-- agm:issue-claim {\"bot\":\"ghost\"} --> 還有文字"}
        self.issue(415, comments=[inline])
        self.assertEqual(self.run_cli("issue", "claim", "415")[0], 0, "夾在句子中間的也不算")

    def test_a_label_only_release_cannot_erase_a_claim_that_lands_in_between(self):
        """票上有 wip 但沒有任何認領留言（上次交回只拿掉了一半、或人手貼的）：release 的標記沒有 claim_id 時，
        是『交回當下有效的任何一筆』——在它讀完留言、寫入之前搶進來的別人的新認領，就被它悄悄清掉了。"""
        self.issue(425, labels=["wip"])
        os.environ["GH_INJECT_COMPETITOR"] = "race-winner"
        code, _out, err = self.run_cli("issue", "release", "425")
        self.assertEqual(code, 0, err)
        issue = json.loads((self.state / "issue-425.json").read_text())
        claim = agm.current_claim(issue)
        self.assertIsNotNone(claim, "別人剛搶到的認領被這次的 label-only release 清掉了")
        self.assertEqual(claim["bot"], "race-winner")

    def test_rate_limits_are_told_apart_from_a_plain_failure(self):
        """限流不是『沒人認領』：呼叫端要等一等再試，不能當成可以直接派工。種類獨立、標 retryable。"""
        self.issue(425)
        for msg in ("gh: API rate limit exceeded for user ID 1", "HTTP 403: You have exceeded a secondary rate limit", "HTTP 429: Too Many Requests"):
            os.environ["GH_FAIL"] = "issue view"
            os.environ["GH_FAIL_MSG"] = msg
            code, _out, err = self.run_cli("issue", "claim", "425")
            e = json.loads(err)
            self.assertEqual((code, e["error"], e.get("retryable")), (1, "gh_rate_limited", True), msg)
            self.assertIn("不是", e["message"])
        os.environ["GH_FAIL_MSG"] = "gh: could not resolve host"
        self.assertEqual(json.loads(self.run_cli("issue", "claim", "425")[2])["error"], "gh_failed")
        self.assertFalse([c for c in self.calls() if c[:2] == ["issue", "comment"]])

    def test_a_failed_label_after_winning_leaves_a_claim_that_a_retry_completes(self):
        self.issue(425)
        os.environ["GH_FAIL"] = "issue edit"
        code, _out, err = self.run_cli("issue", "claim", "425")
        self.assertEqual((code, json.loads(err)["error"]), (1, "gh_failed"))
        os.environ.pop("GH_FAIL")
        got = json.loads(self.run_cli("issue", "claim", "425")[1])
        self.assertTrue(got["already"], "重跑不再留第二則留言")
        self.assertEqual(sum(c[:2] == ["issue", "comment"] for c in self.calls()), 1)
        self.assertEqual([c[-2:] for c in self.calls() if c[:2] == ["issue", "edit"]][-1], ["--add-label", "wip"])

    def test_a_failed_label_removal_after_release_is_cleaned_up_by_a_retry(self):
        self.issue(425, labels=["wip"], comments=[self.claim_comment("vvyyg1", age_secs=300)])
        os.environ["GH_FAIL"] = "issue edit"
        self.assertEqual(self.run_cli("issue", "release", "425")[0], 1)
        os.environ.pop("GH_FAIL")
        got = json.loads(self.run_cli("issue", "release", "425")[1])
        self.assertTrue(got["released"])
        self.assertEqual([c[-2:] for c in self.calls() if c[:2] == ["issue", "edit"]][-1], ["--remove-label", "wip"])

    def test_hand_mangled_markers_never_crash_or_hand_over_the_claim(self):
        """人手改壞的標記：欄位型別亂掉、claim_id 是物件、時間不是時間。不能丟例外，也不能因此讓別人搶走活的認領。"""
        weird = [
            {"createdAt": _iso(-600), "body": '<!-- agm:issue-claim {"bot": ["a"], "claim_id": {"x": 1}, "observed_updated_at": 5} -->'},
            {"createdAt": _iso(-500), "body": '<!-- agm:issue-release {"claim_id": [1, 2]} -->'},
            {"createdAt": "not a time", "body": '<!-- agm:issue-release {"bot":"kd61te"} -->'},
            {"createdAt": _iso(-400), "body": '<!-- agm:issue-claim [1,2,3] -->'},
        ]
        self.issue(413, labels=["wip"], comments=[self.claim_comment("kd61te", age_secs=900)] + weird)
        code, _out, err = self.run_cli("issue", "claim", "413")
        self.assertEqual(code, 3, err)
        self.assertEqual(json.loads(err)["claimed_by"], "kd61te")

    def test_claim_never_touches_the_daemon_runtime(self):
        # AGM_RUNTIME_DIR 指向不存在的目錄：真的去讀 runtime.json 就會是 no_runtime／exit 2。
        self.issue(425)
        self.assertEqual(self.run_cli("issue", "claim", "425")[0], 0)


class AckPathTest(unittest.TestCase):
    """`agm ack <id>` 的 id 是一段路徑：不能讓 `/`、`?`、`#`、`..` 改變它打到哪一個端點。"""

    class Stub:
        def __init__(self):
            self.paths = []

        def post(self, path, body=None):
            self.paths.append(path)
            return {}

    def ack(self, event_id):
        stub = self.Stub()
        agm.cmd_ack(stub, {}, type("Args", (), {"event_id": event_id})())
        return stub.paths[0]

    def test_an_event_id_with_path_characters_stays_one_path_segment(self):
        for evil in ["a/b", "../../health", "a?x=1", "a#frag", "a b", "%2e%2e"]:
            path = self.ack(evil)
            self.assertTrue(path.startswith("/api/supervisor/inbox/") and path.endswith("/ack"), path)
            middle = path[len("/api/supervisor/inbox/") : -len("/ack")]
            self.assertNotIn("/", middle, f"{evil!r} 變成了好幾段：{path}")
            self.assertNotIn("?", middle)
            self.assertNotIn("#", middle)
            self.assertEqual(urllib.parse.unquote(middle), evil, "解回來還是原本那串")

    def test_a_real_ulid_is_unchanged(self):
        self.assertEqual(self.ack("01M3YJ0S4V0Y9004VFV0YJ386C"), "/api/supervisor/inbox/01M3YJ0S4V0Y9004VFV0YJ386C/ack")


class CliHardeningTest(CliCase):
    """審查補的：錯誤輸出的形狀、參數驗證、逾時、非 JSON 回應、Python 3.9 相容。"""

    def test_usage_errors_are_json_on_stderr_not_argparse_prose(self):
        for argv in (("messages",), ("assign",), ("nope-command",), ("messages", "b1", "--limit", "abc")):
            code, out, err = self.run_cli(*argv)
            self.assertEqual(code, 2, argv)
            self.assertEqual(out, "", argv)
            body = json.loads(err)
            self.assertEqual(body["error"], "bad_args", (argv, err))
            self.assertIn("message", body)

    def test_the_timeout_flag_must_be_a_sane_positive_number(self):
        for bad in ("0", "-5", "nan", "inf", "99999", "abc"):
            code, out, err = self.run_cli("--timeout", bad, "health")
            self.assertEqual(code, 2, bad)
            self.assertEqual(json.loads(err)["error"], "bad_args", bad)
        FakeDaemon.routes["GET /api/supervisor/health"] = (200, {"ok": True})
        self.assertEqual(self.ok("--timeout", "5", "health"), {"ok": True})

    def test_limit_flags_reject_non_positive_and_absurd_values(self):
        for argv in (("messages", "b1", "--limit", "0"), ("messages", "b1", "--limit", "-1"),
                     ("messages", "b1", "--limit", "100000"), ("search", "x", "--limit", "0")):
            code, _out, err = self.run_cli(*argv)
            self.assertEqual(code, 2, argv)
            self.assertEqual(json.loads(err)["error"], "bad_args", argv)
        self.assertEqual(FakeDaemon.seen, [], "參數錯誤不能打到 daemon")

    def test_ids_must_be_non_empty_single_line_values(self):
        for bad in ("", "  ", "a\nb", "a\x00b", "x" * 300):
            code, _out, err = self.run_cli("messages", bad)
            self.assertEqual(code, 2, repr(bad))
            self.assertEqual(json.loads(err)["error"], "bad_args", repr(bad))
        self.assertEqual(FakeDaemon.seen, [])

    def test_a_200_with_a_non_json_body_is_an_error_not_a_success(self):
        FakeDaemon.routes["GET /api/supervisor/health"] = (200, Raw("<html>some other service</html>"))
        code, out, err = self.run_cli("health")
        self.assertNotEqual(code, 0)
        self.assertEqual(out, "", "不能把別人的 HTML 當成功印出去")
        body = json.loads(err)
        self.assertEqual(body["error"], "bad_response")
        self.assertLess(len(err), 2000)

    def test_a_non_json_session_answer_is_never_used_as_the_token(self):
        for text in ("<html>nope</html>", "tok\nX-Evil: 1", "has space", ""):
            FakeDaemon.routes["GET /api/session"] = (200, Raw(text))
            FakeDaemon.routes["GET /api/supervisor/health"] = (200, {"ok": True})
            code, out, err = self.run_cli("health")
            self.assertNotEqual(code, 0, repr(text))
            self.assertEqual(json.loads(err)["error"], "no_token", repr(text))
            self.assertEqual(out, "")
            self.assertNotIn("X-Evil", err)
            self.assertFalse([r for r in FakeDaemon.seen if r["path"] == "/api/supervisor/health"], "沒有 token 就不能送出請求")
            FakeDaemon.seen.clear()

    def test_the_token_never_appears_in_error_output(self):
        FakeDaemon.routes["GET /api/supervisor/health"] = (500, {"error": "boom"})
        code, out, err = self.run_cli("health")
        self.assertNotEqual(code, 0)
        self.assertNotIn(TOKEN, err + out)

    def test_a_daemon_error_cannot_reflect_the_session_token_to_cli_output(self):
        from unittest.mock import patch

        service_token_file = Path(self.dir.name) / "service.token"
        service_token_file.write_text("service-secret-token-0123456789abcdef\n", encoding="utf-8")
        service_token_file.chmod(0o600)
        cases = (
            (TOKEN, {}),
            ("bot-secret-token-0123456789abcdef", {"AM_BOT_ID": "bot-agm", "AM_BOT_TOKEN": "bot-secret-token-0123456789abcdef"}),
            ("service-secret-token-0123456789abcdef", {
                "AM_SERVICE_ID": "daemon-swap", "AM_SERVICE_TOKEN_FILE": str(service_token_file),
            }),
        )
        for secret, env in cases:
            with self.subTest(auth=env or "ui"), patch.dict(os.environ, env):
                FakeDaemon.routes["GET /api/supervisor/health"] = (500, {
                    "error": "request_failed", "message": f"bad credential {secret}",
                    "details": {"echo": secret, "nested": [f"Bearer {secret}"]},
                })
                code, out, err = self.run_cli("health")
                self.assertNotEqual(code, 0)
                self.assertEqual(out, "")
                self.assertNotIn(secret, err, "daemon-controlled error detail must not echo the credential used for this request")

                FakeDaemon.routes["GET /api/supervisor/health"] = (200, {"debug": {"auth": secret}})
                code, out, err = self.run_cli("health")
                self.assertEqual(code, 0, err)
                self.assertNotIn(secret, out, "successful daemon responses must not reflect the credential either")
                self.assertIn("[redacted]", out)

    def test_a_huge_error_body_is_capped(self):
        FakeDaemon.routes["GET /api/supervisor/health"] = (502, Raw("x" * 200_000))
        code, _out, err = self.run_cli("health")
        self.assertNotEqual(code, 0)
        body = json.loads(err)
        self.assertEqual((body["error"], body["status"]), ("http_error", 502))
        self.assertLess(len(err), 5000, "daemon 前面擋了一頁 HTML 也不能整頁灌進輸出")

    def test_a_daemon_that_is_not_running_is_a_json_error_with_exit_6(self):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            dead = s.getsockname()[1]
        self.write_runtime({"daemon_url": f"http://127.0.0.1:{dead}", "manager_bot_id": "bot-agm"})
        code, out, err = self.run_cli("health")
        self.assertEqual((code, out), (6, ""))
        self.assertEqual(json.loads(err)["error"], "connect_failed")

    def test_slow_server_side_operations_get_a_longer_default_timeout(self):
        from unittest import mock
        seen = []

        def fake_raw(self_, method, path, body, auth=True):
            seen.append((path, self_.timeout))
            return {}

        with mock.patch.object(agm.Client, "_raw", fake_raw):
            self.ok("mission", "deliver", "m1", "--worktree", "/tmp/w")
            self.ok("release-triage", "publish")
            self.ok("--timeout", "7", "release-triage", "publish")
            self.ok("health")
        self.assertEqual([t for _p, t in seen], [agm.SLOW_TIMEOUT, agm.SLOW_TIMEOUT, 7.0, agm.DEFAULT_TIMEOUT], seen)


class PythonThreeNineTest(unittest.TestCase):
    """m4p 只有 Python 3.9：agm.py 不能用 3.10+ 的語法或 API（`match`、執行期的 `X | Y`、`pairwise`…）。"""

    SRC = Path(agm.__file__).read_text(encoding="utf-8")

    def test_it_parses_as_python_3_9(self):
        import ast
        ast.parse(self.SRC, filename="agm.py", feature_version=(3, 9))

    def test_no_runtime_union_syntax_or_3_10_apis(self):
        import ast
        tree = ast.parse(self.SRC)
        # 註記裡的 `str | None` 靠 `from __future__ import annotations` 不會被求值，所以只看註記**以外**的地方。
        annotation_nodes: set = set()
        for node in ast.walk(tree):
            anns = []
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                anns += [a.annotation for a in node.args.args + node.args.kwonlyargs + node.args.posonlyargs]
                anns += [node.args.vararg.annotation if node.args.vararg else None, node.args.kwarg.annotation if node.args.kwarg else None, node.returns]
            elif isinstance(node, ast.AnnAssign):
                anns.append(node.annotation)
            for a in anns:
                if a is not None:
                    annotation_nodes.update(id(n) for n in ast.walk(a))
        self.assertIn("from __future__ import annotations", self.SRC)
        types = {"str", "int", "float", "bool", "dict", "list", "tuple", "set", "bytes", "object", "type"}
        problems = []
        for node in ast.walk(tree):
            if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr) and id(node) not in annotation_nodes:
                for side in (node.left, node.right):
                    if (isinstance(side, ast.Constant) and side.value is None) or (isinstance(side, ast.Name) and side.id in types):
                        problems.append(f"第 {node.lineno} 行：執行期的 X | Y（3.10+）")
            if isinstance(node, ast.Attribute) and node.attr in {"pairwise", "bit_count", "UTC"}:
                problems.append(f"第 {node.lineno} 行：.{node.attr}（3.10+／3.11+）")
            if isinstance(node, ast.Call) and any(k.arg == "strict" for k in node.keywords) and getattr(node.func, "id", "") == "zip":
                problems.append(f"第 {node.lineno} 行：zip(strict=)（3.10+）")
            if isinstance(node, ast.Call) and any(k.arg in ("slots", "kw_only") for k in node.keywords) and "dataclass" in ast.dump(node.func):
                problems.append(f"第 {node.lineno} 行：dataclass(slots/kw_only)（3.10+）")
            if isinstance(node, ast.ImportFrom) and node.module == "tomllib" or (isinstance(node, ast.Import) and any(a.name == "tomllib" for a in node.names)):
                problems.append(f"第 {node.lineno} 行：tomllib（3.11+）")
        self.assertEqual(problems, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
