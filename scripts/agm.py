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
import http.client
import json
import os
import pathlib
import plistlib
import re
import socket
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

DEFAULT_TIMEOUT = 30.0
# 跟 daemon 的 MAX_PROVABLE_CHARS 同一個數字：超過的內容派送時一定送不出去，daemon 入口回 422 text_too_long（#335）。
MAX_TEXT_CHARS = 200_000
LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost", "0:0:0:0:0:0:0:1"}
DEFAULT_RUNTIME_DIR = "~/.config/agents-manager/supervisor/AGM"
# issue 認領（#425）。`gh` 是唯一的對外通道；label 與這兩個標記就是整個協定。
CLAIM_LABEL = "wip"
CLAIM_MARK = "agm:issue-claim"
RELEASE_MARK = "agm:issue-release"
# 認領多久沒有任何動靜就算放掉了（票上的裁示）。
CLAIM_STALE_SECS = 24 * 3600
GH_TIMEOUT = 60.0


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


# ---------------------------------------------------------------- runtime 設定


def runtime_dir(explicit: str | None = None) -> Path:
    """設定目錄。env 優先給測試 override 用，不必動部署好的檔案。"""
    env = os.environ.get("AGM_RUNTIME_DIR")
    if env:
        return Path(env).expanduser()
    if explicit:
        return Path(explicit).expanduser()
    # 部署後這支腳本住在 <cwd>/bin/agm，設定檔在它的上一層。
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
        # URL 裡的帳密會被 urllib 塞進 Authorization header；不接受這種夾帶。
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
    """角色身分的證明：pane 環境裡的 `AM_BOT_ID` + `AM_HOOK_TOKEN`（daemon 注入的那顆 bot 自己的
    hook token）。daemon 只認這兩個對得上的，模型打出來的角色名字不算數。

    只在 `AM_BOT_ID` 就是這個 runtime 的 `self_bot_id` 時才帶：在別的 pane 裡借用這支 CLI，
    不會把那顆 bot 的 token 送出去冒充角色。"""
    bot, tok = os.environ.get("AM_BOT_ID", ""), os.environ.get("AM_HOOK_TOKEN", "")
    mine = cfg.get("self_bot_id") or cfg.get("manager_bot_id") or cfg.get("bot_id")
    if bot and tok and bot == mine:
        return {"X-AM-Bot-Id": bot, "X-AM-Bot-Token": tok}
    return {}


def manager_bot_id(cfg: dict) -> str:
    # `manager_bot_id` 是正式欄位；`bot_id` 是早期部署寫的名字，一起吃掉。
    for key in ("manager_bot_id", "bot_id"):
        v = cfg.get(key)
        if isinstance(v, str) and v:
            return v
    raise AgmError("no_manager_bot", "runtime.json 沒有 manager_bot_id", 2)


# ------------------------------------------------------------------ HTTP 客戶端


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """一律不跟 redirect。跟了就可能把 X-AM-Token 送到別的 host 去。"""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D102
        raise AgmError("redirect_refused", f"daemon 回了 {code} 轉址到別處；不跟隨以免帶著 token 出去", 4)


class Client:
    def __init__(self, base: str, timeout: float = DEFAULT_TIMEOUT, extra_headers: dict | None = None) -> None:
        # 再擋一次 loopback：Client 可能被別的路徑（測試、之後的呼叫端）直接建出來。
        self.base = check_loopback(base).rstrip("/")
        self.timeout = timeout
        self._token: str | None = None
        # 角色身分（見 bot_auth_headers）。只跟著需要驗證的 API 請求走，不送去 /api/session。
        self._extra = dict(extra_headers or {})
        # ProxyHandler({}) 是關鍵：urllib 預設吃 HTTP_PROXY/ALL_PROXY，那會把帶著
        # token 的請求整包送去代理伺服器。本機 daemon 不需要任何 proxy。
        self._opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def token(self) -> str:
        """執行期取 token。不快取到磁碟、不印出來。"""
        if self._token is None:
            body = self._raw("GET", "/api/session", None, auth=False)
            tok = ""
            if isinstance(body, dict) and isinstance(body.get("token"), str):
                tok = body["token"]
            elif isinstance(body, str):
                tok = body.strip()
            if not tok:
                raise AgmError("no_token", "GET /api/session 沒有回 token", 3)
            self._token = tok
        return self._token

    def _raw(self, method: str, path: str, body: object, auth: bool = True) -> object:
        url = self.base + path
        data = None
        headers = {"Accept": "application/json"}
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if auth:
            headers["X-AM-Token"] = self.token()
            # daemon 把寫入型請求的呼叫端記進 log／刪除 intent（issue #406）：自報是 agm，事後查得出來。
            headers["X-AM-Caller"] = "agm"
            headers.update(self._extra)
        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with self._opener.open(req, timeout=self.timeout) as res:
                return _parse(res.read())
        except urllib.error.HTTPError as e:
            # HTTPError 本身是個 response，讀完要關掉；不關的話連線會留著等 GC。
            with e:
                payload = _parse(e.read())
            detail = payload if isinstance(payload, (dict, list)) else str(payload or "")
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
            # 回應讀到一半連線被掐斷：請求已經送出去了，對 mutation 是「送達未知」（#333）。
            raise AgmError("connection_lost", f"{method} {path} 送出後連線中斷：{e}", 5)
        except urllib.error.URLError as e:
            # `reason` 可能是 socket.timeout 包起來的。
            if isinstance(e.reason, socket.timeout):
                raise AgmError("timeout", f"{method} {path} 逾時（{self.timeout:g}s）", 5)
            # 連線被拒／名字解不開：請求根本沒送出去，才是「連不上」。reset／對端沒回就關掉是送出後才斷（#333）。
            if isinstance(e.reason, (ConnectionError, http.client.HTTPException)) and not isinstance(e.reason, ConnectionRefusedError):
                raise AgmError("connection_lost", f"{method} {path} 送出後連線中斷：{e.reason}", 5)
            raise AgmError("connect_failed", f"連不上 daemon：{e.reason}", 6)

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


def optional_get(client: Client, path: str, query: dict | None = None) -> object | None:
    """端點還沒部署（404）時回 None，讓呼叫端退回舊介面而不是整支掛掉。"""
    try:
        return client.get(path, query)
    except AgmError as e:
        if e.kind == "http_error" and e.extra.get("status") == 404:
            return None
        raise


# ------------------------------------------------------------------- 輸出精簡


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
        # 設定值；實際跑起來用的在 run.runtime_model / runtime_effort。
        "model": _s(b.get("model")),
        "effort": _s(b.get("effort")),
        "managed_by": _s(b.get("managed_by")),
        "parent_bot_id": _s(b.get("parent_bot_id")),
        "cwd": _s(b.get("cwd")),
        # 側欄那顆複合燈號：有 daemon 算好的就照抄，沒有就照它同一套規則算（見 `_lamp`）。
        "lamp": _lamp(b, run),
        "is_manager": bool(manager_id) and bid == manager_id,
    }
    # 排隊中的 web prompt：`/api/state` 的 `queued_turn` 是一筆 turn（不是數字），
    # prompt_text 不外傳。型別不混——`queued_turn` 永遠是 turn 或 null，筆數另外放 `queued_turns`。
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
            # 燈號＝使用者在側欄看到的那顆：忙碌 / 等輸入 / 閒置。
            "agent_status": _s(run.get("agent_status") or run.get("status")),
            "runtime_model": _s(run.get("runtime_model")),
            "runtime_effort": _s(run.get("runtime_effort")),
            # 有 native_session_id 才談得上「恢復原 session」。
            "native_session_id": _s(run.get("native_session_id")),
            "pane_id": _s(run.get("pane_id")),
            "turn_error": _s(run.get("turn_error")),
            "started_at": _s(run.get("started_at")),
        }
    else:
        row["run"] = None
    # §6.11 被收起來省 RAM 的那幾顆：`run` 是 null 但叫得醒。少了這個欄位，睡著的跟停掉的、
    # 掛掉的在 `agm state` 裡長得一模一樣（issue #514）。
    asleep = b.get("asleep")
    row["asleep"] = (
        {"since": _s(asleep.get("since")), "idle_minutes": asleep.get("idle_minutes")}
        if isinstance(asleep, dict)
        else None
    )
    # 主機連不連得上。`None` = 這份 state 沒講（`/api/state` 就沒有這個欄位）。
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
    return {
        "id": _s(m.get("id")),
        "bot_id": _s(m.get("bot_id")),
        "bot_name": _s(m.get("bot_name")),
        "project_id": _s(m.get("project_id")),
        "project_label": _s(m.get("project_label")),
        "bot_deleted": bool(m.get("bot_deleted")),
        "turn_id": _s(m.get("turn_id")),
        "role": _s(m.get("role")),
        # `source` 與 `incomplete` 決定這段文字能不能當證據：終端擷取來的可能被裁掉，
        # 少了這兩個欄位，截斷的片段會被當成完整回覆。
        "source": _s(m.get("source")),
        "incomplete": bool(m.get("incomplete")),
        "truncated": bool(m.get("truncated")),
        "relay_from": _s(m.get("relay_from")),
        "content": _s(m.get("content") or m.get("text")),
        "created_at": _s(m.get("created_at") or m.get("ts")),
    }


