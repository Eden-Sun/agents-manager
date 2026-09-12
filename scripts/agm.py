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
import json
import os
import socket
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

DEFAULT_TIMEOUT = 30.0
LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost", "0:0:0:0:0:0:0:1"}
DEFAULT_RUNTIME_DIR = "~/.config/agents-manager/supervisor/AGM"


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
    def __init__(self, base: str, timeout: float = DEFAULT_TIMEOUT) -> None:
        # 再擋一次 loopback：Client 可能被別的路徑（測試、之後的呼叫端）直接建出來。
        self.base = check_loopback(base).rstrip("/")
        self.timeout = timeout
        self._token: str | None = None
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
        except urllib.error.URLError as e:
            # `reason` 可能是 socket.timeout 包起來的。
            if isinstance(e.reason, socket.timeout):
                raise AgmError("timeout", f"{method} {path} 逾時（{self.timeout:g}s）", 5)
            raise AgmError("connect_failed", f"連不上 daemon：{e.reason}", 6)

    def get(self, path: str, query: dict | None = None) -> object:
        if query:
            clean = {k: str(v) for k, v in query.items() if v is not None and v != ""}
            if clean:
                path = f"{path}?{urllib.parse.urlencode(clean)}"
        return self._raw("GET", path, None)

    def post(self, path: str, body: object = None) -> object:
        return self._raw("POST", path, body if body is not None else {})

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


def _bot_row(b: dict, project_id: str, manager_id: str) -> dict:
    """一顆 bot 的精簡投影。`run` 裡帶的是**執行期**真值（可能與設定不同）。"""
    run = b.get("run") if isinstance(b.get("run"), dict) else b.get("active_run")
    run = run if isinstance(run, dict) else {}
    team = b.get("team") if isinstance(b.get("team"), dict) else {}
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
        # `lamp` 是 daemon 已經算好的複合燈號（side bar 上看到的那顆）。自己從 run
        # 推一次只會跟畫面不一致，直接照抄。
        "lamp": _s(b.get("lamp")),
        "is_manager": bool(manager_id) and bid == manager_id,
    }
    # 排隊中的 web prompt 是 `queued_turn`（一筆 turn，不是數字）；prompt_text 不外傳。
    qt = b.get("queued_turn")
    if isinstance(qt, dict):
        row["queued_turn"] = _turn_row(qt)
    elif b.get("queued") is not None:
        row["queued_turn"] = b.get("queued")
    else:
        row["queued_turn"] = None
    if team:
        row["team"] = {
            "id": _s(team.get("id")),
            "name": _s(team.get("label") or team.get("name")),
            "role": _s(team.get("role")),
            # team 的狀態欄位叫 `phase`；`state` 是舊寫法，兩個都讀。
            "phase": _s(team.get("phase") or team.get("state")),
        }
    elif _s(b.get("team_id")):
        row["team"] = {"id": _s(b.get("team_id")), "name": "", "role": "", "phase": ""}
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
        return {"projects": [], "bots": [], "teams": []}
    projects, bots, teams = [], [], []
    seen: set[str] = set()

    def add_bot(b: object, project_id: str = "") -> None:
        if not isinstance(b, dict):
            return
        row = _bot_row(b, project_id, manager_id)
        if not row["id"] or row["id"] in seen:
            return
        seen.add(row["id"])
        bots.append(row)

    def add_team(t: object, project_id: str = "") -> None:
        if not isinstance(t, dict):
            return
        teams.append(
            {
                "id": _s(t.get("id")),
                "name": _s(t.get("name")),
                "project_id": _s(t.get("project_id")) or project_id,
                # team 的階段欄位是 `phase`。
                "phase": _s(t.get("phase") or t.get("state")),
                "pause_reason": _s(t.get("pause_reason")),
            }
        )

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
        for t in p.get("teams") or []:
            add_team(t, pid)
    for b in state.get("bots") or []:
        add_bot(b)
    for t in state.get("teams") or []:
        add_team(t)

    out: dict = {"projects": projects, "bots": bots, "teams": teams}
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


def _assign_text(args) -> str:
    if args.text_file:
        try:
            text = Path(args.text_file).expanduser().read_text(encoding="utf-8")
        except OSError as e:
            raise AgmError("bad_args", f"讀不到 --text-file：{e.strerror}", 2)
    else:
        text = args.text or ""
    text = text.strip()
    if not text:
        raise AgmError("bad_args", "交辦內容不可為空", 2)
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
    try:
        return client.post("/api/supervisor/assignments", body)
    except AgmError as e:
        if e.kind == "timeout":
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


OPEN_STATUSES = ("queued", "delivered", "unknown", "awaiting_review", "blocked")


