"""Security regressions for cutover-helper's local API token handling."""

import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest


TOOL = Path(__file__).with_name("cutover-helper.py")
TOKEN = "cutover-helper-test-token-do-not-print"


class QuietHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send_json(self, code, value):
        raw = json.dumps(value).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


class ApiHandler(QuietHandler):
    def do_GET(self):
        self.server.seen.append((self.path, self.headers.get("X-AM-Token")))
        if self.path == "/api/session":
            return self.send_json(200, {"token": self.server.secret})
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", self.server.redirect_to)
            self.end_headers()
            return
        self.send_json(200, {})


class ProxyHandler(QuietHandler):
    def do_GET(self):
        self.server.seen.append((self.path, self.headers.get("X-AM-Token")))
        self.send_json(200, {})


class CutoverHelperTokenTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cutover-helper-test-")
        self.addCleanup(self.temp.cleanup)
        self.token_file = Path(self.temp.name) / "ui-token"
        self.token_file.write_text(TOKEN)
        self.servers = []
        self.addCleanup(self.stop_servers)

    def start_server(self, handler):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        server.seen = []
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.servers.append(server)
        return server

    def stop_servers(self):
        for server in reversed(self.servers):
            server.shutdown()
            server.server_close()

    def run_api(self, base, path, proxy=None):
        env = {
            "HOME": self.temp.name,
            "PATH": os.environ.get("PATH", ""),
            "PYTHONDONTWRITEBYTECODE": "1",
        }
        if proxy:
            env.update(
                HTTP_PROXY=proxy,
                http_proxy=proxy,
                ALL_PROXY=proxy,
                all_proxy=proxy,
                NO_PROXY="",
                no_proxy="",
            )
        return subprocess.run(
            [
                sys.executable, "-B", str(TOOL), "--base", base,
                "--token-file", str(self.token_file), "api", "GET", path,
            ],
            capture_output=True,
            text=True,
            env=env,
            timeout=5,
        )

    def test_session_response_never_prints_the_token(self):
        api = self.start_server(ApiHandler)
        api.secret = TOKEN
        result = self.run_api(f"http://127.0.0.1:{api.server_port}", "/api/session")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(TOKEN, result.stdout + result.stderr)

    def test_http_proxy_cannot_receive_the_ui_token(self):
        proxy = self.start_server(ProxyHandler)
        result = self.run_api("http://credential-sink.invalid:7788", "/api/state",
                              f"http://127.0.0.1:{proxy.server_port}")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(proxy.seen, [], f"proxy received credential: {proxy.seen}")

    def test_redirect_does_not_forward_the_ui_token(self):
        api = self.start_server(ApiHandler)
        receiver = self.start_server(ApiHandler)
        api.redirect_to = f"http://127.0.0.1:{receiver.server_port}/api/state"
        result = self.run_api(f"http://127.0.0.1:{api.server_port}", "/redirect")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(receiver.seen, [], f"redirect target received credential: {receiver.seen}")


if __name__ == "__main__":
    unittest.main()