# -------------------------------------------------------------------- 子命令


def cmd_state(client: Client, cfg: dict, args) -> object:
    # 有 supervisor 專用的 sanitized state 就用它；舊 daemon 退回 /api/state。
    # 兩條路都過 slim_state：`/api/state` 帶著 env 與 args，原樣印出來等於把秘密
    # 倒進總管的對話紀錄裡，所以沒有「不精簡」這個選項。
    manager = ""
    try:
        manager = manager_bot_id(cfg)
    except AgmError:
        pass  # 還沒部署完的 runtime.json 也要能看狀態。
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
    # 舊介面只有 bot_id + hits + snippet，沒有 message ID：明講清楚，別讓模型當成證據 ID。
    hits = client.get("/api/search/messages", {"q": args.query})
    return {
        "source": "legacy_search",
        "query": args.query,
        "note": "舊搜尋介面只有片段，沒有 message/turn ID；要引用證據請再用 `agm messages <bot-id>` 讀原文",
        "hits": hits,
    }


def cmd_messages(client: Client, cfg: dict, args) -> object:
    page = client.get(
        f"/api/bots/{urllib.parse.quote(args.bot_id)}/messages",
        {"limit": args.limit, "before": args.before},
    )
    if not isinstance(page, dict):
        return page
    msgs = [slim_message(m) for m in page.get("messages") or []]
    has_more = bool(page.get("has_more"))
    # daemon 不回 cursor。它給的是「最後 limit 則、正序」，所以要再往前翻就拿第一則的
    # id 當 `before`（端點的條件是 `id < before`）。沒有更早的就不要給游標。
    next_cursor = msgs[0]["id"] if has_more and msgs else None
    return {
        "bot_id": args.bot_id,
        "messages": msgs,
        "has_more": has_more,
        "next_cursor": next_cursor,
        # turns 用來判斷某一則交辦到底跑完了沒——只靠訊息看不出回合是否終結。
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


def _assign_text(args) -> str:
    if args.text_file:
        try:
            text = Path(args.text_file).expanduser().read_text(encoding="utf-8")
        except OSError as e:
            raise AgmError("bad_args", f"讀不到 --text-file：{e.strerror}", 2)
        except UnicodeDecodeError:
            raise AgmError("bad_args", "--text-file 不是 UTF-8 文字", 2)
    else:
        text = args.text or ""
    text = text.strip()
    if not text:
        raise AgmError("bad_args", "交辦內容不可為空", 2)
    _check_length(text)
    return text


def cmd_assign(client: Client, cfg: dict, args) -> object:
    body = {
        "target_bot_id": args.bot,
        "text": _assign_text(args),
        "client_request_id": args.request_id,
    }
    if args.source_turn_id:
        body["source_turn_id"] = args.source_turn_id
    # 交辦時就把檔案／模組範圍記下來，daemon 才有東西可以比對重疊；它只會「回報」衝突，
    # 不會替你決定，真正的協調還是你的事。
    if args.owns:
        body["ownership"] = list(args.owns)
    # 回報給哪個角色驗收。不寫就是「誰派的誰驗」（daemon 依 bot token 認角色）；巡檢的例行
    # 維運（gc、健康追查）寫 patrol，其餘交給協調者。
    if getattr(args, "review_by", None):
        body["review_role"] = args.review_by
    # 通知：只是把話說給 bot 聽（「收到」「進 idle」「看完即可」），送到就結案。
    # 不加這個旗標的一律照舊：回合結束停在 awaiting_review 等你驗收。
    if getattr(args, "notice", False):
        body["kind"] = "notice"
        body["expects_review"] = False
    # 交接給另一個 AGM 角色時明講「這是回覆」：沒帶就當新的事、叫醒對方（SPEC §18.15）。
    if getattr(args, "ack", False):
        body["ack"] = True
    if getattr(args, "reply_to", None):
        body["reply_to"] = args.reply_to
    # 群組任務：這件交辦屬於哪個任務、擔任哪個角色。兩個一起給——少一個 daemon 會回 400，
    # 在這裡先擋，免得一件沒掛上任務的工作被派出去。
    mission, role = getattr(args, "mission", None), getattr(args, "role", None)
    if bool(mission) != bool(role):
        raise AgmError("bad_args", "--mission 與 --role 要一起給（role：executor / reviewer / verifier）", 2)
    if mission:
        body["mission_id"] = mission
        body["role"] = role
    try:
        return client.post("/api/supervisor/assignments", body)
    except AgmError as e:
        if e.kind in ("timeout", "connection_lost"):
            # 送達未知。**不要**換一個 request id 重試——那會派出第二份同樣的工。
            raise AgmError(
                "delivery_unknown",
                "交辦請求逾時，送達狀態未知。請用同一個 client_request_id 先 `agm assignments` 對帳，"
                "確認沒有這筆才用**同一個** ID 重送；不要換新 ID。",
                7,
                client_request_id=args.request_id,
                target_bot_id=args.bot,
            )
        raise


# `quota_blocked`：帳號撞到用量上限，daemon 會在額度回來後自己重送——工作還沒完，所以算 open。
OPEN_STATUSES = ("queued", "delivered", "unknown", "awaiting_review", "blocked", "quota_blocked")
# 一次跟 daemon 要幾筆，以及最多翻幾頁（避免壞掉的游標把這裡變成無窮迴圈）。
ASSIGNMENT_PAGE = 200
MAX_ASSIGNMENT_PAGES = 50


def _all_assignments(client: Client) -> tuple[list, bool]:
    """翻完整份交辦清單。回 `(items, complete)`。

    過濾（`--open`／`--status`／`--id` 的退路）一定要對**全部**做：daemon 的清單預設只回最新
    一頁，而卡最久的那幾筆天生活得比一頁久——`blocked` 的定義就是「還在等，保持未結案」
    （issue #515：正式機上 6 筆未結案有 3 筆在頁外，`--open` 一筆都看不到它們）。

    `complete=False` = 沒撈完，而且**不知道漏了什麼**：舊 daemon 不回 `has_more`（沒有分頁，
    只有那一頁）、或翻頁翻到上限。呼叫端要把這件事講出來，不要把半份清單當成全部。
    """
    items: list = []
    cursor = None
    for _ in range(MAX_ASSIGNMENT_PAGES):
        page = client.get("/api/supervisor/assignments", {"before": cursor, "limit": ASSIGNMENT_PAGE})
        if not isinstance(page, dict):
            return items, False
        items.extend(a for a in page.get("assignments") or [] if isinstance(a, dict))
        if "has_more" not in page:
            # 舊 daemon：沒有游標可以翻，拿到的就只有最新那一頁。
            return items, False
        if not page.get("has_more"):
            return items, True
        cursor = page.get("next_cursor")
        if not isinstance(cursor, str) or not cursor:
            return items, False
    return items, False


def cmd_assignments(client: Client, cfg: dict, args) -> object:
    # 有過濾條件就一定要翻完：只看第一頁的過濾結果會把頁外的未結案講成「沒有」。
    filtered = bool(args.id or args.status or args.open or args.awaiting_review)
    if filtered or args.all:
        items, complete = _all_assignments(client)
    else:
        out = client.get("/api/supervisor/assignments")
        if not isinstance(out, dict):
            return out
        items = [a for a in out.get("assignments") or [] if isinstance(a, dict)]
        # 舊 daemon 沒有 `has_more`：那就是「不知道還有沒有」，不是「沒有了」。
        complete = "has_more" in out and not out.get("has_more")
    if args.id:
        # 單筆優先走 `/assignments/{id}`（有 review 歷程）；那支只吃 assignment id，
        # 拿 client_request_id 來查一定 404，所以退路掃的是**全量**清單。
        one = optional_get(client, f"/api/supervisor/assignments/{urllib.parse.quote(args.id)}")
        if one is not None:
            return one
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
        items = [a for a in items if a.get("status") == args.status]
    # 未結案 = 還在跑 + 等驗收 + 被標阻塞。回合跑完不等於結案，所以 awaiting_review 也算。
    if args.open:
        items = [a for a in items if a.get("status") in OPEN_STATUSES]
    if args.awaiting_review:
        items = [a for a in items if a.get("status") == "awaiting_review"]
    # 一直在重試的那種（bot 停了、在忙、要重登）最容易被看漏：它 updated_at 每次都動，
    # 看起來很忙，其實從來沒送出去過。把它單獨數出來，並附上最後一個理由。
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
        # 這份清單是不是全部。False = 還有沒撈到的，上面每一個數字都只是下限。
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
        if args.followup_file:
            try:
                body["followup_text"] = Path(args.followup_file).expanduser().read_text(encoding="utf-8")
            except OSError as e:
                raise AgmError("bad_args", f"讀不到 --followup-file：{e.strerror}", 2)
        else:
            body["followup_text"] = args.followup_text
        body["followup_request_id"] = args.followup_request_id
        if args.followup_bot:
            body["followup_bot_id"] = args.followup_bot
    path = f"/api/supervisor/assignments/{urllib.parse.quote(args.assignment_id)}/review"
    try:
        return client.post(path, body)
    except AgmError as e:
        if e.kind in ("timeout", "connection_lost"):
            # 跟 assign 同一條規則：逾時是「送達未知」。決定本身是冪等的（同一個
            # decision 重送會回同一筆），followup 也綁同一個 request id，所以先對帳再重試。
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
        # `--id`：清單只回最新 100 筆，排程腳本要確認的那筆可能早就被擠出去了。
        if getattr(args, "id", None):
            return client.get("/api/supervisor/approvals", {"id": args.id})
        return client.get("/api/supervisor/approvals")
    if args.op == "request":
        if not (args.requester and args.purpose and args.scope):
            raise AgmError("bad_args", "approval request 需要 --requester、--purpose、--scope", 2)
        body: dict = {"requester": args.requester, "purpose": args.purpose, "scope": args.scope}
        if args.commit:
            body["target_commit"] = args.commit
        if args.expires_in:
            body["expires_in_secs"] = args.expires_in
        # 重送同一個 request id 回原本那一筆（回應的 created=false）；換了 purpose／scope／commit 回 409。
        if args.request_id:
            body["request_id"] = args.request_id
        # 同一個申請者換 commit 重新申請：舊的那筆標 superseded，等待起點接過來（SPEC §18.10）。
        if args.supersedes:
            body["supersedes"] = args.supersedes
        # 申請理由要**真的送出去**：以前只有 `approval decide` 帶 reason，request 寫了等於沒寫，
        # AGM 看到空欄位就以「未附理由」駁回（2026-09-19，連三張）。
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
    if args.expires_in:
        body["expires_in_secs"] = args.expires_in
    return client.post(f"/api/supervisor/approvals/{urllib.parse.quote(args.approval_id)}/decide", body)


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
        # 先 open 再 fstat，不要先 stat 再 open（issue #89 修過的同一個 TOCTOU 形狀）：兩步之間
        # 路徑可以被換掉，驗過的跟讀到的就不是同一個檔；而且 os.stat 跟隨 symlink，驗到的會是
        # 目標的權限。O_NOFOLLOW 連「路徑本身是 symlink」都擋掉，只認我們自己寫的那個真檔。
        try:
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        except OSError as e:
            raise AgmError("bad_args", f"讀不到 lease token 檔 {path}：{e}", 2) from e
        with os.fdopen(fd, encoding="utf-8") as fh:
            mode = os.fstat(fh.fileno()).st_mode
            if mode & 0o077:
                raise AgmError(
                    "bad_args",
                    f"lease token 檔 {path} 的權限是 {mode & 0o777:o}，group／other 讀得到就當它已經外洩；"
                    "請 chmod 600 之後重拿一次窗口",
                    2,
                )
            raw = fh.read()
        # 多行就拒絕（i407 審核）：`.strip()` 只去頭尾空白，中間的換行會留在 token 裡整串送出去，
        # daemon 只會回「token 不符」，查的人得自己想到 `wc -l`。缺檔、空檔都給了明確訊息，這個也要給。
        lines = [ln for ln in raw.splitlines() if ln.strip()]
        if len(lines) > 1:
            raise AgmError(
                "bad_args",
                f"lease token 檔 {path} 有 {len(lines)} 行；token 是單獨一行，"
                "多半是誤把別的東西也寫進去了（送出去只會換來一句 token 不符）",
                2,
            )
        tok = lines[0].strip() if lines else ""
        if not tok:
            raise AgmError("bad_args", f"lease token 檔 {path} 是空的", 2)
        return tok
    if inline == "-":
        tok = sys.stdin.read().strip()
        if not tok:
            raise AgmError("bad_args", "--lease-token - 要從 stdin 讀，但 stdin 是空的", 2)
        return tok
    return inline or ""


def cmd_lease(client: Client, cfg: dict, args) -> object:
    """執行租約：等安全窗口用 `safety`（唯讀），真的要動手用 `acquire`。

    兩件事分開是刻意的：safety 只說「現在」，acquire 會在同一個鎖裡重驗一次再把窗口拿走，
    拿著 restart 租約期間 supervisor assignment 派送暫停；其他送信路徑仍需協調。
    """
    if args.op == "status":
        return client.get("/api/supervisor/leases")
    if args.op == "safety":
        # --approval：問「這一筆核准現在開得了窗口嗎」。升級（等太久就縮小封鎖面）的計時綁在它身上；
        # 不帶就是純查詢，看最早那筆還活著的核准。
        query = {}
        if args.exclude_bot:
            query["exclude"] = ",".join(args.exclude_bot)
        if args.approval:
            query["approval"] = args.approval
        # --owner：以這個人的身分問，他自己握的租約不算擋（SPEC §18.10「自己的租約不擋自己」）。
        # 只有明確帶了才送：不帶就是舊行為，每一把租約都算擋。
        if args.owner:
            query["owner"] = args.owner
        return client.get("/api/supervisor/maintenance/safety", query or None)
    if not args.resource:
        raise AgmError("bad_args", f"lease {args.op} 需要 resource（rebuild / restart）", 2)
    # acquire／renew／release 一定要有 owner：沒帶就用 $AM_AGENT_NAME（safety 則刻意不補，見上）。
    owner = args.owner or os.environ.get("AM_AGENT_NAME", "agm-ops")
    path = f"/api/supervisor/leases/{urllib.parse.quote(args.resource)}/{args.op}"
    if args.op == "acquire":
        if not args.approval:
            raise AgmError("bad_args", "lease acquire 需要 --approval（核准 id）", 2)
        body: dict = {"owner": owner, "approval_id": args.approval, "require_idle": not args.allow_busy}
        if args.commit:
            body["commit"] = args.commit
        if args.ttl:
            body["ttl_secs"] = args.ttl
        if args.exclude_bot:
            body["exclude_bot_ids"] = list(args.exclude_bot)
        return client.post(path, body)
    if args.fence is None:
        raise AgmError("bad_args", f"lease {args.op} 需要 --fence（acquire 回傳的那個）", 2)
    # renew／release 要出示 acquire 當下發的一次性憑證：owner 與 fence 是公開欄位
    # （`lease status` 就看得到），只靠它們等於誰都能把別人正在換 binary 的窗口收掉。
    body = {"owner": owner, "fence": args.fence}
    if args.ttl:
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
        # 預設不要把整段人設倒進對話；要全文才加 --full。
        if not args.full and isinstance(out, dict) and isinstance(out.get("stored"), dict):
            out["stored"] = {k: v for k, v in out["stored"].items() if k != "text"}
        return out
    if args.op == "adopt-embedded":
        body = {"actor": args.actor}
        if args.reason:
            body["reason"] = args.reason
        return client.post("/api/supervisor/persona/adopt-embedded", body)
    # set
    if args.file:
        try:
            text = Path(args.file).expanduser().read_text(encoding="utf-8")
        except OSError as e:
            raise AgmError("bad_args", f"讀不到 --file：{e.strerror}", 2)
    elif args.text:
        text = args.text
    else:
        raise AgmError("bad_args", "persona set 需要 --text 或 --file", 2)
    body = {"text": text}
    if args.expected_version is not None:
        body["expected_version"] = args.expected_version
    return client.put(base, body)


def cmd_remote(client: Client, cfg: dict, args) -> object:
    """遠端（手機）入口。

    沒有 `active`：argv 帶了 `--remote-control` 只代表要求過。`capability.status=unsupported`
    表示這台 daemon 沒有可靠的觀測來源——那是「不知道」，不是「壞了」，不要在回報裡寫成已連上。
    人工確認會記下 actor 與時間，而且會過期，AGM 重啟後也會失效。
    """
    if args.op == "show":
        out = optional_get(client, "/api/supervisor/remote")
        if out is None:
            raise AgmError("unsupported", "這台 daemon 還沒有 remote 介面（需要更新 agents-managerd）", 6)
        return out
    if args.status in ("verified", "unavailable") and not args.actor:
        raise AgmError("bad_args", f"回報 {args.status} 要有 --actor（是誰確認的）", 2)
    body: dict = {"status": args.status, "source": args.source}
    for key, val in (("actor", args.actor), ("evidence", args.evidence), ("url", args.url)):
        if val:
            body[key] = val
    return client.post("/api/supervisor/remote", body)


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
    # 預設是「工作視圖」：只有還沒 ack 的（pending + delivered），最舊的在前，所以照順序
    # ack 真的清得掉。--all 才是含 handled 的稽核視圖（最新在前）。
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
        "bot_token_present": bool(headers),
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

    daemon 認的是 `X-AM-Bot-Id` ＋ 那顆 bot 自己的 hook token，而 `bot_auth_headers` 只在
    `AM_BOT_ID` 等於這個 runtime 的 `self_bot_id` 時才帶——也就是只有在角色自己的 pane 裡才帶。
    在一般 shell（或別顆 bot 的 pane）裡跑會回 403 `role_required`，那不是壞掉，是這支本來就
    不接受沒有身分的呼叫端。
    """
    try:
        return client.post(f"/api/supervisor/inbox/{urllib.parse.quote(args.event_id)}/ack", {})
    except AgmError as e:
        if e.extra.get("status") != 403:
            raise
        detail = e.extra.get("detail")
        reason = detail.get("reason") if isinstance(detail, dict) else None
        if reason not in ("role_required", "bot_proof_mismatch"):
            raise
        # 兩個 reason 的下一步不一樣，不要收斂成同一句：`bot_proof_mismatch` 的人已經在角色
        # pane 裡了，叫他「去角色 pane 裡跑」等於白說一步。
        if reason == "role_required":
            hint = (
                "ack 只有 AGM 角色做得到：要在巡檢或協調者自己的 pane 裡跑 bin/agm，"
                "或自己帶 X-AM-Bot-Id 與那顆 bot 的 AM_HOOK_TOKEN。"
            )
        else:
            hint = (
                "標頭帶了，但 daemon 對不上那顆 bot 的 hook token（token 輪替過、"
                "或 AM_BOT_ID 與 AM_HOOK_TOKEN 不是同一顆的）：這一顆要重啟才會拿到新的環境。"
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


# ---------------------------------------------------------------- ops-sync（issue #418）

# 來源 → 安裝位置的對照表只寫在這一處；`--check` 讀的是 `--ref`（預設 origin/main）那一版。
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


LAUNCHD_PREFIX = "LaunchAgents/"
#: plist 比對**只忽略這些**，其餘全部算語意（issue #499）。
#:
#: 原本反過來寫成白名單（`Label`／`ProgramArguments`／`StartInterval`／`RunAtLoad`），理由是「launchd
#: 會自己改寫 plist」——**那個理由是錯的**：實機那八份都是純 XML 文字檔，launchd 只讀不回寫，我當初
#: 看到的「鍵順序不同」是 `PlistBuddy -c Print` 把 dict 印出來的順序，不是檔案被改過。而且這裡比的是
#: `plistlib` parse 過的 dict，順序本來就不影響。
#:
#: 白名單的代價是**沒列到的鍵被靜默忽略**：八份 plist 全都有的 `StandardOutPath`／`StandardErrorPath`
#: 就這樣不在比對範圍裡。那兩個是 log 的落點，而 `browser-gc-kick.sh` 沒有 ops-alert 的管道、失敗只留
#: log——log 路徑漂掉卻報「同步」，等於證據來源斷了還顯示綠燈。
#:
#: `EnvironmentVariables` 留在忽略清單：裡面是這台機器的 `PATH` 與 bot id，每台不同。
#: 忽略的只有**值**——鍵在不在兩邊還是要一樣，不然安裝端整份掉了 `EnvironmentVariables`
#: （job 因此少了 `PATH`）會被報成同步（issue #499，i264 review）。
PLIST_IGNORED_KEYS = ("EnvironmentVariables",)


def _launch_agents_dir() -> Path:
    """`~/Library/LaunchAgents`；`AGM_LAUNCHAGENTS_DIR` 只給測試蓋掉（不要讓測試讀到真的 plist）。"""
    override = os.environ.get("AGM_LAUNCHAGENTS_DIR", "").strip()
    return pathlib.Path(override) if override else pathlib.Path.home() / "Library" / "LaunchAgents"


def _install_path(agm_dir: Path, target: str) -> Path:
    """對照表的安裝位置：預設相對 AGM 目錄，`LaunchAgents/` 開頭的走 [`_launch_agents_dir`]（issue #487）。"""
    if target.startswith(LAUNCHD_PREFIX):
        return _launch_agents_dir() / target[len(LAUNCHD_PREFIX):]
    return agm_dir / target


def _git_bytes(repo: Path, ref: str, source: str) -> bytes:
    return subprocess.run(["git", "-C", str(repo), "show", f"{ref}:{source}"], capture_output=True, check=True).stdout


def _plist_semantics(raw: bytes) -> dict:
    """parse 過的 plist，除了 [`PLIST_IGNORED_KEYS`] 的**值**以外全部保留（issue #499）。

    被忽略的鍵仍然留下一個存在標記：整個鍵不見也是落差——安裝端把 `EnvironmentVariables` 整份掉了
    （job 於是少了 `PATH`）不該報成同步。讀不懂就回一個帶錯誤的 dict，讓結果是 drift 而不是靜靜當成相同。
    """
    try:
        d = plistlib.loads(raw)
    except Exception as e:  # noqa: BLE001 - 壞掉的 plist 要報成落差，不是炸掉整份報告
        return {"_error": f"{type(e).__name__}: {e}"}
    return {k: ("<ignored>" if k in PLIST_IGNORED_KEYS else v) for k, v in d.items()}


def ops_sync_report(repo: Path, ref: str, agm_dir: Path) -> dict:
    """唯讀比對已安裝的 ops 腳本與 repo：只跑 git 與讀檔，不改任何安裝檔。

    四種落差分開報，嚴重度不同：`drift`（安裝檔不是 repo 任何一版＝有人直接改了安裝檔）、
    `behind`（repo 有更新沒裝，附落後的 commit）、`missing`（對照表有、安裝端沒有）、
    `extra`（`bin/` 裡有、對照表沒有——沒有版控的腳本）。
    """
    manifest = _git(repo, "show", f"{ref}:{OPS_MANIFEST}")
    entries = []
    for line in manifest.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) != 2:
            raise AgmError("bad_manifest", f"{OPS_MANIFEST} 這行看不懂（要「來源 安裝位置」兩欄）：{line}", 2)
        entries.append((parts[0], parts[1]))
    report: dict = {"ref": ref, "commit": _git(repo, "rev-parse", "--short", ref), "agm_dir": str(agm_dir),
                    "ok": [], "behind": [], "drift": [], "missing": [], "extra": []}
    for source, target in entries:
        path = _install_path(agm_dir, target)
        row: dict = {"source": source, "target": target}
        if not path.is_file():
            report["missing"].append(row)
            continue
        # plist 不按 blob 比（issue #487、#499）：比的是 `plistlib` parse 過的 dict，
        # 所以同樣內容換個鍵序不算 drift。忽略的只有 `PLIST_IGNORED_KEYS`。
        if target.startswith(LAUNCHD_PREFIX):
            want = _plist_semantics(_git_bytes(repo, ref, source))
            got = _plist_semantics(path.read_bytes())
            if want == got:
                report["ok"].append(row)
            else:
                # 兩邊的鍵取聯集：少一個鍵跟改一個值同樣是落差，只看其中一邊會漏掉「安裝端整個少了
                # StandardOutPath」這種。`_error`（plist 讀不懂）也在裡面，不然報告只會說每個欄位都是
                # None，看的人查不出真正的原因是那個檔壞了。
                keys = sorted(set(want) | set(got))
                row["diff"] = {k: {"repo": want.get(k), "installed": got.get(k)} for k in keys if want.get(k) != got.get(k)}
                report["drift"].append(row)
            continue
        installed = _git(repo, "hash-object", str(path))
        if installed == _blob_at(repo, ref, source):
            report["ok"].append(row)
            continue
        found = next((c for c in _git(repo, "log", "--format=%H", ref, "--", source).split()
                      if _blob_at(repo, c, source) == installed), None)
        if found is None:
            report["drift"].append(row)
            continue
        titles = _git(repo, "log", "--format=%h %s", f"{found}..{ref}", "--", source).splitlines()
        row.update({"installed_commit": found[:8], "behind": len(titles), "commits": titles})
        report["behind"].append(row)
    # `agm` 本身由 daemon 部署（內嵌在 binary 裡），備份檔與快取不算。
    listed = {t for _, t in entries}
    # 沒有列進對照表的 `com.agm.*` job：那就是「沒有版控的排程」，正是 #487 要抓的那種。
    agents = _launch_agents_dir()
    if agents.is_dir():
        for f in sorted(agents.glob("com.agm.*.plist")):
            rel = f"{LAUNCHD_PREFIX}{f.name}"
            if rel not in listed:
                report["extra"].append({"target": rel})
    bin_dir = agm_dir / "bin"
    if bin_dir.is_dir():
        for f in sorted(bin_dir.iterdir()):
            rel = f"bin/{f.name}"
            if f.is_file() and f.name != "agm" and ".bak" not in f.name and rel not in listed:
                report["extra"].append({"target": rel})
    report["in_sync"] = not any(report[k] for k in ("behind", "drift", "missing", "extra"))
    return report


