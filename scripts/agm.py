#!/usr/bin/env python3
"""agm — AGM 總管的結構化 JSON CLI（只用 stdlib）。

由 daemon 部署到總管專用 cwd 的 `bin/agm`，封裝既有的 AG Man HTTP API，
輸出精簡 JSON 讓總管模型直接讀。

設計上的三條硬規則：

1. **只連 loopback。** `runtime.json` 的 `daemon_url` 必須指向 127.0.0.1 / ::1 /
   localhost。X-AM-Token 是全域管理權限，指到別的主機就等於把 token 送出去。
2. **token 不落地、不外露。** 執行期 `GET /api/session` 拿，永遠不進 argv、
   不進輸出、不進錯誤訊息。
3. **mutation 逾時不自動重試。** 逾時代表「送達未知」，重試一次會變成派兩份工。
   逾時就回 `delivery_unknown` 並把 client_request_id 還給呼叫者去對帳。

執行期設定：`AGM_RUNTIME_DIR`（測試用 override）> `--runtime-dir` > 這支腳本的
上層目錄（bin/agm → cwd）> `~/.config/agents-manager/supervisor/AGM`。
"""

from __future__ import annotations

import argparse
import datetime
import html
import http.client
import json
import os
import pathlib
import plistlib
import re
import socket
import stat
import subprocess
import sys
import tempfile
import time
import uuid
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

DEFAULT_TIMEOUT = 30.0
SLOW_TIMEOUT = 300.0
AUTO_TIMEOUT = 3600.0
MAX_TIMEOUT = 3600.0
MAX_LIMIT = 1000
MAX_ID_CHARS = 200
MAX_DETAIL_CHARS = 2000
MAX_TEXT_CHARS = 200_000
LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost", "0:0:0:0:0:0:0:1"}
DEFAULT_RUNTIME_DIR = "~/.config/agents-manager/supervisor/AGM"
CLAIM_LABEL = "wip"
CLAIM_MARK = "agm:issue-claim"
RELEASE_MARK = "agm:issue-release"
PROGRESS_MARK = "agm:issue-progress"
LABEL_ONLY_RELEASE = "label-only"
CLAIM_STALE_SECS = 24 * 3600
TRUSTED_ASSOCIATIONS = {"OWNER", "MEMBER", "COLLABORATOR"}
GH_TIMEOUT = 60.0

def path_segment(value: object) -> str:
    """URL 路徑的**一段**：`/`、`?`、`#`、`..` 都編碼，id 不能改變打到哪一個端點。"""
    return urllib.parse.quote(str(value), safe="")

class AgmError(Exception):
    """帶結構的失敗；`main` 會把它印成 JSON 並用非 0 離開。"""

    def __init__(self, kind: str, message: str, exit_code: int = 1, **extra: object) -> None:
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.exit_code = exit_code
        self.extra = extra

    def to_json(self) -> dict:
        out = {"error": self.kind, "message": self.message}
        out.update({k: v for k, v in self.extra.items() if v is not None})
        return out

def runtime_dir(explicit: str | None = None) -> Path:
    """設定目錄。env 優先給測試 override 用，不必動部署好的檔案。"""
    env = os.environ.get("AGM_RUNTIME_DIR")
    if env:
        return Path(env).expanduser()
    if explicit:
        return Path(explicit).expanduser()
    here = Path(__file__).resolve().parent
    if here.name == "bin" and (here.parent / "runtime.json").is_file():
        return here.parent
    return Path(DEFAULT_RUNTIME_DIR).expanduser()

def load_runtime(explicit: str | None = None) -> dict:
    d = runtime_dir(explicit)
    path = d / "runtime.json"
    try:
        raw = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise AgmError("no_runtime", f"找不到 runtime.json：{path}", 2, runtime_dir=str(d))
    except OSError as e:
        raise AgmError("no_runtime", f"讀不到 {path}：{e.strerror}", 2)
    try:
        cfg = json.loads(raw)
    except json.JSONDecodeError as e:
        raise AgmError("bad_runtime", f"runtime.json 不是合法 JSON：{e}", 2)
    if not isinstance(cfg, dict):
        raise AgmError("bad_runtime", "runtime.json 必須是一個 object", 2)
    cfg["_dir"] = str(d)
    return cfg

def daemon_url(cfg: dict) -> str:
    url = cfg.get("daemon_url") or "http://127.0.0.1:7788"
    if not isinstance(url, str):
        raise AgmError("bad_runtime", "runtime.json 的 daemon_url 必須是字串", 2)
    return check_loopback(url)

def check_loopback(url: str) -> str:
    """擋掉非本機的 daemon_url。token 只能留在這台機器上。"""
    parts = urllib.parse.urlsplit(url)
    if parts.scheme not in ("http", "https"):
        raise AgmError("not_loopback", f"daemon_url 的協定不支援：{parts.scheme or '(空)'}", 2)
    if parts.username or parts.password:
        raise AgmError("not_loopback", "daemon_url 不可帶帳號密碼", 2)
    host = (parts.hostname or "").lower()
    if host not in LOOPBACK_HOSTS:
        raise AgmError(
            "not_loopback",
            f"daemon_url 必須指向本機 loopback，收到 {host or '(空)'}；"
            "X-AM-Token 是全域管理權限，不能送到別台機器",
            2,
        )
    return f"{parts.scheme}://{parts.netloc}"

def self_bot_id(cfg: dict) -> str:
    """這支 CLI 代表誰說話。雙角色部署寫 `self_bot_id`；舊的只有 manager_bot_id（那時只有一顆 AGM）。"""
    v = cfg.get("self_bot_id")
    if isinstance(v, str) and v:
        return v
    return manager_bot_id(cfg)

def runtime_role(cfg: dict) -> str:
    """`patrol`（巡檢，使用者入口）或 `responder`（協調者）。舊的 runtime.json 沒寫就是巡檢。"""
    v = cfg.get("role")
    return v if v in ("patrol", "responder") else "patrol"

def bot_auth_headers(cfg: dict) -> dict:
    """角色身份的證明：pane 環境裡的 `AM_BOT_ID` + `AM_BOT_TOKEN`（既有 per-bot token）。
    已運行的舊 pane 暫用相同值的 `AM_HOOK_TOKEN`；新 daemon 只認 id 與自己那顆 token 對得上。

    pane 宣告了 bot id 就帶上該 id；沒有 token 或 token 對不上時，daemon 拒絕該 Bot 請求，
    Client 不會改用共用 User token。角色權限仍由 daemon 依驗過的 bot id 判斷。"""
    child_id = os.environ.get("AM_CHILD_SPAWN_ID", "")
    child_token = os.environ.get("AM_CHILD_TOKEN", "")
    if child_id or child_token:
        return {"X-AM-Child-Spawn-Id": child_id, "X-AM-Child-Token": child_token}
    bot = os.environ.get("AM_BOT_ID", "")
    tok = os.environ.get("AM_BOT_TOKEN") or os.environ.get("AM_HOOK_TOKEN", "")
    if bot:
        headers = {"X-AM-Bot-Id": bot}
        if tok:
            headers["X-AM-Bot-Token"] = tok
        return headers
    return {}

def manager_bot_id(cfg: dict) -> str:
    for key in ("manager_bot_id", "bot_id"):
        v = cfg.get(key)
        if isinstance(v, str) and v:
            return v
    raise AgmError("no_manager_bot", "runtime.json 沒有 manager_bot_id", 2)

class NoRedirect(urllib.request.HTTPRedirectHandler):
    """一律不跟 redirect。跟了就可能把 X-AM-Token 送到別的 host 去。"""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D102
        raise AgmError("redirect_refused", f"daemon 回了 {code} 轉址到別處；不跟隨以免帶著 token 出去", 4)

class Client:
    def __init__(self, base: str, timeout: float = DEFAULT_TIMEOUT, extra_headers: dict | None = None, *, bot_remote: bool = False) -> None:
        if bot_remote:
            parts = urllib.parse.urlsplit(base)
            if parts.scheme not in ("http", "https") or not parts.hostname or parts.username or parts.password or parts.path not in ("", "/") or parts.query or parts.fragment:
                raise AgmError("bad_daemon_url", "AM_DAEMON_URL 不合法", 2)
            self.base = f"{parts.scheme}://{parts.netloc}"
        else:
            self.base = check_loopback(base).rstrip("/")
        self.timeout = timeout
        self._token: str | None = None
        self._extra = dict(extra_headers or {})
        self._opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def token(self) -> str:
        """執行期取 token。不快取到磁碟、不印出來。

        取 token 發生在真正的請求**送出之前**：這裡逾時或斷線，呼叫端的 POST／PATCH 一個位元組都沒出去。
        所以不能原樣丟 `timeout`／`connection_lost`——`assign`／`review` 會把這兩種當成「送達未知」（exit 7）
        叫人去對帳，其實根本沒送。改成 `connect_failed`（exit 6，請求尚未送出）。
        """
        if self._token is None:
            try:
                body = self._raw("GET", "/api/session", None, auth=False, allow_text=True)
            except AgmError as e:
                if e.kind in ("timeout", "connection_lost"):
                    raise AgmError("connect_failed", f"取 token 失敗，請求尚未送出：{e.message}", 6) from e
                raise
            tok = ""
            if isinstance(body, dict) and isinstance(body.get("token"), str):
                tok = body["token"]
            elif isinstance(body, str):
                tok = body.strip()
            if not tok or len(tok) > 512 or not re.fullmatch(r"[A-Za-z0-9._~+/=-]+", tok):
                raise AgmError("no_token", "GET /api/session 沒有回一個合法的 token（空的、含空白／控制字元，或不是 token 的樣子）", 3)
            self._token = tok
        return self._token

    def _raw(self, method: str, path: str, body: object, auth: bool = True, allow_text: bool = False) -> object:
        url = self.base + path
        data = None
        headers = {"Accept": "application/json"}
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if auth:
            headers["X-AM-Caller"] = "agm"
            service_id = os.environ.get("AM_SERVICE_ID", "")
            service_file = os.environ.get("AM_SERVICE_TOKEN_FILE", "")
            if service_id or service_file:
                if not service_id or not service_file:
                    raise AgmError("bad_service_auth", "AM_SERVICE_ID 與 AM_SERVICE_TOKEN_FILE 必須一起設定", 2)
                if any(k in self._extra for k in ("X-AM-Bot-Id", "X-AM-Bot-Token", "X-AM-Child-Spawn-Id", "X-AM-Child-Token")):
                    raise AgmError("bad_service_auth", "service 與 bot 身份不能同時使用", 2)
                headers["X-AM-Service-Id"] = service_id
                headers["X-AM-Service-Token"] = self._read_private_token(service_file)
            elif any(k in self._extra for k in ("X-AM-Bot-Id", "X-AM-Bot-Token", "X-AM-Child-Spawn-Id", "X-AM-Child-Token")):
                headers.update(self._extra)
            else:
                headers["X-AM-Token"] = self.token()
                headers.update(self._extra)
        credential_values = tuple(
            value for name, value in headers.items()
            if name.lower() in {"x-am-token", "x-am-bot-token", "x-am-child-token", "x-am-service-token"} and value
        )
        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with self._opener.open(req, timeout=self.timeout) as res:
                parsed = _redact_credentials(_parse(res.read()), credential_values)
            if isinstance(parsed, str) and parsed.strip() and not allow_text:
                raise AgmError("bad_response", f"{method} {path} 回的不是 JSON（前 80 字：{parsed.strip()[:80]!r}）", 4, status=200)
            return parsed
        except urllib.error.HTTPError as e:
            with e:
                payload = _redact_credentials(_parse(e.read()), credential_values)
            detail = payload if isinstance(payload, (dict, list)) else str(payload or "")
            if isinstance(detail, str) and len(detail) > MAX_DETAIL_CHARS:
                detail = detail[:MAX_DETAIL_CHARS] + f"…（已截斷，共 {len(detail)} 字）"
            elif not isinstance(detail, str) and len(json.dumps(detail, ensure_ascii=False, default=str)) > MAX_DETAIL_CHARS * 4:
                detail = {"truncated": json.dumps(detail, ensure_ascii=False, default=str)[:MAX_DETAIL_CHARS] + "…"}
            raise AgmError(
                "http_error",
                f"{method} {path} 回 {e.code}",
                4,
                status=e.code,
                detail=detail,
            )
        except socket.timeout:
            raise AgmError("timeout", f"{method} {path} 逾時（{self.timeout:g}s）", 5)
        except (ConnectionError, http.client.HTTPException) as e:
            raise AgmError("connection_lost", f"{method} {path} 送出後連線中斷：{e}", 5)
        except urllib.error.URLError as e:
            if isinstance(e.reason, socket.timeout):
                raise AgmError("timeout", f"{method} {path} 逾時（{self.timeout:g}s）", 5)
            if isinstance(e.reason, (ConnectionError, http.client.HTTPException)) and not isinstance(e.reason, ConnectionRefusedError):
                raise AgmError("connection_lost", f"{method} {path} 送出後連線中斷：{e.reason}", 5)
            raise AgmError("connect_failed", f"連不上 daemon：{e.reason}", 6)

    @staticmethod
    def _read_private_token(path: str) -> str:
        """Read one owner-only service token without putting it in argv or printing it."""
        parent = os.path.dirname(os.path.abspath(path))
        dir_flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        try:
            dir_fd = os.open(parent, dir_flags)
        except OSError as e:
            raise AgmError("bad_service_auth", f"讀不到 service token 目錄 {parent}：{e}", 2) from e
        try:
            dir_st = os.fstat(dir_fd)
            if not stat.S_ISDIR(dir_st.st_mode):
                raise AgmError("bad_service_auth", f"service token 路徑 {parent} 不是目錄", 2)
            if hasattr(os, "getuid") and dir_st.st_uid != os.getuid():
                raise AgmError("bad_service_auth", f"service token 目錄 {parent} 不是目前使用者擁有", 2)
            if dir_st.st_mode & 0o077:
                raise AgmError("bad_service_auth", f"service token 目錄 {parent} 權限過寬；請 chmod 700", 2)
        finally:
            os.close(dir_fd)
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        try:
            fd = os.open(path, flags)
        except OSError as e:
            raise AgmError("bad_service_auth", f"讀不到 service token 檔 {path}：{e}", 2) from e
        try:
            st = os.fstat(fd)
            if not stat.S_ISREG(st.st_mode):
                raise AgmError("bad_service_auth", f"service token 檔 {path} 不是普通檔案", 2)
            if hasattr(os, "getuid") and st.st_uid != os.getuid():
                raise AgmError("bad_service_auth", f"service token 檔 {path} 不是目前使用者擁有", 2)
            if st.st_mode & 0o077:
                raise AgmError("bad_service_auth", f"service token 檔 {path} 權限過寬；請 chmod 600", 2)
            try:
                data = os.read(fd, 4097)
                if len(data) > 4096:
                    raise AgmError("bad_service_auth", f"service token 檔 {path} 太大", 2)
                raw = data.decode("utf-8")
            except (OSError, UnicodeDecodeError) as e:
                raise AgmError("bad_service_auth", f"讀不到 service token 檔 {path}：{e}", 2) from e
        finally:
            os.close(fd)
        tok = single_line_token(raw, f"service token 檔 {path}")
        if not tok:
            raise AgmError("bad_service_auth", f"service token 檔 {path} 是空的", 2)
        return tok

    def get(self, path: str, query: dict | None = None) -> object:
        if query:
            clean = {k: str(v) for k, v in query.items() if v is not None and v != ""}
            if clean:
                path = f"{path}?{urllib.parse.urlencode(clean)}"
        return self._raw("GET", path, None)

    def post(self, path: str, body: object = None) -> object:
        return self._raw("POST", path, body if body is not None else {})

    def patch(self, path: str, body: object) -> object:
        return self._raw("PATCH", path, body)

    def delete(self, path: str) -> object:
        return self._raw("DELETE", path, None)

    def put(self, path: str, body: object = None) -> object:
        return self._raw("PUT", path, body if body is not None else {})

def _parse(raw: bytes) -> object:
    if not raw:
        return None
    text = raw.decode("utf-8", "replace")
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return text

def _redact_credentials(value: object, credentials: tuple[str, ...]) -> object:
    """Never print a credential just because a local API error or response reflected its auth header."""
    if isinstance(value, str):
        for credential in credentials:
            value = value.replace(credential, "[redacted]")
        return value
    if isinstance(value, list):
        return [_redact_credentials(item, credentials) for item in value]
    if isinstance(value, dict):
        return {
            _redact_credentials(key, credentials): _redact_credentials(item, credentials)
            for key, item in value.items()
        }
    return value

def optional_get(client: Client, path: str, query: dict | None = None) -> object | None:
    """端點還沒部署（404）時回 None，讓呼叫端退回舊介面而不是整支掛掉。"""
    try:
        return client.get(path, query)
    except AgmError as e:
        if e.kind == "http_error" and e.extra.get("status") == 404:
            return None
        raise

def _s(v: object) -> str:
    return v if isinstance(v, str) else ""

def _lamp(b: dict, run: dict) -> str:
    """側欄上那顆複合燈號。

    兩支 state 端點現在都會給 `lamp`，有就照抄——daemon 算它用的是 `bot_connected`
    （bot → host → **herdr session**），CLI 這邊看得到的只有主機層的 `host_connected`，
    同一台主機上 session 掉了的那顆推不出來。

    推算只留給**還沒更新的 daemon**（`/api/supervisor/state` 以前沒有這個欄位，照抄的結果是
    每一顆 bot 都拿到空字串，issue #514）。規則照 `daemon/src/api.rs` 的 `fn lamp` 抄一份，
    差別只在連線那一格會粗一點——所以能照抄就別推。
    """
    lamp = _s(b.get("lamp"))
    if lamp:
        return lamp
    if b.get("host_connected") is False:
        return "disconnected"
    if not run:
        return "offline"
    state = _s(run.get("state") or run.get("run_state"))
    if state in ("starting", "stopping"):
        return state
    if state in ("stopped", "exited"):
        return "offline"
    status = _s(run.get("agent_status") or run.get("status"))
    return status if status in ("idle", "working", "blocked") else "unknown"

def _queued_count(b: dict, queued_turn: object) -> int | None:
    """排隊中的工作有幾筆。`None` = 不知道確切筆數，**不是 0**。

    只有 `/api/supervisor/state` 的 `queued_turns` 是真的筆數。`/api/state` 給的是「下一筆」
    那一則 turn：有它只代表 ≥1，回 1 是在講一個沒有根據的數字——要判斷「有沒有在排隊」看
    `queued_turn` 就好。明講 `null`（沒有下一筆）才是紮實的 0。
    """
    n = b.get("queued_turns")
    if isinstance(n, int) and not isinstance(n, bool):
        return n
    if queued_turn is not None:
        return None
    if "queued_turn" in b or "queued" in b:
        return 0
    return None

def _bot_row(b: dict, project_id: str, manager_id: str) -> dict:
    """一顆 bot 的精簡投影。`run` 裡帶的是**執行期**真值（可能與設定不同）。

    兩種來源的欄位名不一樣，這裡兩種都吃（issue #514）：`/api/state` 給 `lamp` 與 `queued_turn`
    （一筆 turn），`/api/supervisor/state` 給 `queued_turns`（**筆數**）、`asleep`、`host_connected`。
    """
    run = b.get("run") if isinstance(b.get("run"), dict) else b.get("active_run")
    run = run if isinstance(run, dict) else {}
    bid = _s(b.get("id") or b.get("bot_id"))
    row = {
        "id": bid,
        "name": _s(b.get("name") or b.get("label")),
        "project_id": _s(b.get("project_id")) or project_id,
        "kind": _s(b.get("kind")),
        "identity": _s(b.get("identity")),
        "model": _s(b.get("model")),
        "effort": _s(b.get("effort")),
        "managed_by": _s(b.get("managed_by")),
        "parent_bot_id": _s(b.get("parent_bot_id")),
        "cwd": _s(b.get("cwd")),
        "lamp": _lamp(b, run),
        "is_manager": bool(manager_id) and bid == manager_id,
    }
    qt = b.get("queued_turn")
    if isinstance(qt, dict):
        row["queued_turn"] = _turn_row(qt)
    elif b.get("queued") is not None:
        row["queued_turn"] = b.get("queued")
    else:
        row["queued_turn"] = None
    row["queued_turns"] = _queued_count(b, row["queued_turn"])
    if run:
        row["run"] = {
            "id": _s(run.get("id") or run.get("run_id")),
            "state": _s(run.get("state") or run.get("run_state")),
            "agent_status": _s(run.get("agent_status") or run.get("status")),
            "runtime_model": _s(run.get("runtime_model")),
            "runtime_effort": _s(run.get("runtime_effort")),
            "native_session_id": _s(run.get("native_session_id")),
            "adopted": run.get("adopted"),
            "terminal_id": run.get("terminal_id"),
            "identity_recovery_reason": run.get("identity_recovery_reason"),
            "pane_id": _s(run.get("pane_id")),
            "turn_error": _s(run.get("turn_error")),
            "started_at": _s(run.get("started_at")),
        }
    else:
        row["run"] = None
    asleep = b.get("asleep")
    row["asleep"] = (
        {"since": _s(asleep.get("since")), "idle_minutes": asleep.get("idle_minutes")}
        if isinstance(asleep, dict)
        else None
    )
    hc = b.get("host_connected")
    row["host_connected"] = hc if isinstance(hc, bool) else None
    turn = b.get("in_flight_turn") if isinstance(b.get("in_flight_turn"), dict) else b.get("turn")
    if isinstance(turn, dict):
        row["in_flight_turn"] = _turn_row(turn)
    return row