def cmd_assignments(client: Client, cfg: dict, args) -> object:
    out = client.get("/api/supervisor/assignments")
    if not isinstance(out, dict):
        return out
    items = [a for a in out.get("assignments") or [] if isinstance(a, dict)]
    if args.id:
        # 單筆優先走 `/assignments/{id}`（有 review 歷程）；舊 daemon 沒這支就退回清單過濾。
        one = optional_get(client, f"/api/supervisor/assignments/{urllib.parse.quote(args.id)}")
        if one is not None:
            return one
        for a in items:
            if a.get("id") == args.id or a.get("client_request_id") == args.id:
                return a
        raise AgmError("not_found", f"找不到交辦 {args.id}", 4, id=args.id)
    if args.status:
        items = [a for a in items if a.get("status") == args.status]
    # 未結案 = 還在跑 + 等驗收 + 被標阻塞。回合跑完不等於結案，所以 awaiting_review 也算。
    if args.open:
        items = [a for a in items if a.get("status") in OPEN_STATUSES]
    if args.awaiting_review:
        items = [a for a in items if a.get("status") == "awaiting_review"]
    return {
        "assignments": items,
        "open": len([a for a in items if a.get("status") in OPEN_STATUSES]),
        "awaiting_review": len([a for a in items if a.get("status") == "awaiting_review"]),
    }


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
        if e.kind == "timeout":
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
    """重建／重啟核准。申請寫成紀錄（誰、範圍、哪個 commit、到期），AGM 直接核駁。"""
    if args.op == "list":
        return client.get("/api/supervisor/approvals")
    if args.op == "request":
        if not (args.requester and args.purpose and args.scope):
            raise AgmError("bad_args", "approval request 需要 --requester、--purpose、--scope", 2)
        body: dict = {"requester": args.requester, "purpose": args.purpose, "scope": args.scope}
        if args.commit:
            body["target_commit"] = args.commit
        if args.expires_in:
            body["expires_in_secs"] = args.expires_in
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


def cmd_lease(client: Client, cfg: dict, args) -> object:
    """執行租約：等安全窗口用 `safety`（唯讀），真的要動手用 `acquire`。

    兩件事分開是刻意的：safety 只說「現在」，acquire 會在同一個鎖裡重驗一次再把窗口拿走，
    拿著 restart 租約期間 daemon 不會再派新工作出去。
    """
    if args.op == "status":
        return client.get("/api/supervisor/leases")
    if args.op == "safety":
        return client.get("/api/supervisor/maintenance/safety")
    if not args.resource:
        raise AgmError("bad_args", f"lease {args.op} 需要 resource（rebuild / restart）", 2)
    path = f"/api/supervisor/leases/{urllib.parse.quote(args.resource)}/{args.op}"
    if args.op == "acquire":
        if not args.approval:
            raise AgmError("bad_args", "lease acquire 需要 --approval（核准 id）", 2)
        body: dict = {"owner": args.owner, "approval_id": args.approval, "require_idle": not args.allow_busy}
        if args.commit:
            body["commit"] = args.commit
        if args.ttl:
            body["ttl_secs"] = args.ttl
        if args.exclude_bot:
            body["exclude_bot_ids"] = list(args.exclude_bot)
        return client.post(path, body)
    if args.fence is None:
        raise AgmError("bad_args", f"lease {args.op} 需要 --fence（acquire 回傳的那個）", 2)
    body = {"owner": args.owner, "fence": args.fence}
    if args.ttl:
        body["ttl_secs"] = args.ttl
    return client.post(path, body)


def cmd_persona(client: Client, cfg: dict, args) -> object:
    """人設：持久版本是權威，內嵌版只在首次安裝當種子。

    `loaded` 不會出現 `verified`：daemon 只能在啟動 CLI 時把 persona 傳進去，看不到 session
    現在握著什麼。`needs_restart=false` 不等於新版已經生效。
    """
    if args.op == "show":
        out = optional_get(client, "/api/supervisor/persona")
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
    return client.put("/api/supervisor/persona", body)


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
    return client.get("/api/supervisor/inbox", {"all": "1" if args.all else None, "limit": args.limit})


def cmd_ack(client: Client, cfg: dict, args) -> object:
    return client.post(f"/api/supervisor/inbox/{urllib.parse.quote(args.event_id)}/ack", {})


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