def cmd_ops_sync(client: Client, cfg: dict, args) -> object:
    repo = Path(args.repo or os.environ.get("AGM_REPO") or DEFAULT_REPO).expanduser()
    report = ops_sync_report(repo, args.ref, runtime_dir(args.runtime_dir))
    if report["in_sync"]:
        return report
    if args.alert:
        counts = "、".join(f"{k} {len(report[k])}" for k in ("drift", "behind", "missing", "extra") if report[k])
        # plist 的 drift 要講出**哪個鍵、哪一邊**：只寫檔名的話，「log 落點漂掉」跟「腳本落後兩個 commit」
        # 在通知裡長得一模一樣，收到的人得自己再跑一次才知道要不要緊（issue #499，i264 review）。
        def _name(row: dict) -> str:
            diff = row.get("diff")
            if not diff:
                return row["target"]
            bits = "；".join(f"{k}: repo={v['repo']!r} 安裝={v['installed']!r}" for k, v in diff.items())
            return f"{row['target']}（{bits}）"

        names = ", ".join(_name(r) for k in ("drift", "behind", "missing", "extra") for r in report[k])
        detail = f"已安裝的 ops 腳本跟 {args.ref}（{report['commit']}）不一致：{counts}（{names}）。`agm ops-sync --check` 看明細，照 scripts/ops/README.md 重新 install"
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
    if args.op == "dispatched":
        if not args.kind or not args.version:
            raise AgmError("bad_args", "dispatched 要 --kind 與至少一個 --version", 2)
        return client.post("/api/release-triage/dispatched", {"kind": args.kind, "versions": args.version})
    body = {}
    if args.kind:
        body["kind"] = args.kind
    if args.version:
        body["version"] = args.version[0]
    # --dry-run：打開 [release_triage] publish 之前先看會開哪幾張（只讀 gh，不開 issue、不寫帳本）。
    if getattr(args, "dry_run", False):
        body["dry_run"] = True
    return client.post("/api/release-triage/publish", body)