def _turn_row(t: dict) -> dict:
    """一筆 turn。真欄位是 `status` / `completed_at`（不是 state / ended_at）。"""
    return {
        "id": _s(t.get("id")),
        "status": _s(t.get("status") or t.get("state")),
        "delivery": _s(t.get("delivery")),
        "origin": _s(t.get("origin")),
        "client_request_id": _s(t.get("client_request_id")),
        "run_id": _s(t.get("run_id")),
        "created_at": _s(t.get("created_at")),
        "completed_at": _s(t.get("completed_at") or t.get("ended_at")),
    }

def slim_state(state: object, manager_id: str = "") -> dict:
    """把 `/api/state` 砍成總管需要的欄位。

    `/api/state` 把 bots 掛在 `projects[].bots` 底下，supervisor 的 sanitized state
    可能給頂層 `bots`——兩種都吃。env、args、persona、token 一律不帶出來。
    """
    if not isinstance(state, dict):
        return {"projects": [], "bots": []}
    projects, bots = [], []
    seen: set[str] = set()

    def add_bot(b: object, project_id: str = "") -> None:
        if not isinstance(b, dict):
            return
        row = _bot_row(b, project_id, manager_id)
        if not row["id"] or row["id"] in seen:
            return
        seen.add(row["id"])
        bots.append(row)

    for p in state.get("projects") or []:
        if not isinstance(p, dict):
            continue
        pid = _s(p.get("id"))
        projects.append(
            {
                "id": pid,
                "label": _s(p.get("label") or p.get("name")),
                "path": _s(p.get("path")),
                "host": _s(p.get("host")),
            }
        )
        for b in p.get("bots") or []:
            add_bot(b, pid)
    for b in state.get("bots") or []:
        add_bot(b)

    out: dict = {"projects": projects, "bots": bots}
    if manager_id:
        out["manager_bot_id"] = manager_id
    return out

def slim_message(m: object) -> dict:
    if not isinstance(m, dict):
        return {}
    slimmed = {
        "id": _s(m.get("id")),
        "bot_id": _s(m.get("bot_id")),
        "bot_name": _s(m.get("bot_name")),
        "project_id": _s(m.get("project_id")),
        "project_label": _s(m.get("project_label")),
        "bot_deleted": bool(m.get("bot_deleted")),
        "turn_id": _s(m.get("turn_id")),
        "role": _s(m.get("role")),
        "source": _s(m.get("source")),
        "incomplete": bool(m.get("incomplete")),
        "truncated": bool(m.get("truncated")),
        "rewound_at": _s(m.get("rewound_at")) or None,
        "relay_from": _s(m.get("relay_from")) if m.get("relay_from") is not None else None,
        "relay_unverified": bool(m.get("relay_unverified")) if "relay_unverified" in m else None,
        "content": _s(m.get("content") or m.get("text")),
        "created_at": _s(m.get("created_at") or m.get("ts")),
    }
    if "seq" in m:
        slimmed["seq"] = m["seq"]
    return slimmed

def cmd_state(client: Client, cfg: dict, args) -> object:
    manager = ""
    try:
        manager = manager_bot_id(cfg)
    except AgmError:
        pass
    sup = optional_get(client, "/api/supervisor/state")
    return slim_state(sup if sup is not None else client.get("/api/state"), manager)

def cmd_supervisor(client: Client, cfg: dict, args) -> object:
    return client.get("/api/supervisor")

def cmd_health(client: Client, cfg: dict, args) -> object:
    return client.get("/api/supervisor/health")

def cmd_supervisor_action(client: Client, cfg: dict, args) -> object:
    return client.post(f"/api/supervisor/{args.action}", {})

def cmd_search(client: Client, cfg: dict, args) -> object:
    """證據搜尋。優先用新的 evidence 介面（有 message ID、分頁、專案過濾）。"""
    ev = optional_get(
        client,
        "/api/supervisor/evidence",
        {"q": args.query, "bot_id": args.bot, "project_id": args.project, "before": args.before, "limit": args.limit},
    )
    if ev is not None and isinstance(ev, dict):
        return {
            "source": "evidence",
            "query": args.query,
            "messages": [slim_message(m) for m in ev.get("messages") or []],
            "has_more": bool(ev.get("has_more")),
            "next_cursor": ev.get("next_cursor"),
        }
    hits = client.get("/api/search/messages", {"q": args.query})
    return {
        "source": "legacy_search",
        "query": args.query,
        "note": "舊搜尋介面只有片段，沒有 message/turn ID；要引用證據請再用 `agm messages <bot-id>` 讀原文",
        "hits": hits,
    }

def cmd_messages(client: Client, cfg: dict, args) -> object:
    page = client.get(
        f"/api/bots/{path_segment(args.bot_id)}/messages",
        {"limit": args.limit, "before": args.before},
    )
    if not isinstance(page, dict):
        return page
    raw_messages = page.get("messages") or []
    msgs = [slim_message(m) for m in raw_messages]
    has_more = bool(page.get("has_more"))
    valid_seqs = [
        (m["seq"], m)
        for m in raw_messages
        if isinstance(m, dict)
        and isinstance(m.get("seq"), int)
        and not isinstance(m.get("seq"), bool)
        and m["seq"] > 0
    ]
    if has_more and raw_messages:
        if len(valid_seqs) == len(raw_messages):
            oldest = min(valid_seqs, key=lambda item: item[0])[1]
            next_cursor = slim_message(oldest)["id"]
        else:
            next_cursor = msgs[0]["id"]
    else:
        next_cursor = None
    return {
        "bot_id": args.bot_id,
        "messages": msgs,
        "has_more": has_more,
        "next_cursor": next_cursor,
        "turns": [_turn_row(t) for t in page.get("turns") or [] if isinstance(t, dict)],
    }

def _check_length(text: str) -> None:
    if len(text) > MAX_TEXT_CHARS:
        raise AgmError(
            "bad_args",
            f"內容 {len(text)} 字，超過上限 {MAX_TEXT_CHARS}（daemon 也會拒收）。拆成多筆，或把長內容寫進檔案、交辦裡只寫路徑讓對方自己讀",
            2,
            chars=len(text),
            max_chars=MAX_TEXT_CHARS,
        )

def read_text_file(path: str, flag: str) -> str:
    """把 `--…-file` 的內容讀進來。讀不到／不是 UTF-8 都是**呼叫端的參數錯**（`bad_args`／exit 2）。

    `UnicodeDecodeError` 不是 `OSError`：只接 `OSError` 的話它會掉到 `main` 的兜底，變成
    `internal`／exit 1，而 exit 1 在這支 CLI 是「gh 失敗／有落差」那一類，照離開碼分流的
    排程腳本會分錯邊（issue #333 修過 `--text-file`，#516 是同一個形狀的另外三個旗標）。
    """
    try:
        return Path(path).expanduser().read_text(encoding="utf-8")
    except OSError as e:
        raise AgmError("bad_args", f"讀不到 {flag}（{path}）：{e.strerror or e}", 2, path=path)
    except UnicodeDecodeError:
        raise AgmError("bad_args", f"{flag}（{path}）不是 UTF-8 文字", 2, path=path)

def _text_of(args, flag: str, file_attr: str, text_attr: str, what: str) -> str:
    """`--<x>-file` 或 `--<x>`，二選一，回 strip 過、長度檢查過的內容。"""
    path = getattr(args, file_attr, None)
    text = read_text_file(path, flag + "-file") if path else (getattr(args, text_attr, None) or "")
    text = text.strip()
    if not text:
        raise AgmError("bad_args", f"{what}不可為空", 2)
    _check_length(text)
    return text

def _assign_text(args) -> str:
    return _text_of(args, "--text", "text_file", "text", "交辦內容")

def cmd_assign(client: Client, cfg: dict, args) -> object:
    body = {
        "target_bot_id": args.bot,
        "text": _assign_text(args),
        "client_request_id": args.request_id,
    }
    if args.source_turn_id:
        body["source_turn_id"] = args.source_turn_id
    if args.owns:
        body["ownership"] = list(args.owns)
    if getattr(args, "review_by", None):
        body["review_role"] = args.review_by
    if getattr(args, "notice", False):
        body["kind"] = "notice"
        body["expects_review"] = False
    if getattr(args, "ack", False):
        body["ack"] = True
    if getattr(args, "reply_to", None):
        body["reply_to"] = args.reply_to
    try:
        return client.post("/api/supervisor/assignments", body)
    except AgmError as e:
        if e.kind in ("timeout", "connection_lost"):
            raise AgmError(
                "delivery_unknown",
                "交辦請求逾時，送達狀態未知。請用同一個 client_request_id 先 `agm assignments` 對帳，"
                "確認沒有這筆才用**同一個** ID 重送；不要換新 ID。",
                7,
                client_request_id=args.request_id,
                target_bot_id=args.bot,
            )
        raise

OPEN_STATUSES = ("queued", "delivered", "unknown", "awaiting_review", "blocked", "quota_blocked")
ASSIGNMENT_PAGE = 200

def _all_assignments(client: Client, status: str = "") -> tuple[list, bool]:
    """翻完整份（或 `status` 那一份）交辦清單。回 `(items, complete)`。

    過濾一定要對**全部**做：daemon 的清單一頁只有 200 筆，而卡最久的那幾筆天生活得比一頁久
    ——`blocked` 的定義就是「還在等，保持未結案」（issue #515：正式機上 6 筆未結案有 3 筆
    在頁外，`--open` 一筆都看不到它們）。

    所以 `status` 要**送到 daemon 去**（issue #543）：為了 6 筆未結案把一千多筆搬回來一頁一頁
    翻，成本正好落在最需要的那個答案上。舊 daemon 忽略這個參數也沒關係——呼叫端那份過濾照跑，
    只是要多搬幾頁。

    沒有頁數上限：上限等於把「撈得完」換成「看起來像撈完了」，而那正是 #515 要修掉的症狀
    （issue #543 量到兩萬筆時 50 頁只看得到一萬筆）。防無窮迴圈靠的是游標**必須往前走**：
    沒動就停，而不是數頁數。

    `complete=False` = 沒撈完，而且**不知道漏了什麼**：舊 daemon 不回 `has_more`（沒有游標可
    以翻）、游標壞掉或原地打轉。呼叫端要把這件事講出來，不要把半份清單當成全部。
    """
    items: list = []
    cursor = None
    seen: set[str] = set()
    while True:
        page = client.get(
            "/api/supervisor/assignments",
            {"before": cursor, "limit": ASSIGNMENT_PAGE, "status": status},
        )
        if not isinstance(page, dict):
            return items, False
        items.extend(a for a in page.get("assignments") or [] if isinstance(a, dict))
        if "has_more" not in page:
            return items, False
        if not page.get("has_more"):
            return items, True
        cursor = page.get("next_cursor")
        if not isinstance(cursor, str) or not cursor or cursor in seen:
            return items, False
        seen.add(cursor)

def _server_status(args) -> str:
    """交給 daemon 的 `?status=`。**下面那些客戶端過濾照舊全部留著**：舊 daemon 忽略這個參數，
    而少一道就會變成「舊 daemon 上 --status 突然失效」。

    `--id` 要掃全部，所以不篩。`--open`／`--awaiting-review` 各自對應一種；兩個一起給時送
    `open`（比較寬的那個），剩下的交給客戶端那層收。
    """
    if getattr(args, "id", None):
        return ""
    if getattr(args, "status", None):
        return args.status
    if getattr(args, "open", False):
        return "open"
    if getattr(args, "awaiting_review", False):
        return "awaiting_review"
    return ""

def cmd_assignments(client: Client, cfg: dict, args) -> object:
    if args.id:
        one = optional_get(client, f"/api/supervisor/assignments/{path_segment(args.id)}")
        if one is not None:
            return one
    filtered = bool(args.id or args.status or args.open or args.awaiting_review)
    if filtered or args.all:
        items, complete = _all_assignments(client, _server_status(args))
    else:
        out = client.get("/api/supervisor/assignments")
        if not isinstance(out, dict):
            return out
        items = [a for a in out.get("assignments") or [] if isinstance(a, dict)]
        complete = "has_more" in out and not out.get("has_more")
    if args.id:
        for a in items:
            if a.get("id") == args.id or a.get("client_request_id") == args.id:
                return a
        raise AgmError(
            "not_found",
            f"找不到交辦 {args.id}" if complete else f"在撈得到的 {len(items)} 筆裡找不到交辦 {args.id}；清單沒撈完（daemon 可能還沒有分頁），不代表它不存在",
            4,
            id=args.id,
            searched=len(items),
            complete=complete,
        )
    if args.status:
        if args.status == "open":
            items = [a for a in items if a.get("status") in OPEN_STATUSES]
        else:
            items = [a for a in items if a.get("status") == args.status]
    if args.open:
        items = [a for a in items if a.get("status") in OPEN_STATUSES]
    if args.awaiting_review:
        items = [a for a in items if a.get("status") == "awaiting_review"]
    stuck = [
        {
            "id": a.get("id"),
            "target_bot_id": a.get("target_bot_id"),
            "attempts": a.get("attempts"),
            "next_attempt_at": a.get("next_attempt_at"),
            "last_error": a.get("error"),
            "created_at": a.get("created_at"),
        }
        for a in items
        if a.get("status") == "queued" and not a.get("turn_id") and (a.get("attempts") or 0) >= 3
    ]
    out = {
        "assignments": items,
        "open": len([a for a in items if a.get("status") in OPEN_STATUSES]),
        "awaiting_review": len([a for a in items if a.get("status") == "awaiting_review"]),
        "complete": complete,
    }
    if not complete:
        out["note"] = "清單沒撈完（daemon 還沒有分頁，或翻頁翻到上限）：上面的數字是下限，舊的交辦可能不在裡面"
    if stuck:
        out["retrying_undelivered"] = stuck
    return out

def cmd_review(client: Client, cfg: dict, args) -> object:
    """驗收／阻塞／續作／取消一筆交辦。

    這是唯一能把交辦結案的路徑：daemon 只會把回合跑完的交辦放到 awaiting_review，
    要不要算完成是判斷，而判斷要有人、有理由、有證據。
    """
    body: dict = {"decision": args.decision, "actor": args.actor, "source": args.source}
    for key, val in (("reason", args.reason), ("evidence", args.evidence)):
        if val:
            body[key] = val
    if args.decision == "followup":
        if not args.followup_text and not args.followup_file:
            raise AgmError("bad_args", "followup 要用 --followup-text 或 --followup-file 說明接下來做什麼", 2)
        if not args.followup_request_id:
            raise AgmError("bad_args", "followup 要一個穩定的 --followup-request-id（重試沿用同一個）", 2)
        body["followup_text"] = _text_of(args, "--followup", "followup_file", "followup_text", "續作內容")
        body["followup_request_id"] = args.followup_request_id
        if args.followup_bot:
            body["followup_bot_id"] = args.followup_bot
    path = f"/api/supervisor/assignments/{path_segment(args.assignment_id)}/review"
    try:
        return client.post(path, body)
    except AgmError as e:
        if e.kind in ("timeout", "connection_lost"):
            raise AgmError(
                "delivery_unknown",
                "驗收請求逾時，狀態未知。先用 `agm assignments --id <id>` 對帳；"
                "同樣的 decision（followup 連同同一個 --followup-request-id）可以安全重送。",
                7,
                assignment_id=args.assignment_id,
                decision=args.decision,
            )
        raise

def cmd_approval(client: Client, cfg: dict, args) -> object:
    """重建／重啟核准。申請寫成紀錄（誰、範圍、哪個 commit、到期），AGM 直接核駁。

    送出去不確定成不成功時，用**同一個** `--request-id` 重送：回的是原本那一筆（`created=false`），
    不會變成兩筆讓 AGM 一筆核一筆駁（2026-09-16 的事故）。`list` 的輸出帶 `client_request_id` 可以對帳。
    """
    if args.op == "list":
        if getattr(args, "id", None):
            return client.get("/api/supervisor/approvals", {"id": args.id})
        return client.get("/api/supervisor/approvals")
    if args.op == "request":
        if not (args.requester and args.purpose and args.scope):
            raise AgmError("bad_args", "approval request 需要 --requester、--purpose、--scope", 2)
        body: dict = {"requester": args.requester, "purpose": args.purpose, "scope": args.scope}
        if args.commit:
            body["target_commit"] = args.commit
        if args.expires_in is not None:
            body["expires_in_secs"] = args.expires_in
        if args.request_id:
            body["request_id"] = args.request_id
        if args.supersedes:
            body["supersedes"] = args.supersedes
        if args.reason:
            body["reason"] = args.reason
        return client.post("/api/supervisor/approvals", body)
    if not args.approval_id:
        raise AgmError("bad_args", "approval decide 需要 approval id", 2)
    if not args.decision:
        raise AgmError("bad_args", "approval decide 需要 --decision approve|deny|revoke", 2)
    body = {"decision": args.decision, "actor": args.actor}
    if args.reason:
        body["reason"] = args.reason
    if args.expires_in is not None:
        body["expires_in_secs"] = args.expires_in
    return client.post(f"/api/supervisor/approvals/{path_segment(args.approval_id)}/decide", body)

def single_line_token(raw: str, where: str) -> str:
    """把讀進來的內容收成「單獨一行」的 token；多行直接拒絕。

    `.strip()` 只去頭尾空白，中間的換行會留在 token 裡整串送出去，而 daemon 只會回一句
    「token 不符」——查的人得自己想到 `wc -l`（issue #477，i407 審核）。檔案與 stdin 兩條路
    用同一份規則（i267 審核：以前只有檔案那條有）。尾端多一個空行不算多行。
    """
    lines = [ln for ln in raw.splitlines() if ln.strip()]
    if len(lines) > 1:
        raise AgmError(
            "bad_args",
            f"{where} 有 {len(lines)} 行；token 是單獨一行，"
            "多半是誤把別的東西也寫進去了（送出去只會換來一句 token 不符）",
            2,
        )
    return lines[0].strip() if lines else ""

def lease_token_of(args) -> str:
    """`--lease-token-file` / `--lease-token -`（stdin）/ `--lease-token <值>`，取一個。

    **優先給檔案與 stdin**：argv 對同一個 uid 的行程是公開的（`ps` 看得到），而 `lease_token` 是
    「只在 acquire 回應出現一次、任何 API 都查不到」的一次性憑證——拿到就能收掉別人正在換 binary 的
    窗口（issue #477）。`daemon-update-kick.sh` 早就把它寫進 0600 的檔、避免它進派工正文，
    但 child 照著 `--lease-token "$(cat …)"` 帶上時又回到 argv，前面那些功夫就白做了。

    檔案要求權限不寬於 0600：group／other 讀得到的話當成已經外洩，直接拒絕而不是照用。
    """
    path = getattr(args, "lease_token_file", None)
    inline = getattr(args, "lease_token", None)
    if path and inline:
        raise AgmError("bad_args", "--lease-token 與 --lease-token-file 只能給一個", 2)
    if path:
        try:
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        except OSError as e:
            raise AgmError("bad_args", f"讀不到 lease token 檔 {path}：{e}", 2) from e
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode):
            os.close(fd)
            raise AgmError("bad_args", f"lease token 檔 {path} 不是普通檔案", 2)
        with os.fdopen(fd, "rb") as fh:
            if st.st_mode & 0o077:
                raise AgmError(
                    "bad_args",
                    f"lease token 檔 {path} 的權限是 {st.st_mode & 0o777:o}，group／other 讀得到就當它已經外洩；"
                    "請 chmod 600 之後重拿一次窗口",
                    2,
                )

            try:
                raw = fh.read().decode("utf-8")
            except (OSError, UnicodeDecodeError) as e:
                raise AgmError("bad_args", f"讀不到 lease token 檔 {path}：{e}", 2) from e
        tok = single_line_token(raw, f"lease token 檔 {path}")
        if not tok:
            raise AgmError("bad_args", f"lease token 檔 {path} 是空的", 2)
        return tok
    if inline == "-":
        tok = single_line_token(sys.stdin.read(), "--lease-token - 的 stdin")
        if not tok:
            raise AgmError("bad_args", "--lease-token - 要從 stdin 讀，但 stdin 是空的", 2)
        return tok
    return inline or ""

