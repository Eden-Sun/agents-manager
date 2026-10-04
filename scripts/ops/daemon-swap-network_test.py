#!/usr/bin/env python3
"""回歸測試 daemon-swap 的本機 API token 不經環境 proxy 或跨主機轉址。"""

import http.server
import os
import re
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from urllib.parse import urlsplit


SCRIPT = Path(__file__).with_name("daemon-swap.sh")
TOKEN = "daemon-swap-proxy-test-token"


class QuietHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass


class ApiHandler(QuietHandler):
    def do_GET(self):
        if self.path == "/api/session":
            body = ('{"token":"' + TOKEN + '"}').encode()
        elif self.path == "/api/capabilities":
            body = b'{"capabilities":["service_principals","swap_restart_window"]}'
        else:
            body = b"{}"
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class ProxyHandler(QuietHandler):
    tokens = []

    def do_GET(self):
        self.tokens.append(self.headers.get("X-AM-Token"))
        path = urlsplit(self.path).path
        if path.endswith("/api/session"):
            body = ('{"token":"' + TOKEN + '"}').encode()
        else:
            body = b'{"capabilities":["service_principals","swap_restart_window"]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class RedirectHandler(QuietHandler):
    target = ""

    def do_GET(self):
        if self.path == "/api/session":
            body = ('{"token":"' + TOKEN + '"}').encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(302)
        self.send_header("Location", self.target)
        self.send_header("Content-Length", "0")
        self.end_headers()


class ReceiverHandler(QuietHandler):
    tokens = []

    def do_GET(self):
        self.tokens.append(self.headers.get("X-AM-Token"))
        body = b'{"capabilities":["service_principals","swap_restart_window"]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class DaemonSwapNetworkTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        text = SCRIPT.read_text(encoding="utf-8")
        cls.snippets = {}
        for name in ("service_capability", "agm_probe", "restart_window"):
            match = re.search(rf"{name}\(\).*?<<'PY'\n(.*?)\nPY", text, re.S)
            if not match:
                raise AssertionError(f"cannot locate {name} Python request")
            cls.snippets[name] = match.group(1)

    def start(self, handler):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.shutdown)
        self.addCleanup(server.server_close)
        return server, f"http://127.0.0.1:{server.server_port}"

    def run_snippet(self, name, port, env):
        with tempfile.TemporaryDirectory(prefix="daemon-swap-home-") as home:
            child_env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": home, **env}
            return subprocess.run(
                [sys.executable, "-", str(port)], input=self.snippets[name], text=True,
                capture_output=True, env=child_env, timeout=10,
            )

    def test_every_authenticated_api_snippet_disables_proxy_and_redirects(self):
        for name, snippet in self.snippets.items():
            with self.subTest(name=name):
                self.assertIn("ProxyHandler({})", snippet)
                self.assertIn("NoRedirect()", snippet)
                self.assertNotIn("urllib.request.urlopen(", snippet)
                self.assertIn("opener.open(", snippet)

    def test_local_api_token_bypasses_environment_proxy(self):
        _daemon, daemon_url = self.start(ApiHandler)
        proxy, proxy_url = self.start(ProxyHandler)
        ProxyHandler.tokens = []
        result = self.run_snippet(
            "service_capability",
            urlsplit(daemon_url).port,
            {"HTTP_PROXY": proxy_url, "http_proxy": proxy_url, "ALL_PROXY": proxy_url,
             "all_proxy": proxy_url, "NO_PROXY": "", "no_proxy": ""},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "service")
        self.assertNotIn(TOKEN, ProxyHandler.tokens, f"proxy saw the UI token: {ProxyHandler.tokens}")
        self.assertEqual(ProxyHandler.tokens, [], f"local API traffic reached the proxy at {proxy.server_port}")

    def test_local_api_token_does_not_follow_cross_port_redirect(self):
        receiver, receiver_url = self.start(ReceiverHandler)
        ReceiverHandler.tokens = []
        RedirectHandler.target = receiver_url + "/collected"
        daemon, _daemon_url = self.start(RedirectHandler)
        result = self.run_snippet(
            "service_capability",
            daemon.server_port,
            {"HTTP_PROXY": "", "http_proxy": "", "ALL_PROXY": "", "all_proxy": "",
             "NO_PROXY": "", "no_proxy": ""},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(TOKEN, ReceiverHandler.tokens, f"redirect target saw the UI token: {ReceiverHandler.tokens}")
        self.assertEqual(ReceiverHandler.tokens, [], f"request followed redirect to {receiver.server_port}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