def cmd_handoff(client: Client, cfg: dict, args) -> object:
    if args.summary is None and args.summary_file is None:
        return client.get("/api/supervisor/handoff")
    if args.summary_file:
        try:
            summary = Path(args.summary_file).expanduser().read_text(encoding="utf-8")
        except OSError as e:
            raise AgmError("bad_args", f"讀不到 --summary-file：{e.strerror}", 2)
    else:
        summary = args.summary or ""
    return client.put("/api/supervisor/handoff", {"summary": summary})


def cmd_mission(client: Client, cfg: dict, args) -> object:
    """群組任務（docs/API.md「群組任務」）。每個 op 對一個端點，不在 CLI 裡另外拼流程。"""
    op = args.op
    if args.as_user and (op != "answer" or args.reply_to or args.as_daemon):
        raise AgmError("bad_args", "--as-user 僅用於依使用者明確指示回答暫停任務，不可搭配 --reply-to/--as-daemon", 2)
    if op == "list":
        if not args.project:
            raise AgmError("bad_args", "mission list 需要 --project", 2)
        return client.get(
            f"/api/projects/{urllib.parse.quote(args.project)}/missions",
            {"status": args.status, "limit": args.limit},
        )
    if not args.mission_id:
        raise AgmError("bad_args", f"mission {op} 需要 mission id", 2)
    base = f"/api/missions/{urllib.parse.quote(args.mission_id)}"
    if op == "get":
        return client.get(base)
    if op == "events":
        out = client.get(base)
        return {"mission_id": args.mission_id, "events": out.get("events", []) if isinstance(out, dict) else []}
    if op == "event":
        if not args.kind or not (args.text or args.text_file):
            raise AgmError("bad_args", "mission event 需要 --kind（report / note / verified）與 --text 或 --text-file", 2)
        body: dict = {"kind": args.kind, "text": _mission_text(args)}
        # verified 要說驗的是哪個 commit：交付時 daemon 只放行 HEAD 等於它的工作樹（review3 c1 M9）。
        if args.kind == "verified" and not (args.worktree or args.sha):
            raise AgmError("bad_args", "mission event --kind verified 需要 --worktree（驗過的工作樹）或 --sha（驗過的 commit）", 2)
        if args.worktree:
            body["worktree"] = str(Path(args.worktree).expanduser())
        if args.sha:
            body["sha"] = args.sha
        _with_relay(body, cfg, args)
        return client.post(f"{base}/events", body)
    if op == "pause":
        if not args.reason:
            raise AgmError("bad_args", "mission pause 需要 --reason", 2)
        body = {"reason": args.reason}
        if args.detail:
            body["detail"] = args.detail
        return client.post(f"{base}/pause", body)
    if op in ("resume", "cancel", "round"):
        return client.post(f"{base}/{op}", {})
    if op in ("question", "answer", "revise"):
        # 三個都要冪等鍵：重送回同一筆，不會變成第二個問題／第二輪續作。
        if not (args.text or args.text_file):
            raise AgmError("bad_args", f"mission {op} 需要 --text 或 --text-file", 2)
        if not args.request_id:
            raise AgmError("bad_args", f"mission {op} 需要 --request-id（穩定的冪等鍵，重送沿用同一個）", 2)
        body = {"text": _mission_text(args), "client_request_id": args.request_id}
        if op == "answer" and args.reply_to:
            body["reply_to"] = args.reply_to
        # Never silently turn the manager into the user. A user-authorized paused
        # answer must explicitly select --as-user; normal bot replies need --reply-to.
        if not args.as_user:
            _with_relay(body, cfg, args)
        return client.post(f"{base}/{op}", body)
    if op == "complete":
        if not (args.text or args.text_file):
            raise AgmError("bad_args", "mission complete 需要 --text 或 --text-file（結果摘要）", 2)
        body = {"result_summary": _mission_text(args)}
        # 沒交付就結案要明講理由（issue #74）；no_changes 派過執行者時 daemon 會要 --worktree 來查。
        if args.no_delivery:
            body["no_delivery"] = args.no_delivery
        if args.worktree:
            body["worktree"] = str(Path(args.worktree).expanduser())
        _with_relay(body, cfg, args)
        return client.post(f"{base}/complete", body)
    if op == "pick":
        if not args.role:
            raise AgmError("bad_args", "mission pick 需要 --role（executor / reviewer / verifier）", 2)
        return client.get(f"{base}/pick", {"role": args.role, "exclude": args.exclude})
    if op == "deliver":
        if not args.worktree:
            raise AgmError("bad_args", "mission deliver 需要 --worktree（本機絕對路徑）", 2)
        body = {"worktree": str(Path(args.worktree).expanduser())}
        for key, val in (("title", args.title), ("body", args.body)):
            if val:
                body[key] = val
        _with_relay(body, cfg, args)
        return client.post(f"{base}/deliver", body)
    raise AgmError("bad_args", f"未知的 mission op：{op}", 2)