def cmd_lease(client: Client, cfg: dict, args) -> object:
    """執行租約：等安全窗口用 `safety`（唯讀），真的要動手用 `acquire`。

    兩件事分開是刻意的：safety 只說「現在」，acquire 會在同一個鎖裡重驗一次再把窗口拿走，
    拿著 restart 租約期間 supervisor assignment 派送暫停；其他送訊息路徑仍需協調。
    """
    if args.op == "status":
        return client.get("/api/supervisor/leases")
    if args.op == "safety":
        query = {}
        if args.exclude_bot:
            query["exclude"] = ",".join(args.exclude_bot)
        if args.approval:
            query["approval"] = args.approval
        if args.owner:
            query["owner"] = args.owner
        return client.get("/api/supervisor/maintenance/safety", query or None)
    if not args.resource:
        raise AgmError("bad_args", f"lease {args.op} 需要 resource（rebuild / restart）", 2)
    owner = args.owner or os.environ.get("AM_BOT_ID", "principal:user")
    path = f"/api/supervisor/leases/{path_segment(args.resource)}/{args.op}"
    if args.op == "acquire":
        if not args.approval:
            raise AgmError("bad_args", "lease acquire 需要 --approval（核准 id）", 2)
        request_id = args.request_id or uuid.uuid4().hex
        body: dict = {"owner": owner, "approval_id": args.approval, "require_idle": not args.allow_busy, "request_id": request_id}
        if args.commit:
            body["commit"] = args.commit
        if args.ttl is not None:
            body["ttl_secs"] = args.ttl
        if args.exclude_bot:
            body["exclude_bot_ids"] = list(args.exclude_bot)
        try:
            return client.post(path, body)
        except AgmError as e:
            if e.kind in ("timeout", "connection_lost"):
                raise AgmError(
                    "delivery_unknown",
                    f"acquire 逾時或連線中斷，租約可能已經開了、lease_token 卻沒收到。用**同一個** --request-id {request_id} "
                    "重送一次拿回同一張租約（回應 replayed=true）；不要換新的 id。",
                    7,
                    request_id=request_id,
                    resource=args.resource,
                    owner=owner,
                )
            raise
    if args.fence is None:
        raise AgmError("bad_args", f"lease {args.op} 需要 --fence（acquire 回傳的那個）", 2)
    body = {"owner": owner, "fence": args.fence}
    if args.ttl is not None:
        body["ttl_secs"] = args.ttl
    tok = lease_token_of(args)
    if tok:
        body["lease_token"] = tok
    if getattr(args, "force", False):
        if not args.reason:
            raise AgmError("bad_args", "lease release --force 需要 --reason（會寫進稽核紀錄）", 2)
        body["force"] = True
        body["reason"] = args.reason
    return client.post(path, body)

def cmd_persona(client: Client, cfg: dict, args) -> object:
    """人設：持久版本是權威，內嵌版只在首次安裝當種子。

    `loaded` 不會出現 `verified`：daemon 只能在啟動 CLI 時把 persona 傳進去，看不到 session
    現在握著什麼。`needs_restart=false` 不等於新版已經生效。
    """
    base = "/api/supervisor/responder/persona" if getattr(args, "role", None) == "responder" else "/api/supervisor/persona"
    if getattr(args, "role", None) == "responder" and args.op == "adopt-embedded":
        raise AgmError("bad_args", "協調者的人設用 set 更新（沒有 adopt-embedded）", 2)
    if args.op == "show":
        out = optional_get(client, base)
        if out is None:
            raise AgmError("unsupported", "這台 daemon 還沒有 persona 介面（需要更新 agents-managerd）", 6)
        if not args.full and isinstance(out, dict) and isinstance(out.get("stored"), dict):
            out["stored"] = {k: v for k, v in out["stored"].items() if k != "text"}
        return out
    if args.op == "adopt-embedded":
        body = {"actor": args.actor}
        if args.reason:
            body["reason"] = args.reason
        return client.post("/api/supervisor/persona/adopt-embedded", body)
    if args.file:
        text = read_text_file(args.file, "--file")
    elif args.text:
        text = args.text
    else:
        raise AgmError("bad_args", "persona set 需要 --text 或 --file", 2)
    body = {"text": text}
    if args.expected_version is not None:
        body["expected_version"] = args.expected_version
    return client.put(base, body)

def cmd_build_inputs(client: Client, cfg: dict, args) -> object:
    """哪些路徑會被編進 binary（含 include_str! 的 persona 與這支 CLI）。

    例行更新判斷「只動到 docs」時要用這份清單，不然 persona 改了卻被當成不必重建。
    """
    out = optional_get(client, "/api/supervisor/build-inputs")
    if out is None:
        raise AgmError("unsupported", "這台 daemon 還沒有 build-inputs 介面（需要更新 agents-managerd）", 6)
    return out

def cmd_incidents(client: Client, cfg: dict, args) -> object:
    """系統層級的故障（host 掉線、bot 該開沒開、交辦卡住、通知送不出去）。

    跟 `agm health` 的 manager_health 分開看：AGM 自己好好的，不代表系統沒事。
    """
    out = optional_get(client, "/api/supervisor/incidents", {"all": "1" if args.all else None})
    if out is None:
        raise AgmError("unsupported", "這台 daemon 還沒有 incident 介面（需要更新 agents-managerd）", 6)
    return out

def cmd_inbox(client: Client, cfg: dict, args) -> object:
    role = getattr(args, "role", None)
    if role == "mine":
        role = runtime_role(cfg)
    return client.get("/api/supervisor/inbox", {"all": "1" if args.all else None, "limit": args.limit, "role": role})

def cmd_whoami(client: Client, cfg: dict, args) -> object:
    """這支 CLI 以哪個角色、哪顆 bot 說話，以及 daemon 能不能驗證（不印 token）。"""
    headers = bot_auth_headers(cfg)
    return {
        "role": runtime_role(cfg),
        "self_bot_id": cfg.get("self_bot_id") or cfg.get("manager_bot_id") or cfg.get("bot_id"),
        "manager_bot_id": cfg.get("manager_bot_id"),
        "bot_token_present": bool(headers.get("X-AM-Bot-Token") or headers.get("X-AM-Child-Token")),
    }

def cmd_responder(client: Client, cfg: dict, args) -> object:
    """協調者（responder）：狀態、建立、啟停。建立不會啟動，要再 start 一次。"""
    if args.op == "show":
        return client.get("/api/supervisor/responder")
    if args.op == "setup":
        body = {k: v for k, v in (("identity", args.identity), ("model", args.model), ("effort", args.effort)) if v}
        return client.post("/api/supervisor/responder/setup", body)
    return client.post(f"/api/supervisor/responder/{args.op}", {})

def cmd_ack(client: Client, cfg: dict, args) -> object:
    """結案一則 inbox 通知。**只有 AGM 角色結得掉**（issue #432）。

    daemon 認的是 `X-AM-Bot-Id` ＋那顆 bot 自己的 API token；任何 pane 有 Bot id 就只能以那顆
    Bot 的身份請求，沒有 token 時不會退回共用 User token。角色權限由 daemon 再依驗過的 id 判斷。
    """
    try:
        return client.post(f"/api/supervisor/inbox/{path_segment(args.event_id)}/ack", {})
    except AgmError as e:
        if e.extra.get("status") != 403:
            raise
        detail = e.extra.get("detail")
        reason = detail.get("reason") if isinstance(detail, dict) else None
        if reason not in ("role_required", "bot_proof_mismatch"):
            raise
        if reason == "role_required":
            hint = (
                "ack 只有 AGM 角色做得到：要在巡檢或協調者自己的 pane 裡跑 bin/agm，"
                "或自己帶 X-AM-Bot-Id 與那顆 bot 的 AM_BOT_TOKEN（舊 pane 暫用 AM_HOOK_TOKEN）。"
            )
        else:
            hint = (
                "標頭帶了，但 daemon 對不上那顆 bot 的 API token（token 輪替過、"
                "或 AM_BOT_ID 與 AM_BOT_TOKEN 不是同一顆的）：這一顆要重啟才會拿到新的環境。"
            )
        raise AgmError(
            reason,
            hint + f"（這個 pane 的 AM_BOT_ID={os.environ.get('AM_BOT_ID') or '未設定'}，"
            f"runtime 的 self_bot_id={cfg.get('self_bot_id') or cfg.get('manager_bot_id') or cfg.get('bot_id') or '未設定'}）",
            3,
            status=403,
            detail=detail,
        )

def cmd_ops_alert(client: Client, cfg: dict, args) -> object:
    """排程腳本卡住了、自己解不開：推一則 durable 通知給 AGM 巡檢。

    只給 `scripts/ops/` 那幾支 kick 腳本用。同一個 (source, reason) 每小時最多一則，
    所以五分鐘一輪的腳本每輪照喊也不會灌滿 inbox。
    """
    body = {"source": args.source, "reason": args.reason}
    if args.detail:
        body["detail"] = args.detail
    return client.post("/api/supervisor/ops-alerts", body)

OPS_MANIFEST = "scripts/ops/install-manifest.tsv"
DEFAULT_REPO = "~/project/agents-manager"

class Exit:
    """指令照常輸出 JSON，但用非 0 離開（例如 `ops-sync --check` 有落差）。"""

    def __init__(self, out: object, code: int) -> None:
        self.out = out
        self.code = code

def _git(repo: Path, *args: str) -> str:
    r = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    if r.returncode != 0:
        raise AgmError("git_failed", f"git {' '.join(args)}：{r.stderr.strip()}", 2)
    return r.stdout.strip()

def _blob_at(repo: Path, rev: str, path: str) -> str | None:
    r = subprocess.run(["git", "-C", str(repo), "rev-parse", "--verify", "-q", f"{rev}:{path}"], capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else None

def _path_history(repo: Path, ref: str, path: str) -> list[tuple[str, str]]:
    """Return commits and the path that existed there, following source-file renames."""
    raw = _git(repo, "log", "--follow", "--format=%H%x00", "--name-only", ref, "--", path)
    commit = None
    rows = []
    for line in raw.splitlines():
        if "\0" in line:
            commit = line.split("\0", 1)[0]
        elif commit and line:
            rows.append((commit, line))
    return rows

def _target_path_history(repo: Path, ref: str, target: str, source: str) -> list[tuple[str, str]]:
    """Return this install target's source history, including manifest-recorded source renames.

    Git's similarity-based rename detection can miss a rename when the new file is also rewritten.
    The install manifest is the authoritative link between source paths and an installed target, so
    use its history to discover those paths and verify that each candidate commit mapped the target
    to the candidate path before accepting its contents as a published version.
    """
    manifest_commits = _git(repo, "log", "--format=%H", ref, "--", OPS_MANIFEST).split()
    manifest_cache = {}

    def mapped_source(rev: str) -> str | None:
        if rev not in manifest_cache:
            result = subprocess.run(
                ["git", "-C", str(repo), "show", f"{rev}:{OPS_MANIFEST}"],
                capture_output=True,
                text=True,
            )
            content = result.stdout if result.returncode == 0 else ""
            mapping = {}
            for line in content.splitlines():
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                parts = line.split()
                if len(parts) >= 2:
                    mapping[parts[1]] = parts[0]
            manifest_cache[rev] = mapping
        return manifest_cache[rev].get(target)

    manifest_revs = list(dict.fromkeys([ref] + manifest_commits))
    sources = {source}
    for rev in manifest_revs:
        historical_source = mapped_source(rev)
        if historical_source:
            sources.add(historical_source)

    candidates = set()
    for historical_source in sources:
        for commit, historical_path in _path_history(repo, ref, historical_source):
            if mapped_source(commit) == historical_path and _blob_at(repo, commit, historical_path) is not None:
                candidates.add((commit, historical_path))
    for commit in manifest_revs:
        historical_source = mapped_source(commit)
        if historical_source and _blob_at(repo, commit, historical_source) is not None:
            candidates.add((commit, historical_source))

    order = {commit: i for i, commit in enumerate(_git(repo, "rev-list", "--topo-order", ref).split())}
    return sorted(candidates, key=lambda item: (order.get(item[0], len(order)), item[1]))

LAUNCHD_PREFIX = "LaunchAgents/"
PLIST_IGNORED_KEYS = ("EnvironmentVariables",)

SYSTEMD_PREFIX = "systemd/"
UNIT_IGNORED_KEYS = ("Environment",)
OPS_PLATFORMS = ("darwin", "linux")
_PREFIX_PLATFORM = {LAUNCHD_PREFIX: "darwin", SYSTEMD_PREFIX: "linux"}

def _ops_platform() -> str:
    """這台是哪個平台（對照表第三欄的值）；`AGM_OPS_PLATFORM` 只給測試蓋掉。"""
    override = os.environ.get("AGM_OPS_PLATFORM", "").strip()
    if override:
        return override
    return "linux" if sys.platform.startswith("linux") else sys.platform

def _launch_agents_dir() -> Path:
    """`~/Library/LaunchAgents`；`AGM_LAUNCHAGENTS_DIR` 只給測試蓋掉（不要讓測試讀到真的 plist）。"""
    override = os.environ.get("AGM_LAUNCHAGENTS_DIR", "").strip()
    return pathlib.Path(override) if override else pathlib.Path.home() / "Library" / "LaunchAgents"

def _systemd_user_dir() -> Path:
    """systemd 讀 user unit 的目錄（`$XDG_CONFIG_HOME/systemd/user`）；`AGM_SYSTEMD_USER_DIR` 只給測試蓋掉。"""
    override = os.environ.get("AGM_SYSTEMD_USER_DIR", "").strip()
    if override:
        return pathlib.Path(override)
    xdg = os.environ.get("XDG_CONFIG_HOME", "").strip()
    return (pathlib.Path(xdg) if xdg else pathlib.Path.home() / ".config") / "systemd" / "user"

def _install_path(agm_dir: Path, target: str) -> Path:
    """對照表的安裝位置：預設相對 AGM 目錄，`LaunchAgents/` 開頭的走 [`_launch_agents_dir`]（issue #487），
    `systemd/` 開頭的走 [`_systemd_user_dir`]（issue #677）。"""
    if target.startswith(LAUNCHD_PREFIX):
        return _launch_agents_dir() / target[len(LAUNCHD_PREFIX):]
    if target.startswith(SYSTEMD_PREFIX):
        return _systemd_user_dir() / target[len(SYSTEMD_PREFIX):]
    return agm_dir / target

def _unit_semantics(raw: bytes) -> dict:
    """systemd unit 的語意：`{"<Section>.<Key>": [值, …]}`，註解、空行、行尾 `\\` 接續都攤平（issue #677）。

    同一個鍵出現幾次、什麼順序都算（`ExecStart=` 可以有好幾行，順序就是執行順序）；
    [`UNIT_IGNORED_KEYS`] 只忽略值、留存在標記，跟 [`_plist_semantics`] 同一個道理。
    """
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as e:
        return {"_error": f"{type(e).__name__}: {e}"}
    out: dict = {}
    section = ""
    pending = ""
    for n, line in enumerate(text.splitlines(), 1):
        line = line.strip()
        if line and line[0] in "#;":
            continue
        line = pending + line
        pending = ""
        if line.endswith("\\"):
            pending = line[:-1] + " "
            continue
        if not line or line[0] in "#;":
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1]
            continue
        key, eq, value = line.partition("=")
        if not eq or not section:
            return {"_error": f"第 {n} 行看不懂：{line}"}
        key = f"{section}.{key.strip()}"
        out.setdefault(key, []).append(value.strip())
    for k in list(out):
        if k.split(".", 1)[1] in UNIT_IGNORED_KEYS:
            out[k] = "<ignored>"
    return out

def _git_bytes(repo: Path, ref: str, source: str) -> bytes:
    return subprocess.run(["git", "-C", str(repo), "show", f"{ref}:{source}"], capture_output=True, check=True).stdout

HOME_PLACEHOLDER = b"@HOME@"

def _render_home(raw: bytes) -> bytes:
    """樣板的 `@HOME@` 換成這台的家目錄（XML 轉義，與 render-launchd.sh 同一套），沒有佔位就原樣。"""
    if HOME_PLACEHOLDER not in raw:
        return raw
    return raw.replace(HOME_PLACEHOLDER, html.escape(str(Path.home()), quote=False).encode("utf-8"))

def _plist_semantics(raw: bytes) -> dict:
    """parse 過的 plist，除了 [`PLIST_IGNORED_KEYS`] 的**值**以外全部保留（issue #499）。

    被忽略的鍵仍然留下一個存在標記：整個鍵不見也是落差——安裝端把 `EnvironmentVariables` 整份掉了
    （job 於是少了 `PATH`）不該報成同步。讀不懂就回一個帶錯誤的 dict，讓結果是 drift 而不是靜靜當成相同。
    """
    try:
        d = plistlib.loads(_render_home(raw))
    except Exception as e:  # noqa: BLE001 - 壞掉的 plist 要報成落差，不是炸掉整份報告
        return {"_error": f"{type(e).__name__}: {e}"}
    if not isinstance(d, dict):
        return {"_error": f"plist 根節點不是 dict（是 {type(d).__name__}）"}
    return {k: ("<ignored>" if k in PLIST_IGNORED_KEYS else v) for k, v in d.items()}

def ops_sync_report(repo: Path, ref: str, agm_dir: Path) -> dict:
    """唯讀比對已安裝的 ops 腳本與 repo：只跑 git 與讀檔，不改任何安裝檔。

    四種落差分開報，嚴重度不同：`drift`（安裝檔不是 repo 任何一版＝有人直接改了安裝檔；腳本比位元組、plist／unit 比語意，
    兩種都回頭找歷史）、
    `behind`（repo 有更新沒裝，附落後的 commit）、`missing`（對照表有、安裝端沒有）、
    `extra`（`bin/` 裡有、對照表沒有——沒有版控的腳本）。
    """
    manifest = _git(repo, "show", f"{ref}:{OPS_MANIFEST}")
    platform = _ops_platform()
    entries = []
    skipped = []
    for line in manifest.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        flags = parts[2:]
        platforms = [t for t in flags if t != "shared"]
        if len(parts) < 2 or len(platforms) > 1 or flags.count("shared") > 1 or any(t not in OPS_PLATFORMS for t in platforms):
            raise AgmError("bad_manifest", f"{OPS_MANIFEST} 這行看不懂（要「來源 安裝位置 [darwin|linux] [shared]」）：{line}", 2)
        only = platforms[0] if platforms else None
        for prefix, want in _PREFIX_PLATFORM.items():
            if parts[1].startswith(prefix) and only != want:
                raise AgmError("bad_manifest", f"{OPS_MANIFEST}：{prefix}… 只在 {want} 上有意義，第三欄要寫 {want}：{line}", 2)
        if only is not None and only != platform:
            skipped.append({"source": parts[0], "target": parts[1], "platform": only})
            continue
        entries.append((parts[0], parts[1]))
    report: dict = {"ref": ref, "commit": _git(repo, "rev-parse", "--short", ref), "agm_dir": str(agm_dir),
                    "platform": platform, "ok": [], "behind": [], "drift": [], "missing": [], "extra": [],
                    "skipped": skipped}
    for source, target in entries:
        path = _install_path(agm_dir, target)
        row: dict = {"source": source, "target": target}
        if not path.is_file():
            report["missing"].append(row)
            continue
        semantics = _plist_semantics if target.startswith(LAUNCHD_PREFIX) else (
            _unit_semantics if target.startswith(SYSTEMD_PREFIX) else None)
        if semantics is not None:
            want = semantics(_git_bytes(repo, ref, source))
            got = semantics(path.read_bytes())
            if want == got:
                report["ok"].append(row)
            else:
                found = None
                history = _target_path_history(repo, ref, target, source)
                if "_error" not in got:
                    for c, historical_source in history:
                        if semantics(_git_bytes(repo, c, historical_source)) == got:
                            found = c
                            break
                if found is not None:
                    titles = _git(repo, "log", "--follow", "--format=%h %s", f"{found}..{ref}", "--", source).splitlines()
                    row.update({"installed_commit": found[:8], "behind": len(titles), "commits": titles})
                    report["behind"].append(row)
                    continue
                keys = sorted(set(want) | set(got))
                row["diff"] = {k: {"repo": want.get(k), "installed": got.get(k)} for k in keys if want.get(k) != got.get(k)}
                report["drift"].append(row)
            continue
        installed = _git(repo, "hash-object", str(path))
        if installed == _blob_at(repo, ref, source):
            report["ok"].append(row)
            continue
        history = _target_path_history(repo, ref, target, source)
        found = next((c for c, historical_source in history
                      if _blob_at(repo, c, historical_source) == installed), None)
        if found is None:
            report["drift"].append(row)
            continue
        titles = _git(repo, "log", "--follow", "--format=%h %s", f"{found}..{ref}", "--", source).splitlines()
        row.update({"installed_commit": found[:8], "behind": len(titles), "commits": titles})
        report["behind"].append(row)
    listed = {t for _, t in entries}
    if platform == "linux":
        sched_dir, prefix, globs = _systemd_user_dir(), SYSTEMD_PREFIX, ("com.agm.*.service", "com.agm.*.timer")
    else:
        sched_dir, prefix, globs = _launch_agents_dir(), LAUNCHD_PREFIX, ("com.agm.*.plist",)
    if sched_dir.is_dir():
        for f in sorted(p for g in globs for p in sched_dir.glob(g)):
            rel = f"{prefix}{f.name}"
            if rel not in listed:
                report["extra"].append({"target": rel})
    bin_dir = agm_dir / "bin"
    if bin_dir.is_dir():
        for f in sorted(bin_dir.iterdir()):
            rel = f"bin/{f.name}"
            if f.is_file() and f.name != "agm" and ".bak" not in f.name and not re.search(r"\.new\.\d+$", f.name) and rel not in listed:
                report["extra"].append({"target": rel})
    report["in_sync"] = not any(report[k] for k in ("behind", "drift", "missing", "extra"))
    return report