def cmd_quota(client: Client, cfg: dict, args) -> object:
    return client.get("/api/quota")


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
    return client.post(f"/api/bots/{urllib.parse.quote(args.bot_id)}/{args.op}", {})


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
            "\n"
            "注意：assign 逾時代表送達未知，**不要**換新的 --request-id 重送，先用 assignments 對帳。\n"
            "注意：回合結束不等於工作完成。交辦會停在 awaiting_review，要 `agm review` 才會結案。"
        ),
    )
    p.add_argument("--runtime-dir", help="覆寫設定目錄（預設讀 AGM_RUNTIME_DIR，再退回腳本上層目錄）")
    p.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT, help=f"HTTP 逾時秒數（預設 {DEFAULT_TIMEOUT:g}）")
    p.add_argument("--compact", action="store_true", help="輸出成單行 JSON")
    sub = p.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("state", help="精簡的 bot / project / team 狀態（不含 env 等敏感欄位）")
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
    s.add_argument("--bot", required=True, help="目標 bot id")
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
    s.set_defaults(func=cmd_assign)

    s = sub.add_parser("assignments", help="列出交辦，或用 --id 查一筆")
    s.add_argument("--id", help="只查這一筆（吃 assignment id 或 client_request_id）")
    s.add_argument("--status", help="只列這個狀態")
    s.add_argument("--open", action="store_true", help="只列未結案（含 awaiting_review、blocked）")
    s.add_argument("--awaiting-review", action="store_true", dest="awaiting_review", help="只列等你驗收的")
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
    s.add_argument("--requester", help="request：申請者（bot id 或名字）")
    s.add_argument("--purpose", choices=["rebuild", "restart"], help="request：要做什麼")
    s.add_argument("--scope", help="request：會動到什麼")
    s.add_argument("--commit", help="request：針對哪個 commit（之後 acquire 要對得上）")
    s.add_argument("--expires-in", type=int, dest="expires_in", metavar="SECS", help="多久之後失效")
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
    s.add_argument("--owner", default=os.environ.get("AM_AGENT_NAME", "agm-ops"), help="誰持有（預設 $AM_AGENT_NAME）")
    s.add_argument("--approval", help="acquire：核准 id")
    s.add_argument("--commit", help="acquire：要處理的 commit，必須符合核准")
    s.add_argument("--ttl", type=int, metavar="SECS", help="租約長度（預設 900，上限 3600）")
    s.add_argument("--fence", type=int, help="renew/release：acquire 回傳的 fence")
    s.add_argument("--allow-busy", action="store_true", dest="allow_busy", help="acquire：跳過「沒人在跑」的檢查")
    s.add_argument("--exclude-bot", action="append", dest="exclude_bot", metavar="BOT_ID", help="idle 檢查要忽略的 bot")
    s.set_defaults(func=cmd_lease)

    s = sub.add_parser("persona", help="人設：show / set / adopt-embedded")
    s.add_argument("op", choices=["show", "set", "adopt-embedded"])
    s.add_argument("--full", action="store_true", help="show：連全文一起印（預設只印版本與 hash）")
    s.add_argument("--text", help="set：新的人設全文")
    s.add_argument("--file", help="set：從檔案讀")
    s.add_argument("--expected-version", type=int, dest="expected_version", help="set：樂觀鎖，對不上就拒絕")
    s.add_argument("--reason", help="adopt-embedded：為什麼要換成內嵌版")
    s.add_argument("--actor", default="AGM", help="adopt-embedded：誰決定的（預設 AGM）")
    s.set_defaults(func=cmd_persona)

    s = sub.add_parser("build-inputs", help="會影響 binary 的路徑（含 include_str! 的檔）")
    s.set_defaults(func=cmd_build_inputs)

    s = sub.add_parser("incidents", help="系統層級故障；預設只列未恢復的")
    s.add_argument("--all", action="store_true", help="含已恢復的")
    s.set_defaults(func=cmd_incidents)

    s = sub.add_parser("inbox", help="還沒 ack 的通知（最舊在前）；--all 才含已處理的")
    s.add_argument("--all", action="store_true", help="含 state=handled 的事件（最新在前）")
    s.add_argument("--limit", type=int, default=200, help="最多幾筆（預設 200，上限 1000）")
    s.set_defaults(func=cmd_inbox)

    s = sub.add_parser("ack", help="確認已處理一則通知")
    s.add_argument("event_id")
    s.set_defaults(func=cmd_ack)

    s = sub.add_parser("handoff", help="讀管理摘要；帶 --summary/--summary-file 就是寫入")
    s.add_argument("--summary")
    s.add_argument("--summary-file")
    s.set_defaults(func=cmd_handoff)

    s = sub.add_parser("quota", help="各身分的額度狀態")
    s.set_defaults(func=cmd_quota)

    s = sub.add_parser("bot", help="管理 bot：start / stop / restart / create")
    s.add_argument("op", choices=["start", "stop", "restart", "create"])
    s.add_argument("bot_id", nargs="?", help="start/stop/restart 的目標")
    s.add_argument("--project", help="create：所屬 project id")
    s.add_argument("--name", help="create：bot 名稱")
    s.add_argument("--kind", help="create：claude / codex / grok")
    s.add_argument("--model")
    s.add_argument("--effort")
    s.add_argument("--identity")
    s.set_defaults(func=cmd_bot)

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    indent = None if args.compact else 2
    try:
        cfg = load_runtime(args.runtime_dir)
        client = Client(daemon_url(cfg), args.timeout)
        out = args.func(client, cfg, args)
    except AgmError as e:
        json.dump(e.to_json(), sys.stderr, ensure_ascii=False, indent=indent)
        sys.stderr.write("\n")
        return e.exit_code
    except KeyboardInterrupt:
        return 130
    json.dump(out, sys.stdout, ensure_ascii=False, indent=indent, default=str)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