def _mission_text(args) -> str:
    if args.text_file:
        try:
            text = Path(args.text_file).expanduser().read_text(encoding="utf-8")
        except OSError as e:
            raise AgmError("bad_args", f"讀不到 --text-file：{e.strerror}", 2)
        except UnicodeDecodeError:
            raise AgmError("bad_args", "--text-file 不是 UTF-8 文字", 2)
    else:
        text = args.text or ""
    if not text.strip():
        raise AgmError("bad_args", "內容是空的", 2)
    _check_length(text)
    return text


def _with_relay(body: dict, cfg: dict, args) -> None:
    """回報進群組時間軸的話要標來源。預設是總管自己（runtime.json 的 manager_bot_id），
    `--as-daemon` 標成 daemon。沒有 bot id 就不帶——daemon 會當成使用者本人，這裡寧可報錯。"""
    if getattr(args, "as_daemon", False):
        body["relay_from"] = "daemon"
        return
    # 以這個 runtime 自己的 bot 發言：協調者的回報不能掛在巡檢名下。
    manager = cfg.get("self_bot_id") or cfg.get("manager_bot_id") or cfg.get("bot_id")
    if not manager:
        raise AgmError("bad_args", "runtime.json 沒有 manager_bot_id，無法標示這則回報是誰說的", 2)
    body["relay_from"] = manager