def short_hash(content: bytes) -> str:
    """FNV-1a 64、取前 12 碼十六進位——跟 daemon 的 `cli_refresh::short_hash` 是同一個算法。

    不能用 `hashlib`：比對的對象是 daemon 回報的雜湊，兩邊算的東西不一樣就永遠對不上。
    """
    h = 0xCBF29CE484222325
    for b in content:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFF_FFFF_FFFF_FFFF
    return f"{h:016x}"[:12]

def _blob_bytes(repo: Path, rev: str, path: str) -> bytes | None:
    """`git show <rev>:<path>` 的**原始位元組**（`_git` 會 strip，雜湊就對不上了）。"""
    r = subprocess.run(["git", "-C", str(repo), "show", f"{rev}:{path}"], capture_output=True)
    return r.stdout if r.returncode == 0 else None

def cli_sync_report(client: Client, repo: Path, ref: str) -> dict:
    """已安裝的 `bin/agm` 跟得上嗎（issue #532）。

    ops 腳本是手動安裝的，`bin/agm` 是 daemon 開機時從 binary 內嵌的那份寫出來的——兩邊各走各的，
    而 `ops-sync --check` 以前只看前者。於是「腳本比 CLI 新」一律回報 ok，正是最會痛的那一種：
    `daemon-update-kick.sh` 派出的正文用 `--lease-token-file`，舊的 `bin/agm` 不認得就 argparse rc 2，
    rebuild 窗口沒交還、握到 TTL（預設 900 秒），期間 assignment 派送也停著。

    兩段落差分開報，因為修法完全不同：
    * `installed`（安裝的 ≠ binary 內嵌的）→ `agm ops-sync --check --refresh-cli` 或下一次開機就好，**不必重建**；
    * `binary`（binary 內嵌的 ≠ repo 的 `scripts/agm.py`）→ 只能重建 binary＋**重啟 daemon**。

    daemon 問不到時回 `state: "unknown"`：這支的本業是比對安裝端，不該因為 daemon 沒開就整個失敗。
    """
    out: dict = {"ref": ref, "state": "ok", "roles": [], "stale_roles": [], "binary_behind": False}
    blob = _blob_bytes(repo, ref, "scripts/agm.py")
    out["repo_hash"] = short_hash(blob) if blob is not None else None
    try:
        info = client.get("/api/supervisor/cli")
    except Exception as e:
        out.update({"state": "unknown", "error": str(e)})
        return out
    if not isinstance(info, dict):
        out.update({"state": "unknown", "error": f"daemon 回了非預期的內容：{info!r}"})
        return out
    out["embedded_hash"] = info.get("embedded_hash")
    out["roles"] = info.get("roles") or []
    out["stale_roles"] = [r for r in out["roles"] if r.get("state") in ("stale", "missing")]
    out["binary_behind"] = bool(out["repo_hash"] and out["embedded_hash"] and out["repo_hash"] != out["embedded_hash"])
    if out["stale_roles"] or out["binary_behind"]:
        out["state"] = "drift"
        bits = []
        if out["stale_roles"]:
            bits.append("已安裝的 bin/agm 不是這顆 binary 內嵌的那份（`agm ops-sync --check --refresh-cli` 就地換，不必重建）")
        if out["binary_behind"]:
            bits.append(f"這顆 binary 內嵌的 agm.py 落後 {ref}：要重建 binary **並重啟 daemon** 才會換上（重啟要另外申請核准）")
        out["action"] = "；".join(bits)
    return out

def cmd_ops_sync(client: Client, cfg: dict, args) -> object:
    repo = Path(args.repo or os.environ.get("AGM_REPO") or DEFAULT_REPO).expanduser()
    report = ops_sync_report(repo, args.ref, runtime_dir(args.runtime_dir))
    cli = cli_sync_report(client, repo, args.ref)
    if args.refresh_cli and cli["stale_roles"]:
        cli["refreshed"] = client.post("/api/supervisor/cli", {})
        cli = cli_sync_report(client, repo, args.ref)
    report["cli"] = cli
    report["in_sync"] = report["in_sync"] and cli["state"] != "drift"
    if report["in_sync"]:
        return report
    if args.alert:
        counts = "、".join(f"{k} {len(report[k])}" for k in ("drift", "behind", "missing", "extra") if report[k])
        def _name(row: dict) -> str:
            diff = row.get("diff")
            if not diff:
                return row["target"]
            bits = "；".join(f"{k}: repo={v['repo']!r} 安裝={v['installed']!r}" for k, v in diff.items())
            return f"{row['target']}（{bits}）"

        names = ", ".join(_name(r) for k in ("drift", "behind", "missing", "extra") for r in report[k])
        if counts:
            detail = f"已安裝的 ops 腳本跟 {args.ref}（{report['commit']}）不一致：{counts}（{names}）。`agm ops-sync --check` 看明細，照 scripts/ops/README.md 重新 install"
        else:
            detail = f"已安裝的 ops 腳本跟 {args.ref}（{report['commit']}）一致。"
        if cli["state"] == "drift":
            stale = "、".join(f"{r['role']}({r['state']})" for r in cli["stale_roles"]) or "無"
            detail += f" bin/agm：{cli['action']}（安裝端落後的角色：{stale}）"
        report["alert"] = client.post("/api/supervisor/ops-alerts", {"source": "ops-sync", "reason": "installed_out_of_sync", "detail": detail})
    return Exit(report, 1)

def cmd_release_triage(client: Client, cfg: dict, args) -> object:
    """上游新版分診（issue #204）：模型交回 verdict、查帳本、標記派出、重試 publish。

    `submit --file` 是模型唯一的出口：它不直接跑 `gh`。daemon 驗過（每個 kept／unmatched entry 都有
    verdict、提案只引用 guard／adopt）才收，`## 來源` 的引用由 daemon 從帳本原文貼。
    """
    if args.op == "show":
        return client.get("/api/release-triage", {"kind": args.kind, "version": (args.version or [None])[0]})
    if args.op == "submit":
        if not args.file:
            raise AgmError("bad_args", "submit 要 --file <verdicts.json>", 2)
        try:
            body = json.loads(Path(args.file).read_text(encoding="utf-8"))
        except (OSError, ValueError) as e:
            raise AgmError("bad_args", f"讀不了 {args.file}：{e}", 2)
        if not isinstance(body, dict):
            raise AgmError("bad_args", f"{args.file} 要是一個 JSON 物件（{{kind,version,verdicts,issues}}）", 2)
        return client.post("/api/release-triage/verdicts", body)
    if args.op == "auto":
        if not args.kind:
            raise AgmError("bad_args", "auto 要 --kind", 2)
        if args.check:
            return client.post("/api/release-triage/auto", {"kind": args.kind, "versions": [], "check_only": True})
        if not args.version:
            raise AgmError("bad_args", "auto 要至少一個 --version", 2)
        body = {"kind": args.kind, "versions": args.version}
        if args.binary_diff_file:
            try:
                body["binary_diff"] = Path(args.binary_diff_file).read_text(encoding="utf-8", errors="replace")
            except OSError as e:
                raise AgmError("bad_args", f"讀不了 {args.binary_diff_file}：{e}", 2)
        return client.post("/api/release-triage/auto", body)
    if args.op == "dispatched":
        if not args.kind or not args.version or not args.bot:
            raise AgmError("bad_args", "dispatched 要 --kind、至少一個 --version，還有 --bot（收件 bot id）", 2)
        return client.post("/api/release-triage/dispatched", {"kind": args.kind, "versions": args.version, "bot_id": args.bot})
    body = {}
    if args.kind:
        body["kind"] = args.kind
    if args.version:
        body["version"] = args.version[0]
    if getattr(args, "dry_run", False):
        body["dry_run"] = True
    return client.post("/api/release-triage/publish", body)

def cmd_handoff(client: Client, cfg: dict, args) -> object:
    if args.summary is None and args.summary_file is None:
        return client.get("/api/supervisor/handoff")
    if args.summary is not None and args.summary_file is not None:
        raise AgmError("bad_args", "--summary 與 --summary-file 只能給一個", 2)
    if args.summary_file is not None and not args.summary_file:
        raise AgmError("bad_args", "--summary-file 的路徑不可為空", 2)
    summary = _text_of(args, "--summary", "summary_file", "summary", "管理摘要")
    return client.put("/api/supervisor/handoff", {"summary": summary})

QUOTA_PROBE_TIMEOUT = 300.0

def cmd_peers(client: Client, cfg: dict, args) -> object:
    project_id = _project_id()
    query = {"for": args.for_track} if getattr(args, "for_track", None) else None
    return client.get("/api/projects/" + urllib.parse.quote(project_id, safe="") + "/peers", query)

def _lineage_path(tail: str = "") -> str:
    return "/api/projects/" + urllib.parse.quote(_project_id(), safe="") + "/lineage" + tail

def _lineage_body(args) -> dict:
    """advance／add 共用：只送有給的欄位（daemon 對沒給的欄位不動或用預設）。"""
    body = {"line": args.line, "session_id": args.session_id}
    for key, attr in (("parent_session_id", "parent"), ("head_sha", "head"), ("summary", "summary"), ("role", "role"), ("kind", "kind"), ("context_tokens", "context_tokens"), ("last_api_at", "last_api_at")):
        value = getattr(args, attr, None)
        if value is not None and value != "":
            body[key] = value
    return body

def cmd_lineage_list(client: Client, cfg: dict, args) -> object:
    """列出本專案的 session 血統（#1943）；--line／--state 篩選。"""
    return client.get(_lineage_path(), {"line": args.line, "state": args.state})

def cmd_lineage_add(client: Client, cfg: dict, args) -> object:
    """登記一顆進行中（active）的 session；之後做完用 advance 前移、放棄用 retire。"""
    return client.post(_lineage_path(), _lineage_body(args))

def cmd_lineage_advance(client: Client, cfg: dict, args) -> object:
    """把 session 設為該線的檢查點；原檢查點改 retired（回應的 `retired`）。"""
    return client.post(_lineage_path("/advance"), _lineage_body(args))

def cmd_lineage_retire(client: Client, cfg: dict, args) -> object:
    return client.post(_lineage_path("/retire"), {"session_id": args.session_id})

def _valid_line(line: str) -> bool:
    return (
        bool(line)
        and 1 <= len(line) <= 64
        and line[0].isalnum()
        and all(c.isalnum() or c in "._-" for c in line)
    )

def _glob_to_regex(pat: str) -> str:
    pat = pat.replace("\\", "/").strip()
    while pat.startswith("/"):
        pat = pat[1:]
    parts: list[str] = []
    i = 0
    n = len(pat)
    while i < n:
        if pat[i:i + 3] == "/**":
            parts.append("(?:/.*)?")
            i += 3
        elif pat[i:i + 3] == "**/":
            parts.append("(?:.*/)?")
            i += 3
        elif pat[i:i + 2] == "**":
            parts.append(".*")
            i += 2
        elif pat[i] == "*":
            parts.append("[^/]*")
            i += 1
        elif pat[i] == "?":
            parts.append("[^/]")
            i += 1
        elif pat[i] == "[":
            j = i + 1
            if j < n and pat[j] in "!^":
                j += 1
            if j < n and pat[j] == "]":
                j += 1
            while j < n and pat[j] != "]":
                j += 1
            if j >= n:
                parts.append("\\[")
                i += 1
            else:
                stuff = pat[i + 1:j]
                if stuff.startswith(("!", "^")):
                    stuff = "^" + stuff[1:]
                parts.append(f"[{stuff}]")
                i = j + 1
        else:
            parts.append(re.escape(pat[i]))
            i += 1
    return r"^" + "".join(parts) + r"$"

def _match_glob(path: str, pat: str) -> bool:
    path = path.replace("\\", "/").strip()
    while path.startswith("./"):
        path = path[2:]
    return bool(re.match(_glob_to_regex(pat), path))

def _load_lines_toml(path: Path) -> dict:
    try:
        import importlib
        tomllib = importlib.import_module("tomllib")
    except ImportError:
        try:
            import importlib
            tomllib = importlib.import_module("tomli")
        except ImportError:
            raise AgmError("no_tomllib", "讀取 toml 需要標準庫 tomllib（Python 3.11+）", 1)
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except Exception as e:
        raise AgmError("invalid_lines_toml", f"解析 {path} 失敗：{e}", 1)

def _resolve_repo(explicit: str | None = None) -> Path:
    if explicit:
        p = Path(explicit).expanduser().resolve()
        if p.is_dir():
            return p
        raise AgmError("no_repo", f"指定的 repo 目錄不存在：{explicit}", 2)
    env_repo = os.environ.get("AGM_REPO")
    if env_repo:
        p = Path(env_repo).expanduser().resolve()
        if p.is_dir():
            return p
    here = Path(__file__).resolve().parent
    if here.parent.is_dir() and (here.parent / ".git").exists():
        return here.parent
    try:
        r = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=False)
        if r.returncode == 0 and r.stdout.strip():
            return Path(r.stdout.strip()).resolve()
    except Exception:
        pass
    return Path(DEFAULT_REPO).expanduser().resolve()

def _resolve_lines_file(explicit: str | None, repo: Path) -> Path:
    if explicit:
        p = Path(explicit).expanduser()
        if not p.is_absolute():
            p = (Path.cwd() / p).resolve()
        if not p.is_file():
            raise AgmError("no_lines_config", f"找不到 lines.toml 定義檔：{p}", 1)
        return p
    env_lines = os.environ.get("AGM_LINES_FILE") or os.environ.get("LINES_TOML")
    if env_lines:
        p = Path(env_lines).expanduser()
        if not p.is_absolute():
            p = (Path.cwd() / p).resolve()
        if not p.is_file():
            raise AgmError("no_lines_config", f"找不到 lines.toml 定義檔：{p}", 1)
        return p
    default_p = repo / "docs" / "notes" / "lines.toml"
    if not default_p.is_file():
        raise AgmError("no_lines_config", f"找不到 lines.toml 定義檔：{default_p}", 1)
    return default_p

def cmd_lineage_pick(client: Client | None, cfg: dict, args) -> object:
    """依 lines.toml 把路徑對到線（#1943 子票 B2）；全同線回 line，跨線或 unmatched 回 null。"""
    repo = _resolve_repo(getattr(args, "repo", None))
    lines_file = _resolve_lines_file(getattr(args, "lines_file", None), repo)
    lines_cfg = _load_lines_toml(lines_file)
    lines_dict = lines_cfg.get("lines", {})
    if not isinstance(lines_dict, dict):
        raise AgmError("invalid_lines_config", "lines.toml 格式錯誤：缺少 [lines] 表格", 1)

    compiled_lines: list[tuple[str, list[str]]] = []
    for line_name, line_info in lines_dict.items():
        if not _valid_line(line_name):
            raise AgmError("invalid_line_name", f"模組線名稱不合規則：{line_name}（須為 1–64 個英數字元或 ._-，且以英數開頭）", 1, line=line_name)
        if not isinstance(line_info, dict) or "paths" not in line_info:
            raise AgmError("invalid_lines_config", f"模組線 `{line_name}` 缺少 paths 設定", 1, line=line_name)
        paths_patterns = line_info["paths"]
        if not isinstance(paths_patterns, list) or not all(isinstance(p, str) for p in paths_patterns):
            raise AgmError("invalid_lines_config", f"模組線 `{line_name}` 的 paths 必須是字串清單", 1, line=line_name)
        compiled_lines.append((line_name, paths_patterns))

    lines_matched: dict[str, list[str]] = {}
    unmatched: list[str] = []

    for raw_p in args.paths:
        p = raw_p.strip()
        norm_p = p.replace("\\", "/")
        while norm_p.startswith("./"):
            norm_p = norm_p[2:]
        matched_any = False
        for line_name, patterns in compiled_lines:
            if any(_match_glob(norm_p, pat) for pat in patterns):
                target = lines_matched.setdefault(line_name, [])
                if p not in target:
                    target.append(p)
                matched_any = True
        if not matched_any:
            if p not in unmatched:
                unmatched.append(p)

    line = list(lines_matched.keys())[0] if len(lines_matched) == 1 and not unmatched else None

    return {
        "lines": lines_matched,
        "unmatched": unmatched,
        "line": line,
    }

def cmd_lineage_since(client: Client, cfg: dict, args) -> object:
    """取該線檢查點 head_sha，計算自檢查點以來的 git diff --stat 與 prompt_prefix（#1943 子票 B2）。"""
    line = args.line.strip()
    if not _valid_line(line):
        raise AgmError("invalid_line", f"模組線名稱不合規則：{line}（須為 1–64 個英數字元或 ._-，且以英數開頭）", 2, line=line)

    res = client.get(_lineage_path(), {"line": line, "state": "checkpoint"})
    sessions = res.get("sessions", []) if isinstance(res, dict) else []
    checkpoint = next((s for s in sessions if s.get("state") == "checkpoint" and s.get("line") == line), None)
    if not checkpoint:
        raise AgmError("no_checkpoint", f"模組線 `{line}` 目前沒有檢查點 session", 1, line=line)

    checkpoint_session = checkpoint.get("session_id")
    head_sha = checkpoint.get("head_sha")
    if not head_sha or not str(head_sha).strip():
        raise AgmError("no_head_sha", f"模組線 `{line}` 的檢查點 `{checkpoint_session}` 沒有記錄 head_sha", 1, line=line, checkpoint_session=checkpoint_session)
    head_sha = str(head_sha).strip()

    repo = _resolve_repo(getattr(args, "repo", None))
    lines_file = _resolve_lines_file(getattr(args, "lines_file", None), repo)
    lines_cfg = _load_lines_toml(lines_file)
    lines_dict = lines_cfg.get("lines", {})
    if line not in lines_dict:
        raise AgmError("unknown_line", f"lines.toml 未定義模組線 `{line}`", 1, line=line)

    line_entry = lines_dict[line]
    paths = line_entry.get("paths", [])
    if not isinstance(paths, list):
        raise AgmError("invalid_lines_config", f"lines.toml 中模組線 `{line}` 的 paths 必須是清單", 1, line=line)

    r = subprocess.run(["git", "-C", str(repo), "rev-parse", "--verify", "-q", f"{head_sha}^{{commit}}"], capture_output=True, text=True)
    if r.returncode != 0:
        raise AgmError("sha_not_found", f"commit {head_sha} 不在本地儲存庫中，請先執行 `git fetch`", 1, line=line, head_sha=head_sha)

    git_cmd = ["git", "-C", str(repo), "diff", "--stat", f"{head_sha}..HEAD"]
    if paths:
        git_cmd.extend(["--", *paths])
    diff_proc = subprocess.run(git_cmd, capture_output=True, text=True)
    if diff_proc.returncode != 0:
        raise AgmError("git_diff_failed", f"git diff 執行失敗：{diff_proc.stderr.strip()}", 1, line=line, head_sha=head_sha)

    diff_stat = diff_proc.stdout.strip()
    if diff_stat:
        prompt_prefix = f"上次之後這條線動過的檔案（{head_sha}..HEAD）：\n{diff_stat}\n\n涉及的請重讀。"
    else:
        prompt_prefix = f"上次之後這條線動過的檔案（{head_sha}..HEAD）：（無變動），涉及的請重讀。"

    return {
        "line": line,
        "checkpoint_session": checkpoint_session,
        "head_sha": head_sha,
        "diff_stat": diff_stat,
        "prompt_prefix": prompt_prefix,
    }

def _extract_session_id_from_text(text: str) -> str | None:
    """從 `codex exec fork --json` 輸出（或文字訊息）抓新產生的 session/thread id。"""
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        if line.startswith("{"):
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                obj = None
            if isinstance(obj, dict):
                for key in ("thread_id", "session_id"):
                    val = obj.get(key)
                    if isinstance(val, str) and val.strip():
                        return val.strip()
    m = re.search(r"session\s+id:\s*([A-Za-z0-9._-]+)", text, re.IGNORECASE)
    if m:
        return m.group(1)
    return None

def _codex_backup_fork(session_id: str, repo: Path) -> str:
    """跑無 prompt 的 `codex exec fork --json <session_id>` 留乾淨備份，不呼叫模型。"""
    codex_bin = os.environ.get("AGM_CODEX_BIN") or "codex"
    try:
        proc = subprocess.run(
            [codex_bin, "exec", "fork", "--json", session_id],
            cwd=str(repo),
            capture_output=True,
            text=True,
            timeout=60,
        )
    except FileNotFoundError:
        raise AgmError("codex_not_found", f"找不到 codex 執行檔：{codex_bin}", 1, base_session_id=session_id)
    except subprocess.TimeoutExpired:
        raise AgmError("backup_fork_timeout", "codex exec fork 備份逾時", 1, base_session_id=session_id)
    if proc.returncode != 0:
        msg = proc.stderr.strip() or proc.stdout.strip() or f"exit {proc.returncode}"
        raise AgmError("backup_fork_failed", f"codex exec fork 備份失敗：{msg}", 1, base_session_id=session_id)
    backup_id = _extract_session_id_from_text(proc.stdout) or _extract_session_id_from_text(proc.stderr)
    if not backup_id:
        raise AgmError("backup_fork_failed", "codex exec fork 未回傳備份 session id", 1, base_session_id=session_id)
    return backup_id

def _herdr_bin() -> str:
    return os.environ.get("AGM_HERDR_BIN") or "herdr"