# 強制 `/usage` 探測：daemon 要先等同一台的 probe_lock（背景輪詢一個一個探身分時會佔著），再自己探最久 40 秒。
QUOTA_PROBE_TIMEOUT = 300.0


def cmd_quota(client: Client, cfg: dict, args) -> object:
    if not getattr(args, "probe", False):
        if args.account or args.host:
            raise AgmError("bad_args", "--account／--host 只配 --probe 用", 2)
        return client.get("/api/quota")
    # 蓋掉 statusLine 鎖住的錯值（#404）：結果 source=claude-usage，直接寫進 cache。
    query = {k: v for k, v in (("kind", args.kind), ("account", args.account), ("host", args.host)) if v}
    if client.timeout == DEFAULT_TIMEOUT:
        client.timeout = QUOTA_PROBE_TIMEOUT
    return client.post(f"/api/quota/probe?{urllib.parse.urlencode(query)}")


# ------------------------------------------------------ issue 認領（#425，不經 daemon）


def gh(argv: list[str], *, allow_fail: bool = False) -> str:
    """跑一次 `gh`，回 stdout。這條路不碰 daemon、不碰 token。"""
    try:
        r = subprocess.run(["gh", *argv], capture_output=True, text=True, timeout=GH_TIMEOUT)
    except FileNotFoundError:
        raise AgmError("no_gh", "PATH 上沒有 gh；issue 認領要靠它", 2)
    except subprocess.TimeoutExpired:
        raise AgmError("gh_timeout", f"gh {' '.join(argv[:2])} 超過 {GH_TIMEOUT:g} 秒沒回", 1)
    if r.returncode != 0 and not allow_fail:
        raise AgmError("gh_failed", f"gh {' '.join(argv[:2])} 失敗（rc={r.returncode}）：{r.stderr.strip()[:300]}", 1)
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
    """一則留言裡的認領／交回標記。`<!-- agm:issue-claim {...} -->`，JSON 壞掉就當沒有這個標記。"""
    m = re.search(r"<!--\s*(agm:issue-(?:claim|release))\s+(\{.*?\})\s*-->", body or "", re.S)
    if not m:
        return None
    try:
        payload = json.loads(m.group(2))
    except json.JSONDecodeError:
        return None
    return (m.group(1), payload) if isinstance(payload, dict) else None


# `gh issue view --json comments` 一次最多給這麼多則（底層是 GraphQL 的 `comments(first: 100)`）。
INLINE_COMMENT_CAP = 100


def all_comments(args, number: int, inline: list) -> list:
    """這張票的所有留言。

    `gh issue view --json comments` 只給**最舊**的 100 則，而現在的持有者由**最新**一個標記
    決定（交回的標記＝沒人認領）。討論長一點的票上，交回那一則正好是被截掉的那一端：
    `agm issue claim` 會一直看到一筆早就放掉的認領而 exit 3，沒有任何辦法繞過。
    沒滿 100 則就直接用剛剛那一份（一次呼叫就夠）；滿了才用 REST 翻完。
    """
    if len(inline) < INLINE_COMMENT_CAP:
        return inline
    repo = getattr(args, "repo", None)
    # `gh api` 不吃 `-R`；沒指定 repo 時用 gh 自己的 `{owner}`／`{repo}` 佔位符從 cwd 推。
    path = f"repos/{repo}/issues/{number}/comments" if repo else f"repos/{{owner}}/{{repo}}/issues/{number}/comments"
    raw = gh(["api", "--paginate", f"{path}?per_page=100"])
    try:
        rows = json.loads(raw or "[]")
    except json.JSONDecodeError:
        raise AgmError("gh_failed", f"gh api {path} 回的不是 JSON：{raw.strip()[:200]}", 1)
    if not isinstance(rows, list):
        raise AgmError("gh_failed", f"gh api {path} 回的不是留言清單", 1)
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
    """目前的認領狀態：由**最新**一個標記決定（交回的標記就是「沒人認領」）。

    回 `{"bot", "child", "worktree", "branch", "at", "last_activity", "stale"}`，沒人認領回 `None`。
    「有動靜」取「認領留言的時間」與「這張票最後被動到的時間」裡比較晚的那個：認領的人還在留言、
    改 label、推 commit 關聯，都算它還活著。
    """
    latest = None
    for c in issue.get("comments") or []:
        parsed = mark_of(c.get("body") or "")
        if not parsed:
            continue
        # `gh issue view --json` 是 `createdAt`，REST（`gh api`）是 `created_at`。
        at = parse_iso(c.get("createdAt") or c.get("created_at"))
        if at is None:
            continue
        if latest is None or at > latest[0]:
            latest = (at, parsed)
    if latest is None:
        return None
    at, (kind, payload) = latest
    if kind == RELEASE_MARK:
        return None
    updated = parse_iso(issue.get("updatedAt"))
    last = max([t for t in (at, updated) if t is not None])
    now = datetime.datetime.now(datetime.timezone.utc)
    return {
        "bot": payload.get("bot"),
        "child": payload.get("child"),
        "worktree": payload.get("worktree"),
        "branch": payload.get("branch"),
        "at": at.isoformat(),
        "last_activity": last.isoformat(),
        "stale": (now - last).total_seconds() > CLAIM_STALE_SECS,
    }


def held_by_other(claim: dict | None, me: str) -> dict | None:
    """別人還按著這張票就回那筆認領；沒人、是我自己、或已經放到過期都回 None。"""
    if claim is None or claim.get("bot") == me or claim.get("stale"):
        return None
    return claim


def claim_conflict(number: int, claim: dict) -> AgmError:
    who = claim.get("bot") or "(沒寫名字的 bot)"
    where = "，".join(f"{k} {claim[k]}" for k in ("child", "worktree", "branch") if claim.get(k))
    return AgmError(
        "issue_claimed",
        f"#{number} 已經由 {who} 認領（{where or '沒寫 worktree／分支'}），最後動靜 {claim['last_activity']}；不要再派第二顆",
        3,
        issue=number,
        claimed_by=who,
        claim=claim,
    )


def cmd_issue(_client, _cfg: dict, args) -> object:
    number = args.number
    me = claiming_bot(args)
    issue = read_issue(args, number)
    # 留言滿 100 則就代表可能被截掉，而截掉的是決定持有者的那一端（見 `all_comments`）。
    issue["comments"] = all_comments(args, number, issue.get("comments") or [])
    claim = current_claim(issue)
    blocker = held_by_other(claim, me)
    if blocker is not None:
        raise claim_conflict(number, blocker)
    labels = [l.get("name") for l in issue.get("labels") or []]
    # 關掉的票沒有什麼好派的。`release` 還是放行：關票之後清掉留著的 label 是正常的收尾。
    if args.op == "claim" and (issue.get("state") or "").upper() != "OPEN":
        raise AgmError(
            "issue_closed",
            f"#{number} 已經是 {issue.get('state')}，不認領也不派工；要清掉留著的 label 用 `agm issue release {number}`",
            2,
            issue=number,
            state=issue.get("state"),
        )
    now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")

    if args.op == "release":
        out = {"issue": number, "title": issue.get("title"), "bot": me, "released": True, "was_claimed": claim is not None}
        if claim is None and CLAIM_LABEL not in labels:
            out["already"] = True
            return out
        body = f"{me} 交回 #{number}。\n\n<!-- {RELEASE_MARK} {json.dumps({'bot': me, 'at': now}, ensure_ascii=False, sort_keys=True)} -->"
        gh(["issue", "comment", str(number), *repo_args(args), "--body", body])
        if CLAIM_LABEL in labels:
            gh(["issue", "edit", str(number), *repo_args(args), "--remove-label", CLAIM_LABEL])
        return out

    # claim：同一顆 bot 重跑不再留第二則（重試不該洗版），但 label 掉了會補回去。
    if claim is not None and claim.get("bot") == me and not claim.get("stale"):
        if CLAIM_LABEL not in labels:
            gh(["issue", "edit", str(number), *repo_args(args), "--add-label", CLAIM_LABEL])
        return {"issue": number, "title": issue.get("title"), "claimed": True, "already": True, "bot": me, "claim": claim}

    payload = {"bot": me, "at": now}
    for key in ("child", "worktree", "branch"):
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
    # label 不存在時 `--add-label` 會失敗，先建一次（已存在會失敗，無妨；同 ci-watch-kick 的做法）。
    gh(["label", "create", CLAIM_LABEL, *repo_args(args), "--color", "FBCA04", "--description", "有 bot 正在做（agm issue claim）"], allow_fail=True)
    gh(["issue", "comment", str(number), *repo_args(args), "--body", body])
    gh(["issue", "edit", str(number), *repo_args(args), "--add-label", CLAIM_LABEL])
    out = {"issue": number, "title": issue.get("title"), "claimed": True, "already": False, "bot": me, "label": CLAIM_LABEL}
    out.update({k: v for k, v in payload.items() if k in ("child", "worktree", "branch")})
    if took_over:
        out["took_over_stale_claim_from"] = claim.get("bot")
    return out


def cmd_bot(client: Client, cfg: dict, args) -> object:
    if args.op == "create":
        if not args.project or not args.name:
            raise AgmError("bad_args", "bot create 需要 --project 與 --name", 2)
        body: dict = {"name": args.name}
        for key, val in (("kind", args.kind), ("model", args.model), ("effort", args.effort), ("identity", args.identity)):
            if val:
                body[key] = val
        return client.post(f"/api/projects/{urllib.parse.quote(args.project)}/bots", body)
    if not args.bot_id:
        raise AgmError("bad_args", f"bot {args.op} 需要 bot id", 2)
    if args.op == "set":
        body = {key: val for key, val in (("model", args.model), ("effort", args.effort), ("identity", args.identity)) if val is not None}
        if not body:
            raise AgmError("bad_args", "bot set 至少需要 --model、--effort 或 --identity 其中一個", 2)
        return client.patch(f"/api/bots/{urllib.parse.quote(args.bot_id)}", body)
    if args.op == "delete":
        # 軟刪：先停 pane，再從設定拿掉；它開的子 agent 一起收，對話紀錄保留。
        # AGM 的 bot（總管專案裡的、總管的 child）不帶 --confirm-supervisor 會 409 supervisor_owned（issue #406）。
        query = "?confirm=supervisor" if getattr(args, "confirm_supervisor", False) else ""
        return client.delete(f"/api/bots/{urllib.parse.quote(args.bot_id)}{query}")
    if args.op == "restore":
        # 還原被軟刪的 bot（API §10.4a；子 agent 只清 deleted_at、不開 run，pane 由父 bot 用 herdr 重開）。
        return client.post(f"/api/bots/{urllib.parse.quote(args.bot_id)}/restore", {})
    path = f"/api/bots/{urllib.parse.quote(args.bot_id)}/{args.op}"
    # `--resume native`：接回 DB 記的原生對話（接不回 daemon 回 409 resumed:false，不會默默開新的）。
    # 回應原樣印出——resumed／session_id／resume_outcome 就是呼叫端要看的。
    if getattr(args, "session", None) and not getattr(args, "resume", None):
        raise AgmError("bad_args", "--session 要跟 --resume native 一起用", 2)
    if getattr(args, "resume", None):
        if args.op not in ("start", "restart"):
            raise AgmError("bad_args", "--resume 只有 bot start / restart 收", 2)
        query = {"resume": args.resume}
        if getattr(args, "session", None):
            # 救援：DB 記錯 session 時指名接回哪一段（只有 agm 露出；網頁沒有）。
            query["session"] = args.session
        path = f"{path}?{urllib.parse.urlencode(query)}"
    return client.post(path, {})