def _herdr_run(cmd_args: list[str], err_kind: str, err_prefix: str) -> str:
    try:
        proc = subprocess.run([_herdr_bin(), *cmd_args], capture_output=True, text=True, timeout=90)
    except FileNotFoundError:
        raise AgmError("herdr_not_found", f"找不到 herdr 執行檔：{_herdr_bin()}", 1)
    except subprocess.TimeoutExpired:
        raise AgmError(err_kind, f"{err_prefix}：逾時", 1)
    if proc.returncode != 0:
        msg = proc.stderr.strip() or proc.stdout.strip() or f"exit {proc.returncode}"
        raise AgmError(err_kind, f"{err_prefix}：{msg}", 1)
    return proc.stdout.strip()

def _herdr_split_pane(parent_pane: str) -> str:
    out = _herdr_run(["pane", "split", "--pane", parent_pane], "herdr_split_failed", "herdr pane split 失敗")
    if out.startswith("{"):
        try:
            obj = json.loads(out)
            if isinstance(obj, dict) and isinstance(obj.get("pane_id"), str) and obj["pane_id"].strip():
                return obj["pane_id"].strip()
        except json.JSONDecodeError:
            pass
    lines = [ln.strip() for ln in out.splitlines() if ln.strip()]
    if lines:
        return lines[-1].split()[-1]
    raise AgmError("herdr_split_failed", "herdr pane split 未回傳 pane id", 1)

def _herdr_close_pane(pane_id: str) -> None:
    try:
        subprocess.run([_herdr_bin(), "pane", "close", pane_id], capture_output=True, text=True, timeout=15)
    except Exception:
        pass

def _herdr_agent_session(target: str) -> str | None:
    out = _herdr_run(["agent", "get", target], "herdr_get_failed", "herdr agent get 失敗")
    if out.startswith("{"):
        try:
            obj = json.loads(out)
        except json.JSONDecodeError:
            obj = None
        if isinstance(obj, dict):
            sess = obj.get("agent_session")
            if isinstance(sess, dict) and isinstance(sess.get("value"), str) and sess["value"].strip():
                return sess["value"].strip()
            if isinstance(obj.get("session_id"), str) and obj["session_id"].strip():
                return obj["session_id"].strip()
    return None

def _is_role_refused(e: AgmError) -> bool:
    detail = e.extra.get("detail")
    return e.kind == "http_error" and e.extra.get("status") == 403 and isinstance(detail, dict) and detail.get("reason") == "role_required"

def _bot_from_peer(peer: dict) -> dict:
    run_state = peer.get("run_state")
    run = {"state": run_state, "native_session_id": peer.get("native_session_id")} if run_state not in (None, "stopped") else None
    return {"id": peer.get("id"), "name": peer.get("name"), "kind": peer.get("kind"), "run": run}

def _find_bot_in_peers(client: Client, bot_id: str) -> dict | None:
    data = client.get("/api/projects/" + urllib.parse.quote(_project_id(), safe="") + "/peers")
    for p in (data.get("peers") if isinstance(data, dict) else None) or []:
        if isinstance(p, dict) and p.get("id") == bot_id:
            return _bot_from_peer(p)
    return None

def _find_bot_in_state(client: Client, cfg: dict, bot_id: str) -> dict | None:
    manager = ""
    try:
        manager = manager_bot_id(cfg)
    except AgmError:
        pass
    try:
        sup = optional_get(client, "/api/supervisor/state")
    except AgmError as e:
        if not _is_role_refused(e):
            raise
        return _find_bot_in_peers(client, bot_id)
    if sup is None:
        return _find_bot_in_peers(client, bot_id)
    state = slim_state(sup, manager)
    for b in state.get("bots") or []:
        if b.get("id") == bot_id:
            return b
    return None

def _poll_fork_session(client: Client, cfg: dict, bot_id: str, base_session_id: str) -> str | None:
    for attempt in range(6):
        b = _find_bot_in_state(client, cfg, bot_id)
        run = (b or {}).get("run") or {}
        sid = run.get("native_session_id")
        if isinstance(sid, str) and sid.strip() and sid.strip() != base_session_id:
            return sid.strip()
        if attempt < 5:
            time.sleep(0.5)
    return None

def cmd_lineage_dispatch(client: Client, cfg: dict, args) -> object:
    """血統派工（#1943 B3）：pick → claim →（resume 先備份）→ 啟動 worker → 前綴 since → 送 prompt。"""
    bot_target = getattr(args, "bot", None)
    child_target = getattr(args, "child", None)
    if bool(bot_target) == bool(child_target):
        raise AgmError("bad_args", "dispatch 必須擇一指定 --bot（專家模式）或 --child（母子模式）", 2)
    paths_arg = getattr(args, "paths", None)
    line_arg = getattr(args, "line", None)
    if not paths_arg and not line_arg:
        raise AgmError("bad_args", "dispatch 必須指定 --paths 或 --line", 2)

    prompt_path = Path(args.prompt_file).expanduser()
    try:
        user_prompt = prompt_path.read_text(encoding="utf-8").strip()
    except OSError as e:
        raise AgmError("bad_args", f"讀不到 --prompt-file（{prompt_path}）：{e.strerror or e}", 2)
    if not user_prompt:
        raise AgmError("bad_args", "--prompt-file 內容不可為空", 2)

    repo = _resolve_repo(getattr(args, "repo", None))
    force_fork = bool(getattr(args, "fork", False))

    if paths_arg:
        picked = cmd_lineage_pick(None, cfg, args)
        if line_arg:
            line = line_arg.strip()
            if not _valid_line(line):
                raise AgmError("invalid_line", f"模組線名稱不合規則：{line}", 2, line=line)
            if picked.get("line") is None:
                force_fork = True
        else:
            if picked.get("line") is None:
                raise AgmError(
                    "no_single_line",
                    "路徑跨線或未匹配任何模組線，無法自動決定血統線；請改用 --line 指定或手動處理",
                    1,
                    lines=picked.get("lines"),
                    unmatched=picked.get("unmatched"),
                )
            line = str(picked["line"])
    else:
        line = str(line_arg).strip()
        if not _valid_line(line):
            raise AgmError("invalid_line", f"模組線名稱不合規則：{line}", 2, line=line)

    since_ns = argparse.Namespace(
        line=line,
        lines_file=getattr(args, "lines_file", None),
        repo=str(repo),
    )
    since = cmd_lineage_since(client, cfg, since_ns)
    base_session_id = str(since["checkpoint_session"])
    full_prompt = f"{since['prompt_prefix']}\n\n{user_prompt}"

    mode = "fork" if force_fork else "resume"
    backup_session_id: str | None = None
    claim_resp: dict | None = None

    if mode == "resume":
        backup_session_id = _codex_backup_fork(base_session_id, repo)
        try:
            res = client.post(
                _lineage_path("/claim"),
                {"line": line, "mode": "resume", "backup_session_id": backup_session_id},
            )
            claim_resp = res if isinstance(res, dict) else None
        except AgmError as e:
            detail = e.extra.get("detail") if isinstance(e.extra, dict) else None
            if e.kind == "http_error" and e.extra.get("status") == 409 and isinstance(detail, dict) and detail.get("reason") == "lineage_line_busy":
                mode = "fork"
                backup_session_id = None
            else:
                raise

    if mode == "fork":
        res = client.post(_lineage_path("/claim"), {"line": line, "mode": "fork"})
        claim_resp = res if isinstance(res, dict) else None

    if not claim_resp or not isinstance(claim_resp.get("claim"), dict) or not claim_resp["claim"].get("id"):
        raise AgmError("claim_failed", "認領血統線回傳格式錯誤", 1, line=line)
    claim_id = str(claim_resp["claim"]["id"])

    created_pane: str | None = None
    try:
        worker_info: dict[str, object]
        if bot_target:
            bot_info = _find_bot_in_state(client, cfg, bot_target)
            if bot_info is None:
                raise AgmError("bot_not_found", f"找不到目標 bot：{bot_target}", 1, bot_id=bot_target)
            op = "restart" if bot_info.get("run") else "start"
            if mode == "resume":
                q = urllib.parse.urlencode({"resume": "native", "session": base_session_id})
                start_res = client.post(f"/api/bots/{path_segment(bot_target)}/{op}?{q}", {})
                if isinstance(start_res, dict) and start_res.get("resumed") is False:
                    raise AgmError("worker_resume_failed", f"bot {bot_target} 未能接回檢查點 {base_session_id}", 1, detail=start_res)
                work_session_id = base_session_id
            else:
                q = urllib.parse.urlencode({"resume": "fork", "session": base_session_id})
                start_res = client.post(f"/api/bots/{path_segment(bot_target)}/{op}?{q}", {})
                sid = start_res.get("session_id") if isinstance(start_res, dict) else None
                work_session_id = sid.strip() if isinstance(sid, str) and sid.strip() else None
                if not work_session_id:
                    work_session_id = _poll_fork_session(client, cfg, bot_target, base_session_id)
                if not work_session_id:
                    raise AgmError("fork_session_unknown", f"bot {bot_target} fork 啟動後未取得新 session id", 1, bot_id=bot_target)
            worker_info = {"kind": "bot", "bot_id": bot_target}
        else:
            pane_id = getattr(args, "pane", None)
            if not pane_id:
                parent_pane = os.environ.get("HERDR_PANE_ID", "").strip()
                if not parent_pane:
                    raise AgmError("no_pane", "母子模式需要 --pane 或環境變數 HERDR_PANE_ID", 2)
                pane_id = _herdr_split_pane(parent_pane)
                created_pane = pane_id
            subcmd = "resume" if mode == "resume" else "fork"
            start_cmd = ["agent", "start", child_target, "--kind", "codex", "--pane", pane_id]
            if getattr(args, "identity", None):
                start_cmd.extend(["--identity", args.identity])
            start_cmd.extend(["--", subcmd, base_session_id])
            _herdr_run(start_cmd, "worker_start_failed", f"herdr agent start {child_target} 失敗")
            if mode == "resume":
                work_session_id = base_session_id
            else:
                work_session_id = _herdr_agent_session(child_target)
                if not work_session_id:
                    raise AgmError("fork_session_unknown", f"child {child_target} fork 啟動後未取得新 session id", 1, child=child_target)
            worker_info = {"kind": "child", "child": child_target, "pane_id": pane_id}

        if mode == "fork":
            try:
                client.post(
                    _lineage_path(),
                    {
                        "line": line,
                        "session_id": work_session_id,
                        "parent_session_id": base_session_id,
                        "role": "worker",
                        "kind": "codex",
                    },
                )
            except AgmError as e:
                detail = e.extra.get("detail") if isinstance(e.extra, dict) else None
                if not (e.kind == "http_error" and e.extra.get("status") == 409 and isinstance(detail, dict) and detail.get("reason") == "lineage_session_exists"):
                    raise
            client.patch(
                _lineage_path(f"/claim/{path_segment(claim_id)}"),
                {"work_session_id": work_session_id},
            )

        if bot_target:
            req_id = getattr(args, "request_id", None) or f"lineage-{claim_id}"
            client.post(
                f"/api/bots/{path_segment(bot_target)}/prompt",
                {"text": full_prompt, "client_request_id": req_id},
            )
        else:
            _herdr_run(["agent", "prompt", child_target, full_prompt], "worker_prompt_failed", f"herdr agent prompt {child_target} 失敗")
    except Exception:
        try:
            client.post(_lineage_path(f"/claim/{path_segment(claim_id)}/abandon"), {})
        except Exception:
            pass
        if created_pane:
            _herdr_close_pane(created_pane)
        raise

    return {
        "line": line,
        "mode": mode,
        "claim_id": claim_id,
        "base_session_id": base_session_id,
        "backup_session_id": backup_session_id,
        "work_session_id": work_session_id,
        "worker": worker_info,
    }

def cmd_lineage_finish(client: Client, cfg: dict, args) -> object:
    """結束血統認領（#1943 B3）：--merged 帶 claim_id 前移；--abandon 放棄並回要接手的備份 session。"""
    if args.merged:
        body: dict[str, object] = {"claim_id": args.claim_id}
        for key, attr in (("line", "line"), ("session_id", "session"), ("head_sha", "head"), ("summary", "summary")):
            val = getattr(args, attr, None)
            if val is not None and str(val).strip() != "":
                body[key] = str(val).strip()
        return client.post(_lineage_path("/advance"), body)
    return client.post(_lineage_path(f"/claim/{path_segment(args.claim_id)}/abandon"), {})

def _sol_version_key(model_id: str) -> tuple[int, ...]:
    """從模型 ID 抽數字版本組做比較（例如 gpt-6.2-sol -> (6, 2)），不寫死版號。"""
    nums = [int(x) for x in re.findall(r"\d+", model_id)]
    return tuple(nums)

def _resolve_latest_sol_model(client: Client, host: str) -> str:
    resp = client.get("/api/models", {"kind": "codex", "host": host})
    models = resp.get("models") if isinstance(resp, dict) else None
    if not isinstance(models, list):
        raise AgmError("models_unavailable", f"GET /api/models (kind=codex, host={host}) 回傳格式錯誤", 1, host=host)
    sols: list[tuple[tuple[int, ...], int, str]] = []
    for idx, entry in enumerate(models):
        if not isinstance(entry, dict):
            continue
        mid = entry.get("id")
        if not isinstance(mid, str) or not mid.strip():
            continue
        mid = mid.strip()
        if re.search(r"(?:^|[-_])sol(?:$|[-_])", mid, re.IGNORECASE):
            sols.append((_sol_version_key(mid), -idx, mid))
    if not sols:
        raise AgmError(
            "no_sol_model",
            f"GET /api/models (kind=codex, host={host}) 找不到任何 sol 模型；可用 --model 手動指定",
            1,
            host=host,
        )
    sols.sort(reverse=True)
    return sols[0][2]

def _parse_codex_exec_json(stdout: str) -> tuple[str | None, dict[str, int], str | None]:
    answer: str | None = None
    usage = {"input": 0, "cached": 0, "output": 0}
    err_msg: str | None = None
    for raw_line in stdout.splitlines():
        line = raw_line.strip()
        if not line or not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(ev, dict):
            continue
        ev_type = ev.get("type")
        if ev_type == "item.completed":
            item = ev.get("item")
            if isinstance(item, dict) and item.get("type") == "agent_message":
                txt = item.get("text")
                if isinstance(txt, str) and txt.strip():
                    answer = txt.strip()
        elif ev_type == "turn.completed":
            u = ev.get("usage")
            if isinstance(u, dict):
                usage = {
                    "input": int(u.get("input_tokens") or 0),
                    "cached": int(u.get("cached_input_tokens") or 0),
                    "output": int(u.get("output_tokens") or 0),
                }
        elif ev_type in ("turn.failed", "error"):
            err = ev.get("error")
            if isinstance(err, dict) and isinstance(err.get("message"), str):
                err_msg = err["message"].strip()
            elif isinstance(ev.get("message"), str):
                err_msg = ev["message"].strip()
    return answer, usage, err_msg

def _codex_session_totals(session_id: str) -> dict[str, int] | None:
    """父 session 的 rollout 最後一筆 `total_token_usage`（#1943 實測：ephemeral fork 的 `turn.completed` 用量是連父歷史一起累計的）。"""
    home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex")
    files = sorted((home / "sessions").glob(f"**/rollout-*{session_id}.jsonl"))
    if not files:
        return None
    last = None
    try:
        with open(files[-1], encoding="utf-8") as f:
            for raw in f:
                if '"token_count"' not in raw:
                    continue
                try:
                    info = (json.loads(raw).get("payload") or {}).get("info") or {}
                except json.JSONDecodeError:
                    continue
                if isinstance(info.get("total_token_usage"), dict):
                    last = info["total_token_usage"]
    except OSError:
        return None
    if last is None:
        return None
    return {"input": int(last.get("input_tokens") or 0), "cached": int(last.get("cached_input_tokens") or 0), "output": int(last.get("output_tokens") or 0)}

def cmd_consult(client: Client, cfg: dict, args) -> object:
    """唯讀諮詢 thinker 血統（#1943 C）：ephemeral fork 問一題，不登記、不 claim、不前移。"""
    q_arg = getattr(args, "question", None)
    f_arg = getattr(args, "prompt_file", None)
    if bool(q_arg) == bool(f_arg):
        raise AgmError("bad_args", "consult 必須擇一提供問題文字或 --prompt-file", 2)
    if f_arg:
        p = Path(f_arg).expanduser()
        try:
            question = p.read_text(encoding="utf-8").strip()
        except OSError as e:
            raise AgmError("bad_args", f"讀不到 --prompt-file（{p}）：{e.strerror or e}", 2)
    else:
        question = str(q_arg).strip()
    if not question:
        raise AgmError("bad_args", "諮詢問題不可為空", 2)

    line = (getattr(args, "line", None) or "lead").strip()
    if not _valid_line(line):
        raise AgmError("invalid_line", f"模組線名稱不合規則：{line}", 2, line=line)

    repo = _resolve_repo(getattr(args, "repo", None))
    since_ns = argparse.Namespace(line=line, lines_file=getattr(args, "lines_file", None), repo=str(repo))
    try:
        since = cmd_lineage_since(client, cfg, since_ns)
    except AgmError as e:
        if e.kind == "no_checkpoint":
            raise AgmError(
                "no_thinker_checkpoint",
                f"模組線 `{line}` 目前沒有 thinker 檢查點；請先種一顆：agm lineage advance {line} <session_id> --role thinker --head <sha>",
                1,
                line=line,
            ) from e
        raise

    checkpoint_session = str(since["checkpoint_session"])
    host = (getattr(args, "host", None) or os.environ.get("AM_HOST") or "local").strip()
    model = (getattr(args, "model", None) or "").strip() or _resolve_latest_sol_model(client, host)
    effort = (getattr(args, "effort", None) or "xhigh").strip()
    exec_timeout = float(getattr(args, "exec_timeout", None) or 900.0)

    full_prompt = (
        "[唯讀諮詢：禁止修改任何檔案、禁止執行寫入指令、禁止開啟子 agent。只回答以下問題。]\n\n"
        f"{since['prompt_prefix']}\n\n{question}"
    )

    codex_bin = os.environ.get("AGM_CODEX_BIN") or "codex"
    cmd = [
        codex_bin,
        "exec",
        "-s",
        "read-only",
        "-m",
        model,
        "-c",
        f"model_reasoning_effort={effort}",
        "--json",
        "--ephemeral",
        "fork",
        checkpoint_session,
        "-",
    ]
    try:
        proc = subprocess.run(cmd, input=full_prompt, cwd=str(repo), capture_output=True, text=True, timeout=exec_timeout)
    except FileNotFoundError:
        raise AgmError("codex_not_found", f"找不到 codex 執行檔：{codex_bin}", 1, line=line)
    except subprocess.TimeoutExpired:
        raise AgmError(
            "consult_timeout",
            f"codex exec --ephemeral fork 逾時（{exec_timeout:g}s）",
            5,
            line=line,
            checkpoint_session=checkpoint_session,
        )

    answer, usage, ev_err = _parse_codex_exec_json(proc.stdout)
    if proc.returncode != 0:
        msg = ev_err or proc.stderr.strip() or proc.stdout.strip() or f"exit {proc.returncode}"
        raise AgmError("consult_failed", f"codex exec 失敗：{msg}", 1, line=line, checkpoint_session=checkpoint_session)
    if not answer:
        msg = ev_err or "codex exec 未回傳 agent_message"
        raise AgmError("consult_no_answer", msg, 1, line=line, checkpoint_session=checkpoint_session)

    parent = _codex_session_totals(checkpoint_session)
    if parent is not None:
        usage = {k: max(0, usage[k] - parent[k]) for k in usage}
    return {
        "line": line,
        "checkpoint_session": checkpoint_session,
        "model": model,
        "effort": effort,
        "answer": answer,
        "usage": usage,
        "usage_scope": "this_call" if parent is not None else "cumulative_with_parent",
    }

def cmd_role(client: Client, cfg: dict, args) -> object:
    """專家模式：管理者替同專案的頂層 bot 設執行者／顧問角色（none 清掉）。"""
    role = None if args.role == "none" else args.role
    body = {"role": role}
    if getattr(args, "track", None):
        body["track"] = args.track
    return client.put("/api/bots/" + urllib.parse.quote(args.bot_id, safe="") + "/role", body)

def cmd_hire(client: Client, cfg: dict, args) -> object:
    """專家模式 manager 增聘成員（頂層；預設 executor，--role specialist 時必帶 --track；不自動啟動）。"""
    project_id = os.environ.get("AM_PROJECT_ID", "")
    if not project_id:
        raise AgmError("no_project", "缺少 AM_PROJECT_ID", 2)
    if not getattr(args, "kind", None):
        raise AgmError("bad_args", "hire 需要 --kind", 2)
    body = {"name": args.name}
    for key in ("kind", "model", "effort", "identity", "role", "track", "fork_from"):
        if getattr(args, key, None):
            body[key] = getattr(args, key)
    return client.post("/api/projects/" + urllib.parse.quote(project_id, safe="") + "/members", body)

def cmd_project_set_path(client: Client, cfg: dict, args) -> object:
    """專案目錄搬好之後改 AG Man 記錄的路徑（使用者或 fleet operator；專案內 bot 要先全停）。"""
    if not args.project_id.strip() or not args.path.strip():
        raise AgmError("bad_args", "project set-path 需要專案 id 與新路徑", 2)
    return client.patch("/api/projects/" + urllib.parse.quote(args.project_id.strip(), safe=""), {"path": args.path.strip()})

def _project_id() -> str:
    project_id = os.environ.get("AM_PROJECT_ID", "")
    if not project_id:
        raise AgmError("no_project", "缺少 AM_PROJECT_ID", 2)
    return project_id

def cmd_member(client: Client, cfg: dict, args) -> object:
    """把 expert 專案裡留下的 child 原地轉成成員（pane 與對話不動）。"""
    body = {key: getattr(args, key) for key in ("role", "effort", "session_id") if getattr(args, key)}
    return client.post("/api/bots/" + urllib.parse.quote(args.bot_id, safe="") + "/member", body)

def cmd_requests(client: Client, cfg: dict, args) -> object:
    return client.get("/api/relay-requests", {"from": args.from_bot, "to": args.to_bot, "open": "1" if args.open else None})

def cmd_models(client: Client, cfg: dict, args) -> object:
    """模型查詢／刷新沿用 Client 的角色憑證，權限由 daemon 判定。"""
    query = {"kind": args.kind, "host": args.host, "identity": args.identity,
             "refresh": "1" if args.refresh else None}
    return client.get("/api/models", query)

def cmd_quota(client: Client, cfg: dict, args) -> object:
    if not getattr(args, "probe", False):
        if args.account or args.host:
            raise AgmError("bad_args", "--account／--host 只配 --probe 用", 2)
        return client.get("/api/quota")
    query = {k: v for k, v in (("kind", args.kind), ("account", args.account), ("host", args.host)) if v}
    if getattr(args, "timeout", None) is None:
        client.timeout = QUOTA_PROBE_TIMEOUT
    return client.post(f"/api/quota/probe?{urllib.parse.urlencode(query)}")

def gh(argv: list[str], *, allow_fail: bool = False) -> str:
    """跑一次 `gh`，回 stdout。這條路不碰 daemon、不碰 token。"""
    try:
        r = subprocess.run(["gh", *argv], capture_output=True, text=True, timeout=GH_TIMEOUT)
    except FileNotFoundError:
        raise AgmError("no_gh", "PATH 上沒有 gh；issue 認領要靠它", 2)
    except subprocess.TimeoutExpired:
        raise AgmError("gh_timeout", f"gh {' '.join(argv[:2])} 超過 {GH_TIMEOUT:g} 秒沒回", 1)
    if r.returncode != 0 and not allow_fail:
        detail = r.stderr.strip()[:300]
        if re.search(r"rate limit|HTTP 429|too many requests", r.stderr, re.I):
            raise AgmError("gh_rate_limited", f"gh {' '.join(argv[:2])} 被限流（rc={r.returncode}）：{detail}。這不是『沒人認領』，稍後重試，不要先派工", 1, retryable=True)
        raise AgmError("gh_failed", f"gh {' '.join(argv[:2])} 失敗（rc={r.returncode}）：{detail}", 1)
    return r.stdout

def repo_args(args) -> list[str]:
    """`--repo` 有給就指名，沒給就讓 gh 自己從 cwd 的 remote 推。"""
    return ["-R", args.repo] if getattr(args, "repo", None) else []

def claiming_bot(args) -> str:
    """認領人是誰。managed pane 有 `AM_AGENT_NAME`（herdr 的 agent 名）；不在 pane 裡就要自己講。"""
    for v in (getattr(args, "bot", None), os.environ.get("AM_AGENT_NAME"), os.environ.get("AM_BOT_ID")):
        if v:
            return v
    raise AgmError("no_identity", "認不出這是誰在認領：不在 managed pane 裡（沒有 AM_AGENT_NAME／AM_BOT_ID）就要帶 --bot", 2)

def parse_iso(t: str | None) -> datetime.datetime | None:
    if not isinstance(t, str) or not t:
        return None
    try:
        return datetime.datetime.fromisoformat(t.replace("Z", "+00:00"))
    except ValueError:
        return None

def mark_of(body: str) -> tuple[str, dict] | None:
    """一則留言裡的認領／交回／進度標記。`<!-- agm:issue-claim {...} -->`，JSON 壞掉就當沒有這個標記。

    標記必須**自己一行**（行首只能有空白）：GitHub 的「引用回覆」會把原留言的原始 markdown 逐行加上 `> ` 貼進新留言，
    句子中間夾著的也不是 agm 寫的。這兩種都不算——否則有人引用別顆 bot 的交回標記，就把那顆還按著的認領清掉了。"""
    m = re.search(r"^[ \t]*<!--\s*(agm:issue-(?:claim|release|progress))\s+(\{.*?\})\s*-->[ \t]*$", body or "", re.S | re.M)
    if not m:
        return None
    try:
        payload = json.loads(m.group(2))
    except json.JSONDecodeError:
        return None
    return (m.group(1), payload) if isinstance(payload, dict) else None

def all_comments(args, number: int) -> list:
    """這張票的所有留言。

    GraphQL 的 inline comments 沒有可排序的數字留言 ID；REST 留言有數字 ID，可在同秒時
    決定競爭順序。也要讀完所有頁，避免最新的認領／交回標記落在 inline 的 100 則以外。
    """
    repo = getattr(args, "repo", None)
    path = f"repos/{repo}/issues/{number}/comments" if repo else f"repos/{{owner}}/{{repo}}/issues/{number}/comments"
    raw = gh(["api", "--paginate", f"{path}?per_page=100"])
    try:
        rows = _json_array_pages(raw or "[]")
    except ValueError:
        raise AgmError("gh_failed", f"gh api {path} 回的不是 JSON：{raw.strip()[:200]}", 1)
    return rows

def _json_array_pages(raw: str) -> list:
    """`gh api --paginate` 不加 `--slurp` 時，每一頁各自是一個 JSON 陣列（`[…]\\n[…]`）。逐段解開接成一份清單；
    不用 `--slurp`：舊版 gh 不一定有那個旗標，在這裡解開最不依賴 gh 版本。任何一段不是陣列就整份當成壞的（`ValueError`）。"""
    decoder = json.JSONDecoder()
    rows: list = []
    text = raw.strip()
    idx = 0
    while idx < len(text):
        page, idx = decoder.raw_decode(text, idx)
        if not isinstance(page, list):
            raise ValueError("page is not a JSON array")
        rows.extend(page)
        while idx < len(text) and text[idx].isspace():
            idx += 1
    return rows

def read_issue(args, number: int) -> dict:
    fields = "number,title,state,url,labels,updatedAt,comments"
    raw = gh(["issue", "view", str(number), *repo_args(args), "--json", fields])
    try:
        data = json.loads(raw or "null")
    except json.JSONDecodeError:
        raise AgmError("gh_failed", f"gh issue view {number} 回的不是 JSON：{raw.strip()[:200]}", 1)
    if not isinstance(data, dict):
        raise AgmError("gh_failed", f"gh issue view {number} 回的不是一筆 issue", 1)
    return data

def current_claim(issue: dict) -> dict | None:
    """目前的認領狀態：每個認領期的第一則有效認領勝出，交回標記會結束認領期。

    回 `{"bot", "child", "worktree", "branch", "at", "last_activity", "stale"}`，沒人認領回 `None`。
    「有動靜」取「認領留言的時間」與「這張票最後被動到的時間」裡比較晚的那個：認領的人還在留言、
    改 label、推 commit 關聯，都算它還活著。
    """
    ordered = []
    for index, c in enumerate(issue.get("comments") or []):
        if not isinstance(c, dict):
            continue
        assoc = c.get("author_association") or c.get("authorAssociation")
        if assoc is not None and str(assoc).upper() not in TRUSTED_ASSOCIATIONS:
            continue
        at = parse_iso(c.get("createdAt") or c.get("created_at"))
        if at is None:
            continue
        raw_id = c.get("id")
        try:
            numeric_id = int(raw_id)
        except (TypeError, ValueError):
            numeric_id = None
        order_id = (0, numeric_id) if numeric_id is not None else (1, index)
        ordered.append((at, order_id, index, c, mark_of(c.get("body") or "")))
    ordered.sort(key=lambda row: (row[0], row[1], row[2]))

    active = None
    for at, _order_id, index, comment, parsed in ordered:
        if parsed and parsed[0] == CLAIM_MARK:
            _kind, payload = parsed
            if active is not None:
                holder = active["payload"].get("bot")
                holder_child = active["payload"].get("child")
                if payload.get("bot") in (holder, holder_child):
                    activity_time = parse_iso(payload.get("at")) or at
                    active["last_activity"] = max(active["last_activity"], activity_time)
                    continue
                prior_activity = active["last_activity"]
                if (at - prior_activity).total_seconds() <= CLAIM_STALE_SECS:
                    active["last_activity"] = max(active["last_activity"], at)
                    continue
            raw_id = comment.get("id")
            claim_id = payload.get("claim_id") or (str(raw_id) if raw_id is not None else str(index))
            claim_time = parse_iso(payload.get("at")) or at
            active = {
                "payload": payload,
                "claim_id": str(claim_id),
                "at": claim_time,
                "last_activity": claim_time,
            }
            continue

        if parsed and parsed[0] == PROGRESS_MARK:
            _kind, payload = parsed
            if active is not None:
                holder = active["payload"].get("bot")
                holder_child = active["payload"].get("child")
                target = payload.get("claim_id")
                if payload.get("bot") in (holder, holder_child) and (target is None or str(target) == active["claim_id"]):
                    progress_time = parse_iso(payload.get("at")) or at
                    active["last_activity"] = max(active["last_activity"], progress_time)
            continue

        if parsed and parsed[0] == RELEASE_MARK:
            _kind, payload = parsed
            target = payload.get("claim_id")
            if target is None or (active is not None and str(target) == active["claim_id"]):
                active = None
            elif active is not None:
                holder = active["payload"].get("bot")
                holder_child = active["payload"].get("child")
                if payload.get("bot") in (holder, holder_child):
                    rel_time = parse_iso(payload.get("at")) or at
                    active["last_activity"] = max(active["last_activity"], rel_time)
            continue

    if active is None:
        return None
    at = active["at"]
    last = active["last_activity"]
    payload = active["payload"]
    now = datetime.datetime.now(datetime.timezone.utc)
    return {
        "bot": payload.get("bot"),
        "child": payload.get("child"),
        "worktree": payload.get("worktree"),
        "branch": payload.get("branch"),
        "claim_id": active["claim_id"],
        "at": at.isoformat(),
        "last_activity": last.isoformat(),
        "stale": (now - last).total_seconds() > CLAIM_STALE_SECS,
    }

def issue_claim_state(args, number: int) -> tuple[dict, dict | None]:
    issue = read_issue(args, number)
    issue["comments"] = all_comments(args, number)
    return issue, current_claim(issue)

def ensure_claim_label(args) -> None:
    """repo 裡要有 wip label 才能 `--add-label`（不存在時 gh 硬失敗）。已存在就失敗，無妨。"""
    gh(["label", "create", CLAIM_LABEL, *repo_args(args), "--color", "FBCA04", "--description", "有 bot 正在做（agm issue claim）"], allow_fail=True)

def reconcile_claim_label(args, number: int) -> tuple[dict, dict | None]:
    """Treat the ordered comment history as the claim source of truth and repair its `wip` projection.

    The comment and label APIs are separate writes. Re-read after each label change so a release that
    lands between a claim marker and its label (or a new claim between release and label removal)
    does not leave a successful command with the opposite state projected on the issue.
    """
    for _ in range(3):
        issue, claim = issue_claim_state(args, number)
        labels = [label.get("name") for label in issue.get("labels") or []]
        has_label = CLAIM_LABEL in labels
        if has_label == (claim is not None):
            return issue, claim
        action = "--add-label" if claim is not None else "--remove-label"
        if action == "--add-label":
            ensure_claim_label(args)
        gh(["issue", "edit", str(number), *repo_args(args), action, CLAIM_LABEL])
    issue, claim = issue_claim_state(args, number)
    has_label = CLAIM_LABEL in [label.get("name") for label in issue.get("labels") or []]
    if has_label != (claim is not None):
        raise AgmError(
            "issue_claim_state_raced",
            f"#{number} 的認領留言與 {CLAIM_LABEL} label 持續變動；先不要派工，稍後重試",
            1,
            issue=number,
            retryable=True,
        )
    return issue, claim

def claim_scope_mismatch(claim: dict, args) -> bool:
    """同一顆 bot 底下，child／worktree／branch 有講而且跟現況不同，就算另一筆認領。沒講的欄位當重試。"""
    for key in ("child", "worktree", "branch"):
        requested = getattr(args, key, None)
        if requested and requested != claim.get(key):
            return True
    return False

def held_by_other(claim: dict | None, me: str, args) -> dict | None:
    """別人還按著這張票就回那筆認領；沒人、是我自己且範圍沒變、或已經放到過期都回 None。
    `args` 為 None 時不比範圍（交回用：範圍欄位只有 claim 才有意義，#1720）。"""
    if claim is None or claim.get("stale"):
        return None
    if claim.get("bot") == me and (args is None or not claim_scope_mismatch(claim, args)):
        return None
    return claim

def claim_conflict(number: int, claim: dict) -> AgmError:
    who = claim.get("bot") or "(沒寫名字的 bot)"
    where = "，".join(f"{k} {claim[k]}" for k in ("child", "worktree", "branch") if claim.get(k))
    hours_untouched = 0
    last_act = parse_iso(claim.get("last_activity"))
    if last_act is not None:
        now = datetime.datetime.now(datetime.timezone.utc)
        hours_untouched = int((now - last_act).total_seconds() // 3600)
    touch_hint = f"；對方已 {hours_untouched} 小時沒 touch；超過 24 小時才能接手"
    return AgmError(
        "issue_claimed",
        f"#{number} 已經由 {who} 認領（{where or '沒寫 worktree／分支'}），最後動靜 {claim['last_activity']}{touch_hint}；不要再派第二顆",
        3,
        issue=number,
        claimed_by=who,
        claim=claim,
    )

COLLISION_TIMEOUT = 2.0

_SERVER_SECTION = re.compile(r"\[\s*server\s*\]\s*(#.*)?\Z")
_DATA_DIR_LINE = re.compile(r"""data_dir\s*=\s*(?:"([^"]*)"|'([^']*)')\s*(#.*)?\Z""")

def configured_data_dir(cfg_dir: Path) -> Path | None:
    """讀 `<cfg_dir>/config.toml` 的 `[server] data_dir`，規則同 daemon `startup::data_dir`：`~` 展開，相對路徑接在設定檔所在目錄後面。
    讀不到、沒有這一行、或是空的，回 None（呼叫端退回預設目錄）。只認這一行的簡單寫法，解析不出來就不猜。"""
    try:
        text = (cfg_dir / "config.toml").read_text(encoding="utf-8")
    except OSError:
        return None
    in_server = False
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("["):
            in_server = bool(_SERVER_SECTION.match(line))
            continue
        if not in_server:
            continue
        m = _DATA_DIR_LINE.match(line)
        if m:
            value = (m.group(1) if m.group(1) is not None else m.group(2)).strip()
            if not value:
                return None
            p = Path(os.path.expanduser(value))
            return p if p.is_absolute() else cfg_dir / p
    return None

def claim_identity_fields(payload: dict) -> dict:
    """撞題檢查靠 child 的 bot ID 與這次認領的 claim ID 認人認單；`child`（名字）只做顯示。有才帶。"""
    return {k: payload[k] for k in ("child_bot_id", "claim_id") if payload.get(k)}

def spool_claim_collision(number: int, title: str, me: str, payload: dict) -> None:
    """遠端 pane 沒有本機 daemon runtime 時，把認領提示交給該 bot 的 hook spool。"""
    bot_id = os.environ.get("AM_BOT_ID", "")
    instance = os.environ.get("AM_INSTANCE", "")
    component = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,127}\Z")
    if not component.fullmatch(bot_id) or (instance and not component.fullmatch(instance)):
        return
    provider = os.environ.get("AM_KIND") or "claude"
    if provider not in {"claude", "codex", "grok", "agy"}:
        return
    data_dir = os.environ.get("AM_DATA_DIR", "")
    if data_dir:
        root = Path(data_dir)
        if not root.is_absolute():
            return
    else:
        root = Path.home() / ".config" / "agents-manager"
        if instance:
            root = root / "instances" / instance
        else:
            root = configured_data_dir(root) or root
    spool_dir = root / "bots" / bot_id / "hook-spool.d"
    try:
        spool_dir.mkdir(parents=True, exist_ok=True)
        body = {
            "bot_id": bot_id,
            "provider": provider,
            "truncated": False,
            "run_id": "",
            "received_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
            "payload": {
                "hook_event_name": "AmClaimCollisionCheck",
                "issue": number,
                "title": title,
                "child": payload.get("child") or me,
                "branch": payload.get("branch"),
                **claim_identity_fields(payload),
            },
        }
        fd, temporary = tempfile.mkstemp(prefix=".tmp.", dir=spool_dir)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as f:
                json.dump(body, f, ensure_ascii=False, separators=(",", ":"))
                f.write("\n")
            target = spool_dir / f"{time.time_ns()}-{uuid.uuid4().hex}.json"
            os.replace(temporary, target)
        except OSError:
            try:
                os.unlink(temporary)
            except OSError:
                pass
    except OSError:
        return

def collision_check_hint(args, number: int, title: str, me: str, payload: dict) -> None:
    """認領成功後 best-effort 問一次撞題，不擋認領。無法確認 daemon 收件時排入 bot spool。"""
    try:
        rt = load_runtime(getattr(args, "runtime_dir", None))
        client = Client(daemon_url(rt), COLLISION_TIMEOUT, bot_auth_headers(rt))
        client.post(
            "/api/judge/collision-check",
            {"issue": number, "title": title, "child": payload.get("child") or me, "branch": payload.get("branch"), **claim_identity_fields(payload)},
        )
    except AgmError as e:
        status = e.extra.get("status")
        retryable = e.kind in {"no_runtime", "connect_failed", "connection_lost", "timeout"}
        if e.kind == "http_error" and isinstance(status, int) and 500 <= status < 600:
            retryable = True
        if retryable:
            spool_claim_collision(number, title, me, payload)
            return
        detail = e.extra.get("detail")
        reason = detail.get("reason") or detail.get("error") if isinstance(detail, dict) else None
        if not isinstance(reason, str) or not reason.strip():
            reason = e.kind
        reason = " ".join(reason.split())[:200]
        label = f"HTTP {status}: {reason}" if isinstance(status, int) else reason
        print(f"agm: collision check failed ({label}); not spooled", file=sys.stderr)
    except Exception as e:  # noqa: BLE001 — 附帶提示的任何失敗都不能讓認領看起來失敗
        print(f"agm: collision check failed ({type(e).__name__}); not spooled", file=sys.stderr)
        return