# ------------------------------------------------------------------ 參數解析


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
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
            "  agm mission list --project <id> --status open  群組任務；pick --role verifier 照規則挑身分\n"
            "  agm issue claim 425 --child i425 --worktree … --branch …   派工前先認領（exit 3＝別人在做）\n"
            "\n"
            "注意：assign 逾時代表送達未知，**不要**換新的 --request-id 重送，先用 assignments 對帳。\n"
            "注意：回合結束不等於工作完成。交辦會停在 awaiting_review，要 `agm review` 才會結案。"
        ),
    )
    p.add_argument("--runtime-dir", help="覆寫設定目錄（預設讀 AGM_RUNTIME_DIR，再退回腳本上層目錄）")
    p.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT, help=f"HTTP 逾時秒數（預設 {DEFAULT_TIMEOUT:g}）")
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
    s.add_argument("--limit", type=int, default=20)
    s.set_defaults(func=cmd_search)

    s = sub.add_parser("messages", help="讀某個 bot 的對話（分頁）")
    s.add_argument("bot_id")
    s.add_argument("--limit", type=int, default=50)
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
    s.add_argument("--mission", help="群組任務 id：這件交辦屬於哪個任務（要和 --role 一起給）")
    s.add_argument("--role", choices=["executor", "reviewer", "verifier"], help="在任務裡擔任的角色")
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
    s.add_argument("assignment_id")
    s.add_argument(
        "--decision",
        required=True,
        choices=["accept", "block", "followup", "fail", "cancel"],
        help="accept=驗收結案／block=還在等，保持未結案／followup=派續作／fail=判定失敗／cancel=取消",
    )
    # cancel 是唯一能在回合還在跑時用的決定，但它停的是「追蹤」，不是那顆 bot：
    # daemon 不會中止回合，delivered／unknown 的 delivery 與 turn_id 都會原樣留著。
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
    s.add_argument("--requester", help="request：申請者（bot id 或名字）")
    s.add_argument("--purpose", choices=["rebuild", "restart"], help="request：要做什麼")
    s.add_argument("--scope", help="request：會動到什麼")
    s.add_argument("--commit", help="request：針對哪個 commit（之後 acquire 要對得上）")
    s.add_argument(
        "--request-id",
        dest="request_id",
        help="request：穩定 id，重送同一個回原本那一筆（回應 created=false）；換了內容回 409。不確定送出去沒有時用它重送，不要換新 id",
    )
    s.add_argument("--expires-in", type=int, dest="expires_in", metavar="SECS", help="多久之後失效")
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
        help="acquire/renew/release：誰持有（預設 $AM_AGENT_NAME）；safety：以這個人的身分問，他自己握的租約不算擋（不帶＝每一把都算擋）",
    )
    s.add_argument("--approval", help="acquire：核准 id；safety：用這筆核准的等待時間判斷要不要縮小封鎖面")
    s.add_argument("--commit", help="acquire：要處理的 commit，必須符合核准")
    s.add_argument("--ttl", type=int, metavar="SECS", help="租約長度（預設 900，上限 3600）")
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

    s = sub.add_parser("remote", help="遠端入口：show / observe（人工確認會過期）")
    s.add_argument("op", choices=["show", "observe"])
    s.add_argument("--status", choices=["requested", "verified", "unavailable", "unknown"], default="verified")
    s.add_argument("--source", choices=["manual", "provider"], default="manual", help="argv 不接受：那是 daemon 自己的紀錄")
    s.add_argument("--actor", help="誰確認的（verified / unavailable 必填）")
    s.add_argument("--evidence", help="依據（例如實際用手機連過）")
    s.add_argument("--url", help="觀測到的入口 URL（URL 本身不算證據）")
    s.set_defaults(func=cmd_remote)

    s = sub.add_parser("build-inputs", help="會影響 binary 的路徑（含 include_str! 的檔）")
    s.set_defaults(func=cmd_build_inputs)

    s = sub.add_parser("incidents", help="系統層級故障；預設只列未恢復的")
    s.add_argument("--all", action="store_true", help="含已恢復的")
    s.set_defaults(func=cmd_incidents)

    s = sub.add_parser("inbox", help="還沒 ack 的通知（最舊在前）；--all 才含已處理的")
    s.add_argument("--all", action="store_true", help="含 state=handled 的事件（最新在前）")
    s.add_argument("--role", choices=["patrol", "responder", "mine"], help="只看某個 AGM 角色收的（mine＝runtime.json 的角色）")
    s.add_argument("--limit", type=int, default=200, help="最多幾筆（預設 200，上限 1000）")
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
    s.add_argument("event_id")
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
    s.set_defaults(func=cmd_ops_sync)

    s = sub.add_parser("release-triage", help="上游新版分診：submit（交回 verdict）/ show / dispatched / publish（issue #204）")
    s.add_argument("op", choices=["submit", "show", "dispatched", "publish"])
    s.add_argument("--file", help="submit：verdicts.json（{kind,version,verdicts,issues}）")
    s.add_argument("--kind", choices=["claude", "codex"])
    s.add_argument("--version", action="append", help="show／publish：某一版；dispatched：可重複給多版")
    s.add_argument("--dry-run", action="store_true", help="publish：乾跑。檢查 gh auth／repo 權限／標籤齊不齊，印出會開哪幾張（含標題與內文），一張都不開、帳本不動")
    s.set_defaults(func=cmd_release_triage)

    s = sub.add_parser("handoff", help="讀管理摘要；帶 --summary/--summary-file 就是寫入")
    s.add_argument("--summary")
    s.add_argument("--summary-file")
    s.set_defaults(func=cmd_handoff)

    s = sub.add_parser("quota", help="各身分的額度狀態；--probe 強制重跑 /usage 並覆寫 cache")
    s.add_argument("--probe", action="store_true", help="強制重跑該帳號的 /usage，結果蓋掉 statusLine 的值")
    s.add_argument("--kind", default="claude", choices=["claude"], help="目前只有 claude 有強制探測")
    s.add_argument("--account", help="身分名（cc0、cc1…）；省略＝預設帳號")
    s.add_argument("--host", help="主機名；省略＝本機")
    s.set_defaults(func=cmd_quota)

    s = sub.add_parser(
        "mission",
        help="群組任務：list / get / events / event / pause / resume / cancel / complete / round / pick / deliver",
        description="對應 docs/API.md「群組任務」的端點。回報進群組的 event / complete / deliver 預設標成總管說的。",
    )
    s.add_argument(
        "op",
        choices=[
            "list", "get", "events", "event", "pause", "resume", "cancel", "complete", "round", "pick", "deliver",
            # 完成之後還能往下談：question 問一句（不改東西）、answer 回覆、revise 開續作。
            "question", "answer", "revise",
        ],
    )
    s.add_argument("mission_id", nargs="?", help="list 以外都需要")
    s.add_argument("--project", help="list：專案 id")
    s.add_argument("--status", choices=["all", "open", "done", "cancelled"], help="list：篩選（已完成任務＝done）")
    s.add_argument("--limit", type=int, help="list：筆數上限")
    s.add_argument("--kind", choices=["report", "note", "verified"], help="event：事件種類（verified＝驗證通過，交付前必須有）")
    s.add_argument("--text", help="event：內容／complete：結果摘要")
    s.add_argument("--text-file", dest="text_file", help="從檔案讀 --text")
    s.add_argument("--reason", help="pause：暫停原因（機器碼，例如 waiting_user）")
    s.add_argument("--detail", help="pause：補充說明")
    s.add_argument("--role", choices=["executor", "reviewer", "verifier"], help="pick：要挑哪個角色的身分")
    s.add_argument("--exclude", help="pick：排除的身分（reviewer 排除執行者的身分）")
    s.add_argument("--worktree", help="deliver：要交付的 worktree；event --kind verified：驗過的 worktree（daemon 記下它的 HEAD）；complete --no-delivery no_changes：執行者的 worktree（daemon 查它沒有改動）")
    s.add_argument(
        "--no-delivery",
        dest="no_delivery",
        choices=["no_changes", "user_declined"],
        help="complete：沒交付就結案的理由（no_changes＝沒有改東西；user_declined＝使用者回答過不要交付）",
    )
    s.add_argument("--sha", help="event --kind verified：驗過的 commit（沒有 --worktree 時必給）")
    s.add_argument("--request-id", dest="request_id", help="question/answer/revise：穩定的冪等鍵，重送沿用同一個")
    s.add_argument("--reply-to", dest="reply_to", help="answer：回的是哪一則 question 的事件 id")
    s.add_argument("--as-user", action="store_true", help="answer：依使用者明確指示代送暫停回答（不帶 relay_from）")
    s.add_argument("--title", help="deliver（pr）：PR 標題")
    s.add_argument("--body", help="deliver（pr）：PR 內文")
    s.add_argument("--as-daemon", dest="as_daemon", action="store_true", help="event/complete/deliver：來源標成 daemon 而不是總管")
    s.set_defaults(func=cmd_mission)

    s = sub.add_parser(
        "issue",
        help="issue 認領：claim / release（#425；只用 gh，不連 daemon）",
        description=(
            "派任何 issue 給 child 之前先 claim，claim 失敗（exit 3）就不要派——"
            "同一張票被兩顆父 bot 各派一顆 child 是 2026-09-23 發生三次的重工來源。收尾時 release。"
        ),
    )
    s.add_argument("op", choices=["claim", "release"])
    s.add_argument("number", type=int, help="issue 編號")
    s.add_argument("--repo", help="owner/name；省略就讓 gh 從 cwd 的 remote 自己推")
    s.add_argument("--bot", help="認領人；省略取 AM_AGENT_NAME、再退 AM_BOT_ID")
    s.add_argument("--child", help="claim：要派出去的 child agent 名")
    s.add_argument("--worktree", help="claim：child 會用的 worktree")
    s.add_argument("--branch", help="claim：child 會用的分支")
    s.set_defaults(func=cmd_issue, needs_client=False)

    s = sub.add_parser("bot", help="管理 bot：start / stop / restart / set / create / delete / restore")
    s.add_argument("op", choices=["start", "stop", "restart", "set", "create", "delete", "restore"])
    s.add_argument("bot_id", nargs="?", help="start/stop/restart/delete/restore 的目標（delete＝停 pane 並軟刪，子 agent 一起收；restore＝還原被軟刪的子 agent，不開 run）")
    s.add_argument("--project", help="create：所屬 project id")
    s.add_argument("--name", help="create：bot 名稱")
    s.add_argument("--kind", help="create：claude / codex / grok")
    s.add_argument("--model")
    s.add_argument("--effort")
    s.add_argument("--identity")
    s.add_argument("--resume", choices=["native"], help="start/restart：接回 DB 記的原生對話（?resume=native）；接不回是 409 resumed:false")
    s.add_argument("--session", help="跟 --resume native 一起：不看 DB，指名接回這一段 session id（DB 記錯時的救援路徑）")
    s.add_argument("--confirm-supervisor", dest="confirm_supervisor", action="store_true",
                   help="delete：確定要刪 AGM 的 bot（總管專案裡的、總管的 child）；沒帶會 409 supervisor_owned")
    s.set_defaults(func=cmd_bot)

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    indent = None if args.compact else 2
    try:
        # `issue` 只用 gh，不連 daemon：沒有 runtime.json 的 pane（一般的父 bot）也要能 claim。
        cfg: dict = {}
        client = None
        if getattr(args, "needs_client", True):
            cfg = load_runtime(args.runtime_dir)
            client = Client(daemon_url(cfg), args.timeout, bot_auth_headers(cfg))
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