def cmd_issue(_client, _cfg: dict, args) -> object:
    number = args.number
    me = claiming_bot(args)
    issue = read_issue(args, number)
    issue["comments"] = all_comments(args, number)
    claim = current_claim(issue)
    labels = [l.get("name") for l in issue.get("labels") or []]
    if args.op == "claim" and (issue.get("state") or "").upper() != "OPEN":
        raise AgmError(
            "issue_closed",
            f"#{number} 已經是 {issue.get('state')}，不認領也不派工；要清掉留著的 label 用 `agm issue release {number}`",
            2,
            issue=number,
            state=issue.get("state"),
        )
    if args.op == "claim" and claim is None and CLAIM_LABEL in labels:
        raise AgmError(
            "issue_claim_state_unknown",
            f"#{number} 有 `{CLAIM_LABEL}` label 但沒有有效認領標記；為避免重複派工，先用 `agm issue release {number}` 清理不一致狀態",
            3,
            issue=number,
            claimed_by="unknown",
        )
    if claim is not None and CLAIM_LABEL not in labels and not claim.get("stale"):
        issue, claim = reconcile_claim_label(args, number)
        labels = [l.get("name") for l in issue.get("labels") or []]
    blocker = held_by_other(claim, me, None if args.op == "release" else args)
    if blocker is not None:
        raise claim_conflict(number, blocker)
    now =datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")

    if args.op == "touch":
        if claim is None or claim.get("stale"):
            raise AgmError("issue_not_claimed", f"#{number} 目前沒有有效認領，不能 touch；請先 claim", 2, issue=number)
        holder = claim.get("bot")
        holder_child = claim.get("child")
        if me not in (holder, holder_child):
            raise claim_conflict(number, claim)

        repo = getattr(args, "repo", None)
        touch_payload = {
            "bot": me,
            "claim_id": claim.get("claim_id"),
            "at": now,
        }
        body = f"{me} 仍在處理 #{number}。\n\n<!-- {PROGRESS_MARK} {json.dumps(touch_payload, ensure_ascii=False, sort_keys=True)} -->"

        existing_progress_comment = None
        for c in reversed(issue.get("comments") or []):
            if not isinstance(c, dict):
                continue
            assoc = c.get("author_association") or c.get("authorAssociation")
            if assoc is not None and str(assoc).upper() not in TRUSTED_ASSOCIATIONS:
                continue
            parsed = mark_of(c.get("body") or "")
            if parsed and parsed[0] == PROGRESS_MARK:
                p_kind, p_payload = parsed
                p_bot = p_payload.get("bot")
                p_target = p_payload.get("claim_id")
                if p_bot in (holder, holder_child) and (p_target is None or str(p_target) == claim.get("claim_id")):
                    existing_progress_comment = c
                    break

        if existing_progress_comment and existing_progress_comment.get("id"):
            comment_id = str(existing_progress_comment["id"])
            c_path = f"repos/{repo}/issues/comments/{comment_id}" if repo else f"repos/{{owner}}/{{repo}}/issues/comments/{comment_id}"
            gh(["api", "-X", "PATCH", c_path, "-f", f"body={body}"])
            patched = True
        else:
            gh(["issue", "comment", str(number), *repo_args(args), "--body", body])
            patched = False

        return {
            "issue": number,
            "title": issue.get("title"),
            "touched": True,
            "patched": patched,
            "bot": me,
            "last_activity": now,
        }

    if args.op == "release":
        out = {"issue": number, "title": issue.get("title"), "bot": me, "released": True, "was_claimed": claim is not None}
        if claim is None and CLAIM_LABEL not in labels:
            out["already"] = True
            return out
        release_payload = {"bot": me, "at": now}
        release_payload["claim_id"] = claim["claim_id"] if claim and claim.get("claim_id") else LABEL_ONLY_RELEASE
        body = f"{me} 交回 #{number}。\n\n<!-- {RELEASE_MARK} {json.dumps(release_payload, ensure_ascii=False, sort_keys=True)} -->"
        gh(["issue", "comment", str(number), *repo_args(args), "--body", body])
        if CLAIM_LABEL in labels:
            gh(["issue", "edit", str(number), *repo_args(args), "--remove-label", CLAIM_LABEL])
        reconcile_claim_label(args, number)
        return out

    if claim is not None and claim.get("bot") == me and not claim.get("stale"):
        if CLAIM_LABEL not in labels:
            ensure_claim_label(args)
            gh(["issue", "edit", str(number), *repo_args(args), "--add-label", CLAIM_LABEL])
        issue, claim = reconcile_claim_label(args, number)
        if claim is None:
            raise AgmError("issue_claim_lost", f"#{number} 的認領已在 label 同步前被交回；不可派工", 3, issue=number)
        blocker = held_by_other(claim, me, args)
        if blocker is not None:
            raise claim_conflict(number, blocker)
        if claim.get("bot") == me and not claim.get("stale"):
            return {"issue": number, "title": issue.get("title"), "claimed": True, "already": True, "bot": me, "claim": claim}
        labels = [l.get("name") for l in issue.get("labels") or []]

    payload = {"bot": me, "at": now, "claim_id": uuid.uuid4().hex}
    if issue.get("updatedAt"):
        payload["observed_updated_at"] = issue["updatedAt"]
    for key in ("child", "child_bot_id", "worktree", "branch"):
        val = getattr(args, key, None)
        if val:
            payload[key] = val
    bits = []
    if payload.get("child"):
        bits.append(f"child {payload['child']}")
    if payload.get("worktree"):
        bits.append(f"worktree `{payload['worktree']}`")
    if payload.get("branch"):
        bits.append(f"分支 `{payload['branch']}`")
    detail = "，".join(bits)
    took_over = bool(claim and claim.get("stale"))
    lead = f"派給 {me}" + (f"（{detail}）" if detail else "")
    if took_over:
        lead += f"。接手 {claim.get('bot')} 超過 24 小時沒動靜的認領（最後動靜 {claim['last_activity']}）"
    body = f"{lead}。\n\n<!-- {CLAIM_MARK} {json.dumps(payload, ensure_ascii=False, sort_keys=True)} -->"
    ensure_claim_label(args)
    gh(["issue", "comment", str(number), *repo_args(args), "--body", body])

    def release_unconfirmed_claim() -> None:
        released_at = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
        release_payload = {"bot": me, "at": released_at, "claim_id": payload["claim_id"]}
        release_body = (
            f"{me} 交回 #{number} 的未確認認領。\n\n"
            f"<!-- {RELEASE_MARK} {json.dumps(release_payload, ensure_ascii=False, sort_keys=True)} -->"
        )
        gh(["issue", "comment", str(number), *repo_args(args), "--body", release_body])

    confirmed = dict(issue)
    try:
        confirmed["comments"] = all_comments(args, number)
    except AgmError as read_error:
        try:
            release_unconfirmed_claim()
        except AgmError as release_error:
            raise AgmError(
                "gh_failed",
                f"認領留言可能已寫入，但回讀與交回標記都失敗；不可派工（回讀：{read_error.message}；交回：{release_error.message}）",
                1,
                issue=number,
            )
        raise AgmError(
            "gh_failed",
            f"認領留言可能已寫入，但回讀失敗；已追加交回標記，不可派工：{read_error.message}",
            1,
            issue=number,
        )

    winner = current_claim(confirmed)
    if winner is None or winner.get("claim_id") != payload["claim_id"]:
        try:
            release_unconfirmed_claim()
        except AgmError as release_error:
            raise AgmError(
                "gh_failed",
                f"回讀確認這次認領沒有勝出，但交回標記寫入失敗；不可派工：{release_error.message}",
                1,
                issue=number,
            )
        if winner is not None:
            raise claim_conflict(number, winner)
        raise AgmError("issue_claimed", f"#{number} 的認領回讀沒有確認本次標記；已交回，不可派工", 3, issue=number)

    gh(["issue", "edit", str(number), *repo_args(args), "--add-label", CLAIM_LABEL])
    issue, winner = reconcile_claim_label(args, number)
    if winner is None:
        raise AgmError("issue_claim_lost", f"#{number} 的認領在 label 同步前已被交回；不可派工", 3, issue=number)
    if winner.get("claim_id") != payload["claim_id"]:
        raise claim_conflict(number, winner)
    out = {"issue": number, "title": issue.get("title"), "claimed": True, "already": False, "bot": me, "label": CLAIM_LABEL}
    out.update({k: v for k, v in payload.items() if k in ("child", "worktree", "branch")})
    if took_over:
        out["took_over_stale_claim_from"] = claim.get("bot")
    collision_check_hint(args, number, issue.get("title") or f"#{number}", me, payload)
    return out

def cmd_bot(client: Client, cfg: dict, args) -> object:
    if args.op == "create":
        if not args.project or not args.name:
            raise AgmError("bad_args", "bot create 需要 --project 與 --name", 2)
        body: dict = {"name": args.name}
        for key, val in (("kind", args.kind), ("model", args.model), ("effort", args.effort), ("identity", args.identity)):
            if val:
                body[key] = val
        return client.post(f"/api/projects/{path_segment(args.project)}/bots", body)
    if not args.bot_id:
        raise AgmError("bad_args", f"bot {args.op} 需要 bot id", 2)
    if args.op == "set":
        body = {key: val for key, val in (("model", args.model), ("effort", args.effort), ("identity", args.identity), ("name", args.name)) if val is not None}
        if not body:
            raise AgmError("bad_args", "bot set 至少需要 --model、--effort、--name 或 --identity 其中一個", 2)
        return client.patch(f"/api/bots/{path_segment(args.bot_id)}", body)
    if args.op == "delete":
        query = "?confirm=supervisor" if getattr(args, "confirm_supervisor", False) else ""
        return client.delete(f"/api/bots/{path_segment(args.bot_id)}{query}")
    if args.op == "restore":
        return client.post(f"/api/bots/{path_segment(args.bot_id)}/restore", {})
    path = f"/api/bots/{path_segment(args.bot_id)}/{args.op}"
    if getattr(args, "session", None) and not getattr(args, "resume", None):
        raise AgmError("bad_args", "--session 要跟 --resume native 或 --resume fork 一起用", 2)
    if getattr(args, "session", None) and args.resume not in ("native", "fork"):
        raise AgmError("bad_args", "--session 要跟 --resume native 或 --resume fork 一起用", 2)
    if getattr(args, "resume", None) == "fork" and not getattr(args, "session", None):
        raise AgmError("bad_args", "--resume fork 必須帶 --session", 2)
    if getattr(args, "resume", None):
        if args.op not in ("start", "restart"):
            raise AgmError("bad_args", "--resume 只有 bot start / restart 收", 2)
        query = {"resume": args.resume}
        if getattr(args, "session", None):
            query["session"] = args.session
        path = f"{path}?{urllib.parse.urlencode(query)}"
    return client.post(path, {})

class JsonArgumentParser(argparse.ArgumentParser):
    """用法錯誤也走 `AgmError`（JSON 進 stderr、exit 2）：呼叫端（排程腳本、模型）只解析一種錯誤形狀。`--help` 不受影響。"""

    def error(self, message: str):  # noqa: D102
        raise AgmError("bad_args", message, 2, usage=self.format_usage().strip())

def _timeout_arg(raw: str) -> float:
    try:
        v = float(raw)
    except ValueError:
        raise argparse.ArgumentTypeError(f"--timeout 要是數字，收到 {raw!r}")
    if not (v == v) or v <= 0 or v > MAX_TIMEOUT:
        raise argparse.ArgumentTypeError(f"--timeout 要在 0 到 {MAX_TIMEOUT:g} 秒之間，收到 {raw!r}")
    return v

def _limit_arg(raw: str) -> int:
    try:
        v = int(raw)
    except ValueError:
        raise argparse.ArgumentTypeError(f"要是整數，收到 {raw!r}")
    if not 1 <= v <= MAX_LIMIT:
        raise argparse.ArgumentTypeError(f"要在 1 到 {MAX_LIMIT} 之間，收到 {v}")
    return v

def _positive_int_arg(raw: str) -> int:
    """正整數（秒數、issue 編號）：0、負數和非整數在解析階段拒絕。"""
    try:
        value = int(raw)
    except ValueError:
        raise argparse.ArgumentTypeError(f"要是正整數，收到 {raw!r}")
    if value < 1:
        raise argparse.ArgumentTypeError(f"要是正整數（≥ 1），收到 {value}")
    return value

MAX_TTL_SECS = 3600

def _ttl_arg(raw: str) -> int:
    value = _positive_int_arg(raw)
    if value > MAX_TTL_SECS:
        raise argparse.ArgumentTypeError(f"要在 1 到 {MAX_TTL_SECS} 之間，收到 {value}")
    return value

def _id_arg(raw: str) -> str:
    v = raw.strip()
    if not v or len(v) > MAX_ID_CHARS or any(ord(c) < 32 or ord(c) == 127 for c in v):
        raise argparse.ArgumentTypeError(f"id 不能是空的、含控制字元或超過 {MAX_ID_CHARS} 字")
    return v

def build_parser() -> argparse.ArgumentParser:
    p = JsonArgumentParser(
        prog="agm",
        description="AGM 總管工具。輸出 JSON；失敗時 JSON 進 stderr 並以非 0 離開。token 由本機執行期取得，不會出現在輸出或參數裡。",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "常見流程：\n"
            "  agm state                              看目前有哪些專案／bot 與狀態\n"
            "  agm search '登入 遠端身份' --limit 20   找相關歷史（帶 message/turn ID）\n"
            "  agm messages <bot-id> --limit 50       讀某個 bot 的原始對話\n"
            "  agm assign --bot <id> --text '…' --request-id agm-2026-09-09-001\n"
            "  agm assignments --open                 對帳未結案交辦（含等驗收的）\n"
            "  agm review <id> --decision accept --reason '…' --evidence '…'\n"
            "  agm incidents                          看系統層級故障（host／bot／卡住的交辦）\n"
            "  agm approval request --requester … --purpose rebuild --scope … --commit <sha>\n"
            "  agm lease safety / agm lease acquire rebuild --approval <id> --commit <sha>\n"
            "  agm inbox / agm ack <event-id>         處理通知（預設只列未 ack、最舊在前）\n"
            "  agm handoff / agm handoff --summary '…' 讀寫管理摘要\n"
            "  agm issue claim 425 --child i425 --worktree … --branch …   派工前先認領（exit 3＝別人在做）\n"
            "\n"
            "注意：assign 逾時代表送達未知，**不要**換新的 --request-id 重送，先用 assignments 對帳。\n"
            "注意：回合結束不等於工作完成。交辦會停在 awaiting_review，要 `agm review` 才會結案。"
        ),
    )
    p.add_argument("--runtime-dir", help="覆寫設定目錄（預設讀 AGM_RUNTIME_DIR，再退回腳本上層目錄）")
    p.add_argument("--timeout", type=_timeout_arg, default=None, help=f"HTTP 逾時秒數（預設 {DEFAULT_TIMEOUT:g}）")
    p.add_argument("--compact", action="store_true", help="輸出成單行 JSON")
    sub = p.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("state", help="精簡的 bot / project 狀態（不含 env 等敏感欄位）")
    s.set_defaults(func=cmd_state)

    s = sub.add_parser("supervisor", help="總管自己的設定與狀態（GET /api/supervisor）")
    s.set_defaults(func=cmd_supervisor)

    s = sub.add_parser("health", help="讀取 daemon、AGM、bot 與 quota 的健康摘要")
    s.set_defaults(func=cmd_health)

    for act in ("setup", "start", "stop", "fallback"):
        s = sub.add_parser(f"supervisor-{act}", help=f"總管 {act}")
        s.set_defaults(func=cmd_supervisor_action, action=act)

    s = sub.add_parser("search", help="搜尋歷史訊息，回可追溯的證據")
    s.add_argument("query")
    s.add_argument("--bot", help="限定 bot id")
    s.add_argument("--project", help="限定 project id")
    s.add_argument("--before", help="分頁游標")
    s.add_argument("--limit", type=_limit_arg, default=20)
    s.set_defaults(func=cmd_search)

    s = sub.add_parser("messages", help="讀某個 bot 的對話（分頁）")
    s.add_argument("bot_id", type=_id_arg)
    s.add_argument("--limit", type=_limit_arg, default=50)
    s.add_argument("--before", help="分頁游標")
    s.set_defaults(func=cmd_messages)

    s = sub.add_parser("assign", help="建立持久交辦並送出（逾時不自動重試）")
    s.add_argument("--bot", required=True, help="目標 bot id（填另一個 AGM 角色的 bot 就是交接：排進它的佇列，回佇列收據而不是交辦）")
    s.add_argument("--text", help="交辦內容")
    s.add_argument("--text-file", help="從檔案讀交辦內容")
    s.add_argument("--request-id", required=True, help="穩定的 client_request_id；重試沿用同一個")
    s.add_argument("--source-turn-id", help="使用者原始訊息的 turn id")
    s.add_argument(
        "--owns",
        action="append",
        metavar="PATH",
        help="這筆交辦負責的檔案／模組（可重複）。重疊時 daemon 會回報 ownership_conflicts",
    )
    s.add_argument(
        "--notice",
        action="store_true",
        help="通知，不是交辦：送到、回合結束就自動結案，不進 awaiting_review、不會被當成卡住的工作",
    )
    s.add_argument(
        "--ack",
        action="store_true",
        help="只用在交接給另一個 AGM 角色：這句是純告知（「收到」），不叫醒對方。沒帶＝新的事，會叫醒",
    )
    s.add_argument(
        "--reply-to",
        dest="reply_to",
        metavar="EVENT_ID",
        help="只用在交接給另一個 AGM 角色：這句回的是哪一則 inbox 事件，不叫醒對方。要對方處理的事不要帶",
    )
    s.add_argument(
        "--review-by",
        dest="review_by",
        choices=["patrol", "responder"],
        help="回報給哪個 AGM 角色驗收（預設：派工的角色自己；巡檢的例行維運寫 patrol）",
    )
    s.set_defaults(func=cmd_assign)

    s = sub.add_parser("assignments", help="列出交辦，或用 --id 查一筆")
    s.add_argument("--id", help="只查這一筆（吃 assignment id 或 client_request_id）")
    s.add_argument("--status", help="只列這個狀態")
    s.add_argument("--open", action="store_true", help="只列未結案（含 awaiting_review、blocked）")
    s.add_argument("--awaiting-review", action="store_true", dest="awaiting_review", help="只列等你驗收的")
    s.add_argument("--all", action="store_true", help="翻完所有分頁（預設只看最新一頁；有過濾條件時本來就會翻完）")
    s.set_defaults(func=cmd_assignments)

    s = sub.add_parser(
        "review",
        help="驗收／阻塞／續作／取消一筆交辦（唯一能結案的路徑）",
        description="回合跑完只會進 awaiting_review。要結案就在這裡說清楚是誰、依據什麼決定的。",
    )
    s.add_argument("assignment_id", type=_id_arg)
    s.add_argument(
        "--decision",
        required=True,
        choices=["accept", "block", "followup", "fail", "cancel"],
        help="accept=驗收結案／block=還在等，保持未結案／followup=派續作／fail=判定失敗／cancel=取消",
    )
    s.add_argument("--reason", help="為什麼這樣決定")
    s.add_argument("--evidence", help="依據（turn id、commit、測試數字）")
    s.add_argument("--actor", default="AGM", help="決定的人／bot（預設 AGM）")
    s.add_argument("--source", default="cli", help="來源（預設 cli）")
    s.add_argument("--followup-text", dest="followup_text", help="followup：續作要做什麼")
    s.add_argument("--followup-file", dest="followup_file", help="followup：從檔案讀續作內容")
    s.add_argument(
        "--followup-request-id",
        dest="followup_request_id",
        help="followup：續作的穩定 client_request_id；重試沿用同一個",
    )
    s.add_argument("--followup-bot", dest="followup_bot", help="followup：改派給別的 bot（預設同一顆）")
    s.set_defaults(func=cmd_review)

    s = sub.add_parser("approval", help="重建／重啟核准：request / decide / list")
    s.add_argument("op", choices=["request", "decide", "list"])
    s.add_argument("approval_id", nargs="?", help="decide 的目標")
    s.add_argument("--id", help="list：只查這一筆（清單只回最新 100 筆，舊的要用這個查）")
    s.add_argument("--requester", help="request：申請者（已驗證的 bot id；User principal 使用 principal:user；名稱不是身份）")
    s.add_argument("--purpose", choices=["rebuild", "restart"], help="request：要做什麼")
    s.add_argument("--scope", help="request：會動到什麼")
    s.add_argument("--commit", help="request：針對哪個 commit（之後 acquire 要對得上）")
    s.add_argument(
        "--request-id",
        dest="request_id",
        help="request：穩定 id，重送同一個回原本那一筆（回應 created=false）；換了內容回 409。不確定送出去沒有時用它重送，不要換新 id",
    )
    s.add_argument("--expires-in", type=_positive_int_arg, dest="expires_in", metavar="SECS", help="多久之後失效")
    s.add_argument(
        "--supersedes",
        metavar="APPROVAL_ID",
        help="request：取代自己先前同一種用途的申請（換 commit 重申請）；舊的標 superseded，升級的等待時間接過來",
    )
    s.add_argument("--decision", choices=["approve", "deny", "revoke"], help="decide：核准／駁回／撤銷")
    s.add_argument("--reason", help="decide：理由")
    s.add_argument("--actor", default="AGM", help="decide：決定者（預設 AGM）")
    s.set_defaults(func=cmd_approval)

    s = sub.add_parser(
        "lease",
        help="執行租約：safety / acquire / renew / release / status",
        description="等窗口用 safety（唯讀，只說現在）；真的要動手用 acquire（同一個鎖裡重驗並拿走窗口）。",
    )
    s.add_argument("op", choices=["safety", "acquire", "renew", "release", "status"])
    s.add_argument("resource", nargs="?", choices=["rebuild", "restart"], help="acquire/renew/release 的目標")
    s.add_argument(
        "--owner",
        help="acquire/renew/release：誰持有（預設 $AM_BOT_ID，外部人類為 principal:user）；safety：以這個人的身份問，他自己握的租約不算擋（不帶＝每一把都算擋）",
    )
    s.add_argument("--approval", help="acquire：核准 id；safety：用這筆核准的等待時間判斷要不要縮小封鎖面")
    s.add_argument("--commit", help="acquire：要處理的 commit，必須符合核准")
    s.add_argument("--ttl", type=_ttl_arg, metavar="SECS", help="租約長度（預設 900，上限 3600）")
    s.add_argument("--fence", type=int, help="renew/release：acquire 回傳的 fence")
    s.add_argument(
        "--lease-token",
        dest="lease_token",
        metavar="TOKEN|-",
        help="renew/release：acquire 回應裡的 lease_token（只出現那一次，不會在 lease status 裡）。"
        "**argv 同一台機器上誰都看得到（ps）**——請改用 --lease-token-file，或給 `-` 從 stdin 讀（issue #477）",
    )
    s.add_argument(
        "--lease-token-file",
        dest="lease_token_file",
        metavar="PATH",
        help="renew/release：從檔案讀 lease_token（權限要 600，group／other 讀得到就拒絕）。優先用這個，不要把 token 放進 argv",
    )
    s.add_argument("--force", action="store_true", help="release：強制接管（持有者已經不在了），要附 --reason，會留稽核紀錄")
    s.add_argument(
        "--request-id",
        dest="request_id",
        metavar="ID",
        help="acquire：冪等鍵（只有你知道的祕密）。回應逾時、lease_token 沒收到時，帶**同一個**重送就拿回同一張租約；"
        "不給就自己產一個並在逾時時印出來",
    )
    s.add_argument("--allow-busy", action="store_true", dest="allow_busy", help="acquire：跳過「沒人在跑」的檢查")
    s.add_argument("--exclude-bot", action="append", dest="exclude_bot", metavar="BOT_ID", help="idle 檢查要忽略的 bot")
    s.add_argument("--reason", help="release --force：為什麼要強制接管")
    s.set_defaults(func=cmd_lease)

    s = sub.add_parser("persona", help="人設：show / set / adopt-embedded")
    s.add_argument("op", choices=["show", "set", "adopt-embedded"])
    s.add_argument("--full", action="store_true", help="show：連全文一起印（預設只印版本與 hash）")
    s.add_argument("--text", help="set：新的人設全文")
    s.add_argument("--file", help="set：從檔案讀")
    s.add_argument("--expected-version", type=int, dest="expected_version", help="set：樂觀鎖，對不上就拒絕")
    s.add_argument("--reason", help="adopt-embedded：為什麼要換成內嵌版")
    s.add_argument("--actor", default="AGM", help="adopt-embedded：誰決定的（預設 AGM）")
    s.add_argument("--role", choices=["patrol", "responder"], default="patrol", help="哪個角色的人設（預設巡檢）")
    s.set_defaults(func=cmd_persona)

    s = sub.add_parser("build-inputs", help="會影響 binary 的路徑（含 include_str! 的檔）")
    s.set_defaults(func=cmd_build_inputs)

    s = sub.add_parser("incidents", help="系統層級故障；預設只列未恢復的")
    s.add_argument("--all", action="store_true", help="含已恢復的")
    s.set_defaults(func=cmd_incidents)

    s = sub.add_parser("inbox", help="還沒 ack 的通知（最舊在前）；--all 才含已處理的")
    s.add_argument("--all", action="store_true", help="含 state=handled 的事件（最新在前）")
    s.add_argument("--role", choices=["patrol", "responder", "mine"], help="只看某個 AGM 角色收的（mine＝runtime.json 的角色）")
    s.add_argument("--limit", type=_limit_arg, default=200, help="最多幾筆（預設 200，上限 1000）")
    s.set_defaults(func=cmd_inbox)

    s = sub.add_parser("whoami", help="這支 CLI 代表哪個 AGM 角色（patrol／responder）與 bot")
    s.set_defaults(func=cmd_whoami)

    s = sub.add_parser("responder", help="AGM 協調者：show / setup / start / stop（setup 不會啟動）")
    s.add_argument("op", choices=["show", "setup", "start", "stop"])
    s.add_argument("--identity", help="setup：帳號（預設沿用，第一次 cc0）")
    s.add_argument("--model", help="setup：模型（預設沿用，第一次 opus）")
    s.add_argument("--effort", help="setup：強度（預設沿用，第一次 high）")
    s.set_defaults(func=cmd_responder)

    s = sub.add_parser("ack", help="確認已處理一則通知（帶角色 token 時只能 ack 自己角色收的）")
    s.add_argument("event_id", type=_id_arg)
    s.set_defaults(func=cmd_ack)

    s = sub.add_parser("ops-alert", help="排程腳本卡住時喊人：推一則 durable 通知給巡檢（同 source+reason 每小時一則）")
    s.add_argument("--source", required=True, help="哪一支腳本（例如 daemon-update-kick）")
    s.add_argument("--reason", required=True, help="卡在什麼上（例如 stale_lock、approval_missing）")
    s.add_argument("--detail", help="人看得懂的細節：要怎麼處理")
    s.set_defaults(func=cmd_ops_alert)

    s = sub.add_parser("ops-sync", help="已安裝的 ops 腳本跟 repo 比對（唯讀；有落差 exit 1）")
    s.add_argument("--check", action="store_true", required=True, help="只比對不安裝（目前唯一的模式）")
    s.add_argument("--repo", help=f"repo 位置（預設 AGM_REPO，再退回 {DEFAULT_REPO}）")
    s.add_argument("--ref", default="origin/main", help="跟哪一版比（預設 origin/main；要最新先 git fetch）")
    s.add_argument("--alert", action="store_true", help="有落差時推一則 ops_alert 給巡檢（同 source+reason 每小時一則）")
    s.add_argument("--refresh-cli", action="store_true",
                   help="安裝的 bin/agm 不是 binary 內嵌的那份時，順手叫 daemon 就地換掉（不必等開機；binary 自己落後仍要重建＋重啟）")
    s.set_defaults(func=cmd_ops_sync)

    s = sub.add_parser("release-triage", help="上游新版分診：submit（交回 verdict）/ show / dispatched / publish / auto（issue #204、#1919）")
    s.add_argument("op", choices=["submit", "show", "dispatched", "publish", "auto"])
    s.add_argument("--file", help="submit：verdicts.json（{kind,version,dispatch_gen,verdicts,issues}，一次一版；dispatch_gen 抄 show 那一列）")
    s.add_argument("--kind", choices=["claude", "codex", "agy", "grok"])
    s.add_argument("--version", action="append", help="show／publish：某一版；dispatched：可重複給多版")
    s.add_argument("--bot", help="dispatched：這批交辦收件的 bot id（#801）。之後只有它或它底下的 child 能交這些版的 verdict")
    s.add_argument("--check", action="store_true", help="auto：只問 daemon 的 Claude API 分診（[claude_api.uses.release_triage]）有沒有開，不呼叫 API")
    s.add_argument("--binary-diff-file", help="auto：claude_binary_diff.py 的報告檔，附進 prompt 並併進通知")
    s.add_argument("--dry-run", action="store_true", help="publish：乾跑。檢查 gh auth／repo 權限／標籤齊不齊，印出會開哪幾張（含標題與內文），一張都不開、帳本不動")
    s.set_defaults(func=cmd_release_triage)

    s = sub.add_parser("handoff", help="讀管理摘要；帶 --summary/--summary-file 就是寫入")
    s.add_argument("--summary")
    s.add_argument("--summary-file")
    s.set_defaults(func=cmd_handoff)

    s = sub.add_parser("peers", help="列出同專案 bot 與觀察狀態")
    s.add_argument("--for", dest="for_track", help="專家模式：依 track 評估候選執行者（回傳 candidates）")
    s.set_defaults(func=cmd_peers)
    s = sub.add_parser("lineage", help="codex session 血統（#1943）：按模組線記檢查點；list 同專案都可讀，其餘限管理者／AGM（母子模式為頂層 bot）")
    lineage = s.add_subparsers(dest="lineage_command", required=True)

    def _lineage_fields(p) -> None:
        p.add_argument("line", help="模組線名（例 lifecycle、web、quota；[A-Za-z0-9._-]）")
        p.add_argument("session_id", help="CLI 的 session id（例 codex 的 thread id）")
        p.add_argument("--parent", help="父 session id（advance 新登記時沒給＝前一顆檢查點）")
        p.add_argument("--head", help="當時的 HEAD commit sha（7–64 位 hex）")
        p.add_argument("--summary", help="一行摘要（最多 500 字）")
        p.add_argument("--role", choices=["worker", "thinker"], help="新登記時預設 worker")
        p.add_argument("--kind", choices=["codex"], help="只收 codex（預設）")
        p.add_argument("--context-tokens", dest="context_tokens", type=int, help="目前 context token 數")
        p.add_argument("--last-api-at", dest="last_api_at", help="最後一次 API 回合的時間（RFC 3339）")

    s = lineage.add_parser("list", help="列出本專案的血統列（每條線檢查點排最前）")
    s.add_argument("--line", help="只看這條線")
    s.add_argument("--state", choices=["checkpoint", "active", "retired"], help="只看這個狀態")
    s.set_defaults(func=cmd_lineage_list)
    s = lineage.add_parser("add", help="登記一顆進行中（active）的 session")
    _lineage_fields(s)
    s.set_defaults(func=cmd_lineage_add)
    s = lineage.add_parser("advance", help="把 session 設為該線檢查點，原檢查點改 retired")
    _lineage_fields(s)
    s.set_defaults(func=cmd_lineage_advance)
    s = lineage.add_parser("retire", help="退役一顆 session（做壞／放棄，或重種後的舊檢查點）")
    s.add_argument("session_id")
    s.set_defaults(func=cmd_lineage_retire)
    s = lineage.add_parser("pick", help="依 lines.toml 把路徑對到線；全同線回 line，跨線或 unmatched 回 null")
    s.add_argument("paths", nargs="+", help="檔案路徑清單")
    s.add_argument("--config", "--lines-file", dest="lines_file", help="指定 lines.toml 路徑（預設 docs/notes/lines.toml）")
    s.add_argument("--repo", help="指定 git repo 路徑")
    s.set_defaults(func=cmd_lineage_pick, needs_client=False)
    s = lineage.add_parser("since", help="取該線檢查點 head_sha，計算自檢查點以來的 git diff --stat 與 prompt_prefix")
    s.add_argument("line", help="模組線名（例 web、lifecycle、quota、ops）")
    s.add_argument("--config", "--lines-file", dest="lines_file", help="指定 lines.toml 路徑（預設 docs/notes/lines.toml）")
    s.add_argument("--repo", help="指定 git repo 路徑")
    s.set_defaults(func=cmd_lineage_since)
    s = lineage.add_parser("dispatch", help="血統派工：pick → claim →（resume 備份）→ 啟動 worker → 前綴 since → 派 prompt")
    s.add_argument("--paths", nargs="+", help="要改動的檔案路徑清單（依 lines.toml 挑線）")
    s.add_argument("--line", help="直接指定模組線名（或作為 --paths 跨線時的基準線）")
    s.add_argument("--prompt-file", dest="prompt_file", required=True, help="派工 prompt 檔案路徑")
    s.add_argument("--fork", action="store_true", help="強制走持久 fork 模式")
    s.add_argument("--bot", help="專家模式：目標執行者 bot id")
    s.add_argument("--child", help="母子模式：要開啟的 herdr child 名稱")
    s.add_argument("--pane", help="母子模式：既有 pane id（省略則從 $HERDR_PANE_ID split）")
    s.add_argument("--identity", help="母子模式：herdr agent start 的 --identity")
    s.add_argument("--request-id", dest="request_id", help="送 prompt 用的 client_request_id")
    s.add_argument("--config", "--lines-file", dest="lines_file", help="指定 lines.toml 路徑（預設 docs/notes/lines.toml）")
    s.add_argument("--repo", help="指定 git repo 路徑")
    s.set_defaults(func=cmd_lineage_dispatch)
    s = lineage.add_parser("finish", help="結束血統認領：--merged 帶 claim_id 前移，或 --abandon 放棄並回傳備份 session")
    s.add_argument("claim_id", help="dispatch 回傳的 claim_id")
    fin_mode = s.add_mutually_exclusive_group(required=True)
    fin_mode.add_argument("--merged", action="store_true", help="工作已合流，帶 claim_id 前移檢查點")
    fin_mode.add_argument("--abandon", action="store_true", help="放棄認領，並印出要接手的備份 session")
    s.add_argument("--head", help="--merged 時記錄的 HEAD commit sha（7–64 位 hex）")
    s.add_argument("--summary", help="--merged 時記錄的一行摘要（最多 500 字）")
    s.add_argument("--line", help="選填：覆寫線名驗證")
    s.add_argument("--session", help="選填：覆寫目標 session_id")
    s.set_defaults(func=cmd_lineage_finish)
    s = sub.add_parser("role", help="專家模式管理者：設同專案頂層 bot 的角色（executor／advisor／specialist／none）")
    s.add_argument("bot_id")
    s.add_argument("role", choices=["executor", "advisor", "specialist", "none"])
    s.add_argument("--track", help="role specialist 時必填：專家負責的軌道")
    s.set_defaults(func=cmd_role)
    s = sub.add_parser("hire", help="專家模式管理者：增聘同專案成員，自行決定 kind／model／effort（不自動啟動，之後 agm bot start）")
    s.add_argument("name")
    s.add_argument("--kind", required=True, choices=["claude", "codex", "grok", "agy"], help="新成員使用的 CLI kind（必填）")
    s.add_argument("--model", help="新成員使用的模型")
    s.add_argument("--effort", help="新成員使用的推理強度")
    s.add_argument("--identity")
    s.add_argument("--role", choices=["executor", "specialist"], help="預設 executor；specialist 必須同時給 --track")
    s.add_argument("--track", help="專家負責的軌道（--role specialist 必填）")
    s.add_argument("--fork-from", dest="fork_from", metavar="BOT", help="從同專案同主機的 codex bot 分岔（新成員也必須是 codex；第一次啟動用 codex fork，之後走自己的 session）")
    s.set_defaults(func=cmd_hire)
    s = sub.add_parser("project", help="專案設定（set-path：檔案搬好後改專案路徑）")
    psub = s.add_subparsers(dest="op", required=True)
    ps = psub.add_parser("set-path", help="改專案路徑：先自己搬好檔案、停掉專案內所有 bot，再呼叫；不搬檔、不重啟")
    ps.add_argument("project_id")
    ps.add_argument("path", help="新的絕對路徑（必須是該主機上已存在的目錄）")
    ps.set_defaults(func=cmd_project_set_path)
    s = sub.add_parser("member", help="專家模式：把留下的 child 原地轉成成員")
    s.add_argument("bot_id")
    s.add_argument("--role", choices=["executor", "advisor"])
    s.add_argument("--effort")
    s.add_argument("--session-id", dest="session_id")
    s.set_defaults(func=cmd_member)
    s = sub.add_parser("requests", help="查詢請求結果通知")
    s.add_argument("--to", dest="to_bot")
    s.add_argument("--from", dest="from_bot")
    s.add_argument("--open", action="store_true")
    s.set_defaults(func=cmd_requests)

    s = sub.add_parser("models", help="查可用模型；--refresh 由 AGM／User 更新指定主機與身份快取")
    s.add_argument("--kind", required=True, choices=["claude", "codex", "grok", "agy"])
    s.add_argument("--host", default="local", help="執行 CLI 的主機名；不是 daemon 的位址")
    s.add_argument("--identity", help="身份名；省略＝該主機預設身份")
    s.add_argument("--refresh", action="store_true", help="強制更新快取；一般 Bot 無此權限，403 不改用其他憑證")
    s.set_defaults(func=cmd_models)

    s = sub.add_parser("quota", help="各身份的額度狀態；--probe 強制重跑 /usage 並覆寫 cache")
    s.add_argument("--probe", action="store_true", help="強制重跑該帳號的 /usage，結果蓋掉 statusLine 的值")
    s.add_argument("--kind", default="claude", choices=["claude"], help="目前只有 claude 有強制探測")
    s.add_argument("--account", help="身份名（cc0、cc1…）；省略＝預設帳號")
    s.add_argument("--host", help="主機名；省略＝本機")
    s.set_defaults(func=cmd_quota)

    s = sub.add_parser(
        "issue",
        help="issue 認領：claim / touch / release（#425、#1980；只用 gh，不連 daemon）",
        description=(
            "派任何 issue 給 child 之前先 claim，claim 失敗（exit 3）就不要派——"
            "同一張票被兩顆父 bot 各派一顆 child 是 2026-09-23 發生三次的重工來源。"
            "進行中每 24 小時 touch 一次維持活躍（#1980），收尾時 release。"
        ),
    )
    s.add_argument("op", choices=["claim", "touch", "release"])
    s.add_argument("number", type=_positive_int_arg, help="issue 編號")
    s.add_argument("--repo", help="owner/name；省略就讓 gh 從 cwd 的 remote 自己推")
    s.add_argument("--bot", help="認領人；省略取 AM_AGENT_NAME、再退 AM_BOT_ID")
    s.add_argument("--child", help="claim：要派出去的 child agent 名（只做顯示，不拿來認 bot）")
    s.add_argument("--child-bot-id", dest="child_bot_id", help="claim：child 的 bot ID（已存在的 bot 才有）；撞題檢查靠它排除 child 自己的工作")
    s.add_argument("--worktree", help="claim：child 會用的 worktree")
    s.add_argument("--branch", help="claim：child 會用的分支")
    s.set_defaults(func=cmd_issue, needs_client=False)

    s = sub.add_parser("bot", help="管理 bot：start / stop / restart / set / create / delete / restore")
    s.add_argument("op", choices=["start", "stop", "restart", "set", "create", "delete", "restore"])
    s.add_argument("bot_id", nargs="?", type=_id_arg, help="start/stop/restart/delete/restore 的目標（delete＝停 pane 並軟刪，子 agent 一起收；restore＝還原被軟刪的子 agent，不開 run）")
    s.add_argument("--project", help="create：所屬 project id")
    s.add_argument("--name", help="create：bot 名稱；set：改名（管理者可改自己專案的成員）")
    s.add_argument("--kind", help="create：claude / codex / grok")
    s.add_argument("--model")
    s.add_argument("--effort")
    s.add_argument("--identity")
    s.add_argument("--resume", choices=["native", "fresh", "fork"], help="start/restart：預設就會接回記錄的對話（接不回才開新的）；native＝一定要接回（接不回是 409 resumed:false）；fresh＝明確開新對話；fork＝從 --session 指定之 codex 對話持久分岔")
    s.add_argument("--session", help="跟 --resume native 或 --resume fork 一起：不看 DB，指名接回或分岔這一段 session id")
    s.add_argument("--confirm-supervisor", dest="confirm_supervisor", action="store_true",
                   help="delete：確定要刪 AGM 的 bot（總管專案裡的、總管的 child）；沒帶會 409 supervisor_owned")
    s.set_defaults(func=cmd_bot)

    s = sub.add_parser("consult", help="唯讀諮詢 thinker 血統檢查點（#1943 C：ephemeral fork，不登記不前移）")
    s.add_argument("question", nargs="?", help="要問 thinker 的問題（與 --prompt-file 擇一）")
    s.add_argument("--prompt-file", dest="prompt_file", help="問題檔案路徑（與位置參數擇一）")
    s.add_argument("--line", default="lead", help="thinker 血統線名（預設 lead）")
    s.add_argument("--model", help="覆寫模型（預設從 /api/models 取最新 sol）")
    s.add_argument("--effort", default="xhigh", help="推理強度（預設 xhigh）")
    s.add_argument("--host", help="查詢 /api/models 用的主機名（預設 local）")
    s.add_argument("--exec-timeout", dest="exec_timeout", type=_timeout_arg, default=900.0, help="codex exec 逾時秒數（預設 900）")
    s.add_argument("--config", "--lines-file", dest="lines_file", help="指定 lines.toml 路徑（預設 docs/notes/lines.toml）")
    s.add_argument("--repo", help="指定 git repo 路徑")
    s.set_defaults(func=cmd_consult)

    return p

def _is_slow_command(args) -> bool:
    op = getattr(args, "op", None)
    return args.cmd == "release-triage" and op in ("publish", "auto")

def main(argv: list[str] | None = None) -> int:
    raw_argv = list(sys.argv[1:] if argv is None else argv)
    try:
        args = build_parser().parse_args(argv)
    except AgmError as e:
        json.dump(e.to_json(), sys.stderr, ensure_ascii=False, indent=None if "--compact" in raw_argv else 2)
        sys.stderr.write("\n")
        return e.exit_code
    indent = None if args.compact else 2
    try:
        cfg: dict = {}
        client = None
        if getattr(args, "needs_client", True):
            timeout = args.timeout if args.timeout is not None else DEFAULT_TIMEOUT
            if (args.cmd in ("peers", "requests", "role", "hire", "member", "lineage", "consult", "ops-alert", "project") or (args.cmd == "bot" and getattr(args, "op", "") in ("start", "restart", "stop", "restore", "delete", "set") and os.environ.get("AM_DAEMON_URL") and (os.environ.get("AM_PROJECT_MODE") == "expert" or not os.environ.get("AM_PORT")))) and os.environ.get("AM_BOT_ID"):
                headers = bot_auth_headers({})
                if not (headers.get("X-AM-Bot-Token") or (headers.get("X-AM-Child-Spawn-Id") and headers.get("X-AM-Child-Token"))):
                    raise AgmError("no_bot_token", "缺少 bot 憑證", 2)
                url = os.environ.get("AM_DAEMON_URL")
                if not url:
                    port = os.environ.get("AM_PORT")
                    if not port or not port.isdigit():
                        raise AgmError("no_daemon_url", "缺少 AM_DAEMON_URL；遠端主機需設定 daemon_url", 2)
                    url = f"http://127.0.0.1:{port}"
                client = Client(url, timeout, headers, bot_remote=True)
            else:
                cfg = load_runtime(args.runtime_dir)
                client = Client(daemon_url(cfg), timeout, bot_auth_headers(cfg))
            if args.timeout is None and _is_slow_command(args):
                client.timeout = AUTO_TIMEOUT if getattr(args, "op", None) == "auto" else SLOW_TIMEOUT
        out = args.func(client, cfg, args)
    except AgmError as e:
        json.dump(e.to_json(), sys.stderr, ensure_ascii=False, indent=indent)
        sys.stderr.write("\n")
        return e.exit_code
    except KeyboardInterrupt:
        return 130
    except Exception as e:  # noqa: BLE001 — 兜底：輸出形狀要穩定，呼叫端會解析 stderr 的 JSON
        json.dump({"error": "internal", "message": f"{type(e).__name__}: {e}"}, sys.stderr, ensure_ascii=False, indent=indent)
        sys.stderr.write("\n")
        return 1
    code = 0
    if isinstance(out, Exit):
        out, code = out.out, out.code
    json.dump(out, sys.stdout, ensure_ascii=False, indent=indent, default=str)
    sys.stdout.write("\n")
    return code

if __name__ == "__main__":
    sys.exit(main())
